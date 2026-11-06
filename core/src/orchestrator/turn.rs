//! One turn: conversation or the tool loop, and finishing the answer.

use super::*;

/// Model round-trips in one request when `max_rounds` is not set.
///
/// It was 4, tuned on llama3.2:1b, whose long turns were loops: the same
/// broken call again and again. A capable model's long turns are research:
/// nemotron-3-ultra used all 4 rounds searching with better queries after
/// the first page was the wrong one, and was cut off mid-answer. Loops are
/// now stopped by noticing the repeat (see `call_key`), so this limit only
/// has to stop a turn that never settles.
pub(super) const DEFAULT_MAX_ROUNDS: usize = 8;

/// The identity of a call within a turn: the same tool with the same
/// arguments. Running it again cannot produce anything new, and for a write
/// it would do the same thing twice.
pub(super) fn call_key(name: &str, args: &Value) -> String {
    format!("{}\u{0}{}", name, args)
}

/// The answer to "undo that": undo is the owner's command, never the
/// model's (an injected page could otherwise ask to undo the owner's fix).
pub(super) const UNDO_FROM_CHAT: &str = "I can't undo from chat - run `jarvis undo` (or /undo in the TUI) to see and reverse Jarvis's last file change.";

/// The last request of a turn that ran out of rounds.
pub(super) const CLOSING_REQUEST: &str = "No more tools can be used for this request. Answer the question above now, using only the tool results in this conversation. If they do not fully answer it, say what you found and what is still unknown. Do not call a tool.";

/// What the model is told instead of a result when it repeats a call.
pub(super) const REPEATED_CALL: &str =
    "Not run again: this exact call was already made in this request, and its result is above. Answer from it, or try something different.";

/// Record a tool call the orchestrator made and its result, in the shape the
/// provider expects from a model-initiated call.
pub(super) fn push_synthetic_exchange(
    messages: &mut Vec<ChatMessage>,
    outcome: &ToolOutcome,
    seen: &str,
) {
    messages.push(ChatMessage::assistant_with_tool_calls(
        "",
        vec![ModelToolCall {
            id: Some(Uuid::new_v4().to_string()),
            function: ModelFunction {
                index: None,
                name: outcome.name.clone(),
                arguments: outcome.args.clone(),
            },
        }],
    ));
    messages.push(
        ChatMessage::tool_result(&outcome.name, seen)
            .with_label(Label::of_tool(&outcome.name, &outcome.args)),
    );
}

/// The first absolute URL in a tool's output.
///
/// Used to read the top search hit. Results are rendered one URL per line, so
/// whitespace ends the URL. A trailing `)` is removed only when it has no
/// matching `(` inside the URL: that trims markdown `[text](url)`, while
/// keeping real addresses like `.../Rust_(programming_language)`.
pub(super) fn first_url(text: &str) -> Option<String> {
    let start = text.find("http://").or_else(|| text.find("https://"))?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, ']' | '>' | '"' | '\''))
        .unwrap_or(rest.len());
    let mut url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
    while url.ends_with(')') && url.matches(')').count() > url.matches('(').count() {
        url = &url[..url.len() - 1];
    }
    if url.len() > "https://".len() {
        Some(url.to_string())
    } else {
        None
    }
}

/// Does the request already contain the address it wants read?
///
/// When it does, searching first would be busywork: the user said which page
/// to read, and `fetch` is the whole job.
pub(super) fn input_names_url(input: &str) -> bool {
    let lower = input.to_lowercase();
    lower.contains("http://") || lower.contains("https://")
}

/// Clean up a model response before it becomes the user-facing answer.
pub(super) fn final_answer(response: &str) -> String {
    let cleaned = parser::strip_tool_artifacts(response);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        response.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

impl Orchestrator {
    /// Plain conversation. The model is given no tools, so it cannot try to
    /// use one - which is exactly what we want for "who are you?".
    pub(super) async fn converse(
        &self,
        input: &str,
        history: &[Message],
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder,
    ) -> Result<String> {
        let (system_prompt, recall_label) =
            self.with_recall(input, self.conversation_prompt_for(history));
        let mut plan = context::assemble(&system_prompt, input, history, &self.config.context, 0);
        self.mark_history(&mut plan.messages);
        self.log_plan(&plan);
        let mut system_label = plan.system_label.clone();
        system_label.absorb(&recall_label);
        let reply = self
            .ask_model(
                &plan.system,
                &system_label,
                &plan.messages,
                None,
                confirm,
                cancel,
                sink,
                rec,
            )
            .await;
        let turn = match reply {
            Ok(ModelReply::Turn(turn)) => turn,
            Ok(ModelReply::Withheld(answer)) => return Ok(answer),
            // A provider failure has a deterministic fallback; a user
            // cancelling the turn must not turn into one.
            Err(JarvisError::Cancelled) => return Err(JarvisError::Cancelled),
            Err(e) => {
                info!("Model unavailable: {}, using fallback", e);
                rec.model_failed(&e);
                return Ok(self.fallback_answer(&e));
            }
        };
        rec.model_turn(&turn);
        info!(
            "model turn: prompt_tokens={:?} completion_tokens={:?} content={:?}",
            turn.prompt_tokens, turn.completion_tokens, turn.message.content
        );

        let answer = final_answer(&turn.message.content);
        Ok(if answer.is_empty() {
            self.empty_answer(input)
        } else {
            answer
        })
    }

    /// The ambiguous case: the model may call a tool, and the result is
    /// rendered deterministically.
    pub(super) async fn model_with_tools(
        &self,
        input: &str,
        history: &[Message],
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        rec: &TurnRecorder,
    ) -> Result<String> {
        // The offered tools are the request's tools. There is no second
        // filter here, and no way for the prompt to mention anything else.
        let tools = plan.schemas();
        let (system_prompt, recall_label) =
            self.with_recall(input, self.tool_system_prompt(&plan.tools));
        let reserved = self.tool_schema_tokens(&tools);
        let assembled = context::assemble(
            &system_prompt,
            input,
            history,
            &self.config.context,
            reserved,
        );
        self.log_plan(&assembled);

        let system = assembled.system;
        let mut system_label = assembled.system_label;
        system_label.absorb(&recall_label);
        let mut messages = assembled.messages;
        self.mark_history(&mut messages);
        // How many leading messages are history. Everything after them is the
        // current request and the tool trace, which must never be dropped.
        let mut history_len = messages.len().saturating_sub(1);
        let mut trace: Vec<ToolOutcome> = Vec::new();

        // A web question with no URL starts with a search, decided here rather
        // than by the model. Measured on llama3.2:1b with `search` and `fetch`
        // both offered: it calls `fetch` with the *question* as the URL, gets a
        // contract error, and then answers from its weights instead of
        // retrying. Choosing the first step is the one part of the chain a 1B
        // model does not get right, and it is also the part that needs no
        // judgement: a question about the world begins by finding pages.
        //
        // The step after it - reading the best page - is the same kind of
        // decision, so it is made here too. The model is left with the part it
        // is actually good at: putting the retrieved text into an answer.
        if plan.web && !input_names_url(input) {
            let outcomes = self
                .web_first_steps(input, plan, confirm, &mut messages, rec)
                .await;
            let search_blocked = outcomes.first().is_some_and(|o| o.status.is_blocked());
            trace.extend(outcomes);
            if search_blocked {
                return Ok(render_blocked_report(&trace));
            }
            if cancel.is_some_and(Cancellation::is_cancelled) {
                return self.cancelled_result(&trace);
            }
        }

        let max_rounds = self
            .tier()
            .0
            .max_rounds
            .unwrap_or(DEFAULT_MAX_ROUNDS)
            .max(1);
        let mut seen: std::collections::HashSet<String> =
            trace.iter().map(|o| call_key(&o.name, &o.args)).collect();
        for _ in 0..max_rounds {
            if cancel.is_some_and(Cancellation::is_cancelled) {
                return self.cancelled_result(&trace);
            }

            // Tool results grow the prompt. Shed the oldest history until it
            // fits again rather than letting the provider truncate the front.
            self.fit_prompt(&system, &mut messages, &mut history_len, reserved);

            let reply = self
                .ask_model(
                    &system,
                    &system_label,
                    &messages,
                    Some(&tools),
                    confirm,
                    cancel,
                    None,
                    rec,
                )
                .await;
            let turn = match reply {
                Ok(ModelReply::Turn(t)) => t,
                Ok(ModelReply::Withheld(answer)) => return Ok(answer),
                Err(JarvisError::Cancelled) => return self.cancelled_result(&trace),
                Err(e) => {
                    info!("Model unavailable: {}, using fallback", e);
                    rec.model_failed(&e);
                    return Ok(self.fallback_answer(&e));
                }
            };
            rec.model_turn(&turn);
            info!(
                "model turn: prompt_tokens={:?} completion_tokens={:?} content={:?} tool_calls={}",
                turn.prompt_tokens,
                turn.completion_tokens,
                turn.message.content,
                turn.message.tool_calls.is_some()
            );

            // Native function calling.
            if let Some(calls) = turn.message.tool_calls.as_ref().filter(|c| !c.is_empty()) {
                messages.push(ChatMessage::assistant_with_tool_calls(
                    &turn.message.content,
                    calls.clone(),
                ));
                let mut progressed = false;
                for call in calls {
                    let args = normalize_arguments(call.function.arguments.clone());
                    if !seen.insert(call_key(&call.function.name, &args)) {
                        // Every call needs its result; this one is ours.
                        messages.push(
                            ChatMessage::tool_result(&call.function.name, REPEATED_CALL)
                                .with_label(Label::default()),
                        );
                        continue;
                    }
                    progressed = true;
                    let outcome = self.execute_native_call(call, plan, confirm, rec).await;
                    messages.push(
                        ChatMessage::tool_result(&outcome.name, &self.for_model(&outcome))
                            .with_label(Label::of_tool(&outcome.name, &outcome.args)),
                    );
                    trace.push(outcome);
                    if cancel.is_some_and(Cancellation::is_cancelled) {
                        return self.cancelled_result(&trace);
                    }
                }
                if cancel.is_some_and(Cancellation::is_cancelled) {
                    return self.cancelled_result(&trace);
                }
                if let Some(report) = blocked_report(&trace) {
                    return Ok(report);
                }
                // A round of nothing but repeats is a model going in circles.
                if !progressed {
                    break;
                }
                continue;
            }

            // Tool calls embedded in the reply text.
            let text_calls = parser::parse_tool_calls(&turn.message.content);
            if !text_calls.is_empty() {
                // Text calls have no per-call result slot: a round of only
                // repeats ends the turn, and a repeat among new calls is
                // dropped.
                let fresh: Vec<&parser::ToolCall> = text_calls
                    .iter()
                    .filter(|c| {
                        seen.insert(call_key(&c.name, &normalize_arguments(c.args.clone())))
                    })
                    .collect();
                if fresh.is_empty() {
                    break;
                }
                messages.push(ChatMessage::text("assistant", &turn.message.content));
                for call in fresh {
                    let outcome = self.execute_text_call(call, plan, confirm, rec).await;
                    // Sent with the user role, but it is the tool's output:
                    // labelled as such, so egress does not take it for the
                    // user's own words.
                    messages.push(
                        ChatMessage::text(
                            "user",
                            &format!("Result of {}:\n{}", outcome.name, self.for_model(&outcome)),
                        )
                        .with_label(Label::of_tool(&outcome.name, &outcome.args)),
                    );
                    trace.push(outcome);
                    if cancel.is_some_and(Cancellation::is_cancelled) {
                        return self.cancelled_result(&trace);
                    }
                }
                if cancel.is_some_and(Cancellation::is_cancelled) {
                    return self.cancelled_result(&trace);
                }
                if let Some(report) = blocked_report(&trace) {
                    return Ok(report);
                }
                continue;
            }

            // A reply that is only a malformed tool call is not an answer.
            // Showing it would put protocol JSON in front of the user, and
            // silently ignoring it would hide that nothing was executed.
            if parser::looks_like_tool_call(&turn.message.content) {
                return Ok("I tried to use a tool, but the model's instruction was malformed, so nothing was run. Try rephrasing the request.".to_string());
            }

            // A plain answer.
            let answer = final_answer(&turn.message.content);
            if answer.is_empty() {
                return Ok(if trace.is_empty() {
                    self.empty_answer(input)
                } else {
                    render_execution_report(&trace)
                });
            }
            return Ok(self.grounded_answer(answer, input, &trace, &messages, plan.web));
        }

        // Out of rounds, or going in circles. If a tool found anything, ask
        // once more for an answer from it: a log of tool calls is not an
        // answer, and nemotron-3-ultra had what it needed several rounds
        // before the limit. The tools stay offered (a strict server can
        // reject a conversation of tool calls sent without them); a reply
        // that calls one anyway gets the report.
        let found = trace.iter().any(|o| o.status == OutcomeStatus::Succeeded);
        if found && !cancel.is_some_and(Cancellation::is_cancelled) {
            messages.push(ChatMessage::text("user", CLOSING_REQUEST).with_label(Label::default()));
            self.fit_prompt(&system, &mut messages, &mut history_len, reserved);
            let reply = self
                .ask_model(
                    &system,
                    &system_label,
                    &messages,
                    Some(&tools),
                    confirm,
                    cancel,
                    None,
                    rec,
                )
                .await;
            if let Ok(ModelReply::Turn(turn)) = reply {
                rec.model_turn(&turn);
                let content = &turn.message.content;
                let calls_tool = turn
                    .message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|c| !c.is_empty())
                    || !parser::parse_tool_calls(content).is_empty()
                    || parser::looks_like_tool_call(content);
                let answer = final_answer(content);
                if !calls_tool && !answer.is_empty() {
                    return Ok(self.grounded_answer(answer, input, &trace, &messages, plan.web));
                }
            }
        }
        Ok(render_execution_report(&trace))
    }

    /// The deterministic opening of a web turn: search, then read the top hit.
    ///
    /// Returns the outcomes in execution order and appends the matching
    /// assistant/tool messages so the model sees the exchange as one it took
    /// part in. A blocked search ends the chain (there is nothing to read); a
    /// blocked fetch does not, because the search snippets are still a source
    /// the model can answer from.
    pub(super) async fn web_first_steps(
        &self,
        input: &str,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        messages: &mut Vec<ChatMessage>,
        rec: &TurnRecorder,
    ) -> Vec<ToolOutcome> {
        let mut outcomes = Vec::new();

        let Some(search) = plan
            .tools
            .iter()
            .find(|t| t.name.starts_with("mcp.search."))
            .map(|t| t.name.clone())
        else {
            return outcomes;
        };

        let outcome = self
            .execute_offered(search, json!({ "query": input }), false, plan, confirm, rec)
            .await;
        let succeeded = outcome.status == OutcomeStatus::Succeeded;
        let seen = self.prompt_tool_output(&outcome.output);
        push_synthetic_exchange(messages, &outcome, &self.marked(&outcome, seen.clone()));
        outcomes.push(outcome);

        if !succeeded {
            return outcomes;
        }

        // The first URL in rank order is the page to read. This is a property
        // of search output in general, not of one server's formatting.
        let Some(url) = first_url(&seen) else {
            return outcomes;
        };
        let Some(fetch) = plan
            .tools
            .iter()
            .find(|t| t.name.starts_with("mcp.fetch."))
            .map(|t| t.name.clone())
        else {
            return outcomes;
        };

        let outcome = self
            .execute_offered(fetch, json!({ "url": url }), false, plan, confirm, rec)
            .await;
        let seen = self.prompt_tool_output(&outcome.output);
        push_synthetic_exchange(messages, &outcome, &self.marked(&outcome, seen.clone()));
        outcomes.push(outcome);
        outcomes
    }

    /// What to return when the user cancels a tool-capable turn.
    ///
    /// With no tool trace there is nothing to show, so the cancellation is
    /// propagated and nothing is persisted. Once a tool has run, its result
    /// is real: report it and say plainly that the model never explained it.
    pub(super) fn cancelled_result(&self, trace: &[ToolOutcome]) -> Result<String> {
        // Nothing that ran: propagate, so the caller persists no answer and
        // the UI can keep whatever partial text it already streamed.
        // Everything blocked means the user cancelled the approval itself,
        // which is also not a result worth recording.
        if trace.is_empty() || trace.iter().all(|o| o.status.is_blocked()) {
            Err(JarvisError::Cancelled)
        } else {
            Ok(format!(
                "{}\n\n(cancelled before the model explained the result)",
                render_execution_report(trace)
            ))
        }
    }

    /// Turn a single tool outcome into the user-facing answer.
    pub(super) fn answer_from_outcome(&self, outcome: &ToolOutcome) -> String {
        if outcome.status.is_blocked() {
            return render_blocked_report(std::slice::from_ref(outcome));
        }
        if outcome.name.starts_with("mcp.") {
            return format!("{}:\n{}", outcome.name, outcome.output);
        }
        render_tool_output(outcome)
    }

    /// Attach a caveat when the answer is not supported by retrieved material.
    ///
    /// Two different failures are reported the same way, because from the
    /// user's side they are the same thing: a web answer that retrieved
    /// nothing (all tool calls failed, or the model answered from memory) must
    /// not read as if it came from a source. When sources *were* read, only
    /// figures that appear nowhere in them are called out.
    pub(super) fn grounded_answer(
        &self,
        answer: String,
        input: &str,
        trace: &[ToolOutcome],
        messages: &[ChatMessage],
        web: bool,
    ) -> String {
        let answer = self.relay_checked(answer, input, trace, messages);
        let mut sources: Vec<&str> = trace
            .iter()
            .filter(|o| o.status == OutcomeStatus::Succeeded)
            .map(|o| o.output.as_str())
            .collect();

        if sources.is_empty() {
            if !web {
                return answer;
            }
            // Measured failure: `fetch` was handed a question, failed, and the
            // model answered "1.54.0" from weights with no hint that nothing
            // had been read.
            return format!(
                "{}\n\nNote: I could not retrieve a source for this answer, so it comes from the model's own knowledge and may be out of date.",
                answer.trim_end()
            );
        }

        sources.push(input);
        verify::with_caveat(answer, &sources)
    }

    /// `answer` with a note when it repeats a command or data-carrying
    /// address that came from untrusted content - what this request read,
    /// or untrusted messages earlier in the conversation - and from nothing
    /// the owner wrote or allowed. The gate guards Jarvis's actions; this
    /// covers the answer handing the attacker's instruction to the owner.
    pub(super) fn relay_checked(
        &self,
        answer: String,
        input: &str,
        trace: &[ToolOutcome],
        messages: &[ChatMessage],
    ) -> String {
        let policy = self.egress_policy();
        let names = |sources: &[Source]| -> String {
            sources
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut untrusted: Vec<(String, &str)> = Vec::new();
        let mut trusted: Vec<&str> = vec![input];
        for outcome in trace
            .iter()
            .filter(|o| o.status == OutcomeStatus::Succeeded)
        {
            let sources =
                taint::untrusted_sources(&Label::of_tool(&outcome.name, &outcome.args), &policy);
            if sources.is_empty() {
                trusted.push(&outcome.output);
            } else {
                untrusted.push((names(&sources), &outcome.output));
            }
        }
        for (message, label) in messages.iter().zip(egress::label(messages)) {
            let sources = taint::untrusted_sources(&label, &policy);
            if sources.is_empty() {
                if message.role == "user" {
                    trusted.push(&message.content);
                }
            } else {
                untrusted.push((names(&sources), &message.content));
            }
        }
        // Pages read or found are citations, not instructions: fetched
        // addresses (this turn's calls, and the sources of web labels in
        // the conversation) and every address a search listed.
        let mut cited: Vec<String> = Vec::new();
        for outcome in trace {
            if let Some(url) = outcome.args.get("url").and_then(Value::as_str) {
                cited.push(url.to_string());
            }
            if outcome.name.starts_with("mcp.search.") {
                cited.extend(taint::urls_in(&outcome.output));
            }
        }
        for (message, label) in messages.iter().zip(egress::label(messages)) {
            cited.extend(
                label
                    .sources
                    .iter()
                    .filter(|s| s.starts_with("http"))
                    .cloned(),
            );
            if message
                .tool_name
                .as_deref()
                .is_some_and(|t| t.starts_with("mcp.search."))
            {
                cited.extend(taint::urls_in(&message.content));
            }
        }
        match taint::relay_note(&taint::relayed(&answer, &untrusted, &trusted, &cited)) {
            Some(note) => format!("{}\n\n{}", answer.trim_end(), note),
            None => answer,
        }
    }
}
