//! What a turn did: tool outcomes, the turn report and its recorder, the footer.

use super::*;

/// Where a single tool invocation ended up.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum OutcomeStatus {
    Succeeded,
    Failed,
    /// Not run: the call lacked its required arguments and went back to the
    /// model to fix. Kept apart from `Failed`, which may have run.
    Malformed,
    /// Explicitly denied by the permission policy.
    Denied,
    /// The user was asked and said no.
    Declined,
    /// Requires confirmation; auto-denied because the agent runs headless.
    NeedsConfirmation,
    /// Not run because of what it would send off this machine; `output`
    /// says what. Permissions allowed the tool: loosening them would not
    /// help, and the report must not suggest it.
    Withheld,
    /// An action the model asked for after reading untrusted content, with
    /// nobody to ask. `output` names the sources.
    Untrusted,
    /// Autopilot did not take it: it needs the owner's yes, and waits for
    /// `/approve`. Not blocked: the turn carries on with the rest.
    Deferred,
}

impl OutcomeStatus {
    pub(super) fn is_blocked(&self) -> bool {
        matches!(
            self,
            OutcomeStatus::Denied
                | OutcomeStatus::Declined
                | OutcomeStatus::NeedsConfirmation
                | OutcomeStatus::Withheld
                | OutcomeStatus::Untrusted
        )
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            OutcomeStatus::Succeeded => "executed",
            OutcomeStatus::Failed => "failed",
            OutcomeStatus::Malformed => "not run: arguments missing",
            OutcomeStatus::Denied => "denied by policy",
            OutcomeStatus::Declined => "declined",
            OutcomeStatus::NeedsConfirmation => {
                "requires confirmation (auto-denied in non-interactive mode)"
            }
            OutcomeStatus::Withheld => "not sent",
            OutcomeStatus::Untrusted => {
                "not run: it followed untrusted content and nobody could be asked"
            }
            OutcomeStatus::Deferred => "waiting for /approve",
        }
    }
}

impl ToolOutcome {
    /// Declined with a reason: the owner steering, not stopping (6.7b).
    pub(super) fn redirects(&self) -> bool {
        self.status == OutcomeStatus::Declined
            && self
                .result
                .as_ref()
                .is_some_and(|r| r.get("reason").is_some())
    }
}

/// One tool invocation and its result, kept only for the current turn.
#[derive(Debug, Clone)]
pub(super) struct ToolOutcome {
    pub(super) name: String,
    pub(super) args: Value,
    pub(super) status: OutcomeStatus,
    pub(super) output: String,
    /// The structured result, when the tool ran.
    ///
    /// `output` is the compact string that goes to the model and the audit
    /// log; `result` is the untouched JSON. New renderers read `result`
    /// instead of re-parsing `output`, which older ones have to do because
    /// they predate this field.
    pub(super) result: Option<Value>,
    /// The command pattern that decided the outcome, if one did.
    pub(super) pattern: Option<String>,
}

/// Everything observable about one turn, not just its answer.
///
/// The answer alone cannot tell a correct turn from a lucky one: "Here are
/// the files" reads the same whether `list_dir` ran or the model made the
/// listing up. The eval harness grades the route and the tool calls as well
/// as the text, and it needs them from the orchestrator rather than from
/// parsing logs.
#[derive(Debug, Clone, Default)]
pub struct TurnReport {
    pub answer: String,
    /// The route after the planner narrowed it: `Direct`, `Model` or
    /// `Tool(name)`, in the same spelling as the golden routing test.
    pub route: String,
    /// The tools this turn was allowed to use.
    pub offered: Vec<String>,
    /// Every tool call attempted, in order, including refused ones.
    pub calls: Vec<ToolCallRecord>,
    pub model_calls: u32,
    /// Summed over model calls. `None` if the provider reported no counts.
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Whether a request of this turn went to a remote provider.
    pub remote: bool,
    /// A request was not sent to the remote model and nothing answered.
    pub withheld: bool,
    /// A busy GPU sent this turn to the remote tier.
    pub spilled: bool,
    /// A spill was tried, but what it would send may not go: answered
    /// locally instead.
    pub spill_withheld: bool,
    /// Plan mode refused an action that would change something.
    pub plan_refused: bool,
    /// The model did not respond (unreachable, rate limited); the answer is
    /// a fallback, not the model's.
    pub model_error: Option<String>,
    /// The model and provider that answered; empty when no model was used.
    pub model: String,
    pub provider: String,
    /// Distinct values replaced by placeholders before sending.
    pub redacted: usize,
    /// Where this turn's time went.
    pub timing: TurnTiming,
}

/// Where one turn's time went, in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TurnTiming {
    /// From the request arriving to the answer.
    pub total_ms: u64,
    /// Spent waiting for the owner to answer prompts: their time, not Jarvis's.
    pub you_ms: u64,
    pub model: Vec<ModelTiming>,
    /// Each tool call, including any prompt it asked.
    pub tools: Vec<ToolTiming>,
    /// Spent waiting for a GPU slot behind another window's model call.
    #[serde(default)]
    pub queue_ms: u64,
}

impl TurnTiming {
    /// The turn without the owner's time, or time spent queued behind
    /// another window: what "slow" is judged on.
    pub fn jarvis_ms(&self) -> u64 {
        self.total_ms
            .saturating_sub(self.you_ms)
            .saturating_sub(self.queue_ms)
    }
}

/// One request to a model.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ModelTiming {
    pub model: String,
    /// Our clock, around the whole request.
    pub wall_ms: u64,
    /// The provider's account, when it gives one (Ollama does).
    pub load_ms: Option<u64>,
    pub prompt_ms: Option<u64>,
    pub gen_ms: Option<u64>,
    pub prompt_tokens: Option<u64>,
    pub gen_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ToolTiming {
    pub name: String,
    pub ms: u64,
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: Value,
    /// `executed`, `failed`, `malformed` (not run: arguments missing), `denied`, `declined`, `needs_confirmation`,
    /// `withheld` (not sent, for what it would send off this machine), or
    /// `not_offered` for a call outside the turn's plan.
    pub status: &'static str,
}

/// Collects a [`TurnReport`] while a turn runs.
///
/// Passed down explicitly rather than stored on the orchestrator: the daemon
/// shares one orchestrator, and a field would mix up two turns running at
/// once.
pub(super) struct TurnRecorder<'c> {
    /// The conversation this turn belongs to.
    pub(super) conversation: &'c Conversation,
    pub(super) calls: std::sync::Mutex<Vec<ToolCallRecord>>,
    pub(super) model: std::sync::Mutex<(u32, Option<u64>, Option<u64>)>,
    /// Everything this turn's answer may contain: the tools it ran and the
    /// context the model was given. Stored with the answer.
    pub(super) label: std::sync::Mutex<Label>,
    /// One redactor per turn, so a placeholder means the same value in every
    /// request of the turn.
    pub(super) redactor: std::sync::Mutex<Redactor>,
    /// What the user agreed to send this turn; a new concern asks again.
    pub(super) approved: std::sync::Mutex<Vec<Concern>>,
    /// The same for web calls, which go to a different party: agreeing to
    /// send a file to the model is not agreeing to search for it.
    pub(super) web_approved: std::sync::Mutex<Vec<Concern>>,
    /// Every address in what the model was shown this turn.
    pub(super) shown_urls: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Whether any request of this turn went to a remote provider.
    pub(super) remote: std::sync::atomic::AtomicBool,
    /// The model and provider that answered, for the footer: with a local
    /// fallback it is not the configured one.
    pub(super) used: std::sync::Mutex<Option<(String, String)>>,
    /// A request of this turn was not sent.
    pub(super) withheld: std::sync::atomic::AtomicBool,
    /// Why the model gave no answer, when it did not.
    pub(super) model_error: std::sync::Mutex<Option<String>>,
    /// Where the time went so far.
    pub(super) timing: std::sync::Mutex<TurnTiming>,
    /// The route and tools, once planned: a turn that fails later is still
    /// logged with them.
    pub(super) planned: std::sync::Mutex<Option<(String, Vec<String>)>>,
    /// Where to say what the turn is doing now, for a client to show.
    pub(super) progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// The conversation's settings when the turn started.
    pub(super) pinned: crate::conversation::Pinned,
    /// A busy GPU sent this turn to the remote tier (6.5b).
    pub(super) spilled: std::sync::atomic::AtomicBool,
    /// A spill could not send what it had to: no more spills this turn.
    pub(super) spill_withheld: std::sync::atomic::AtomicBool,
    /// Prompts an Autopilot turn answered no to, because it never prompts.
    pub(super) not_done: std::sync::Mutex<Vec<String>>,
    /// Plan mode refused an action of this turn.
    pub(super) plan_refused: std::sync::atomic::AtomicBool,
    /// Where each step is reported as it starts and ends (6.7b).
    pub(super) steps: Option<tokio::sync::mpsc::UnboundedSender<jarvis_common::ipc::StepEvent>>,
    step_count: std::sync::atomic::AtomicU32,
}

impl<'c> TurnRecorder<'c> {
    pub(super) fn new(conversation: &'c Conversation) -> Self {
        TurnRecorder {
            conversation,
            calls: Default::default(),
            model: Default::default(),
            label: Default::default(),
            redactor: Default::default(),
            approved: Default::default(),
            web_approved: Default::default(),
            shown_urls: Default::default(),
            remote: Default::default(),
            used: Default::default(),
            withheld: Default::default(),
            model_error: Default::default(),
            timing: Default::default(),
            planned: Default::default(),
            progress: Default::default(),
            pinned: conversation.pin(),
            spilled: Default::default(),
            spill_withheld: Default::default(),
            not_done: Default::default(),
            plan_refused: Default::default(),
            steps: None,
            step_count: Default::default(),
        }
    }

    /// A step starts: its number, for `step_end`.
    pub(super) fn step_start(&self, kind: &'static str, name: &str, summary: String) -> u32 {
        let n = self
            .step_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if let Some(steps) = &self.steps {
            let _ = steps.send(jarvis_common::ipc::StepEvent {
                step: n,
                kind,
                name: name.to_string(),
                summary,
                state: "running",
                detail: None,
                ms: None,
            });
        }
        n
    }

    /// Step `n` ended, as `state`.
    pub(super) fn step_end(
        &self,
        n: u32,
        kind: &'static str,
        name: &str,
        state: &'static str,
        detail: Option<String>,
        ms: u64,
    ) {
        if let Some(steps) = &self.steps {
            let _ = steps.send(jarvis_common::ipc::StepEvent {
                step: n,
                kind,
                name: name.to_string(),
                summary: String::new(),
                state,
                detail,
                ms: Some(ms),
            });
        }
    }

    /// The settings this turn's requests use: as pinned, on the remote
    /// tier once spilled.
    pub(super) fn settings(&self) -> crate::conversation::Pinned {
        let mut pinned = self.pinned.clone();
        pinned.remote |= self.spilled.load(std::sync::atomic::Ordering::Relaxed);
        pinned
    }
}

impl TurnRecorder<'_> {
    pub(super) fn used(&self, model: &ModelConfig, provider: &str, remote: bool) {
        *self.used.lock().unwrap() = Some((model.default_model.clone(), provider.to_string()));
        if remote {
            self.remote
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn model_failed(&self, error: &JarvisError) {
        *self.model_error.lock().unwrap() = Some(error.to_string());
    }

    pub(super) fn saw(&self, label: &Label) {
        self.label.lock().unwrap().absorb(label);
    }

    pub(super) fn label(&self) -> Label {
        self.label.lock().unwrap().clone()
    }

    /// The labels of a request about to go to the model.
    pub(super) fn saw_request(&self, system_text: &str, system: &Label, messages: &[ChatMessage]) {
        self.saw(system);
        for label in egress::label(messages) {
            self.saw(&label);
        }
        self.saw_shown(system_text);
        for message in messages {
            self.saw_shown(&message.content);
        }
    }

    /// Remember the addresses in text the model was shown: following one of
    /// them is copying, not building (`taint::is_built`).
    pub(super) fn saw_shown(&self, text: &str) {
        self.shown_urls.lock().unwrap().extend(taint::urls_in(text));
    }

    pub(super) fn tool(&self, outcome: &ToolOutcome, status: &'static str) {
        // Only output that was produced can end up in the answer.
        if status == "executed" {
            self.saw(&Label::of_tool(&outcome.name, &outcome.args));
        }
        self.calls.lock().unwrap().push(ToolCallRecord {
            name: outcome.name.clone(),
            args: outcome.args.clone(),
            status,
        });
    }

    /// Say what the turn is doing now. A client that went away is not an
    /// error: the turn still finishes.
    /// Time spent waiting for a GPU slot.
    pub(super) fn queued(&self, waited: std::time::Duration) {
        self.timing.lock().unwrap().queue_ms += waited.as_millis() as u64;
    }

    pub(super) fn step(&self, text: String) {
        if let Some(progress) = &self.progress {
            let _ = progress.send(text);
        }
    }

    /// Model requests made so far.
    pub(super) fn model_calls(&self) -> u32 {
        self.model.lock().unwrap().0
    }

    /// One model request took `wall` and the provider reported `turn.timing`.
    pub(super) fn model_time(&self, model: &str, wall: std::time::Duration, turn: &ChatTurn) {
        self.timing.lock().unwrap().model.push(ModelTiming {
            model: model.to_string(),
            wall_ms: wall.as_millis() as u64,
            load_ms: turn.timing.load_ms,
            prompt_ms: turn.timing.prompt_ms,
            gen_ms: turn.timing.gen_ms,
            prompt_tokens: turn.prompt_tokens,
            gen_tokens: turn.completion_tokens,
        });
    }

    pub(super) fn tool_time(&self, name: &str, took: std::time::Duration) {
        self.timing.lock().unwrap().tools.push(ToolTiming {
            name: name.to_string(),
            ms: took.as_millis() as u64,
        });
    }

    pub(super) fn model_turn(&self, turn: &ChatTurn) {
        let mut model = self.model.lock().unwrap();
        model.0 += 1;
        if let Some(n) = turn.prompt_tokens {
            model.1 = Some(model.1.unwrap_or(0) + n);
        }
        if let Some(n) = turn.completion_tokens {
            model.2 = Some(model.2.unwrap_or(0) + n);
        }
    }

    /// The report for a turn that ended without an answer, with the route
    /// if it got that far.
    pub(super) fn finish_unplanned(self, planned: Option<(String, Vec<String>)>) -> TurnReport {
        let (route, offered) = planned.unwrap_or_default();
        let (model_calls, prompt_tokens, completion_tokens) = *self.model.lock().unwrap();
        let (model, provider) = self.used.lock().unwrap().clone().unwrap_or_default();
        TurnReport {
            model_error: self.model_error.lock().unwrap().take(),
            withheld: self.withheld.load(std::sync::atomic::Ordering::Relaxed),
            spilled: self.spilled.load(std::sync::atomic::Ordering::Relaxed),
            spill_withheld: self
                .spill_withheld
                .load(std::sync::atomic::Ordering::Relaxed),
            plan_refused: self.plan_refused.load(std::sync::atomic::Ordering::Relaxed),
            model,
            provider,
            remote: self.remote.load(std::sync::atomic::Ordering::Relaxed),
            redacted: 0,
            answer: String::new(),
            route,
            offered,
            calls: self.calls.into_inner().unwrap(),
            model_calls,
            prompt_tokens,
            completion_tokens,
            timing: self.timing.into_inner().unwrap(),
        }
    }

    pub(super) fn finish(self, answer: String, plan: &TurnPlan) -> TurnReport {
        let (model_calls, prompt_tokens, completion_tokens) = *self.model.lock().unwrap();
        let redacted = self.redactor.lock().unwrap().count();
        let (model, provider) = self.used.lock().unwrap().clone().unwrap_or_default();
        TurnReport {
            model_error: self.model_error.lock().unwrap().take(),
            withheld: self.withheld.load(std::sync::atomic::Ordering::Relaxed),
            spilled: self.spilled.load(std::sync::atomic::Ordering::Relaxed),
            spill_withheld: self
                .spill_withheld
                .load(std::sync::atomic::Ordering::Relaxed),
            plan_refused: self.plan_refused.load(std::sync::atomic::Ordering::Relaxed),
            model,
            provider,
            remote: self.remote.load(std::sync::atomic::Ordering::Relaxed),
            redacted,
            answer,
            route: route_label(&plan.route),
            offered: plan.names().into_iter().map(String::from).collect(),
            calls: self.calls.into_inner().unwrap(),
            model_calls,
            prompt_tokens,
            completion_tokens,
            timing: self.timing.into_inner().unwrap(),
        }
    }
}

/// The line shown under every answer: what produced it and where it ran.
///
/// Kept beside the answer, never in it: stored in history, it would be sent
/// back to the model as content and imitated in later replies.
pub fn footer(report: &TurnReport) -> String {
    let (model, provider) = (report.model.as_str(), report.provider.as_str());
    if report.model_calls == 0 && report.withheld {
        return "not sent · no model".to_string();
    }
    if report.model_calls == 0 && report.model_error.is_some() {
        return "no answer from the model".to_string();
    }
    if report.model_calls == 0 {
        // A deterministic answer: say which tool, and that no model was used.
        return match report.calls.first() {
            Some(call) => format!("answered by {} · no model", call.name),
            None => "no model".to_string(),
        };
    }
    let mut parts = vec![model.to_string()];
    parts.push(if report.remote {
        format!("remote: {}", provider)
    } else {
        "local".to_string()
    });
    let tokens = report.prompt_tokens.unwrap_or(0) + report.completion_tokens.unwrap_or(0);
    if report.prompt_tokens.is_some() || report.completion_tokens.is_some() {
        parts.push(format!("{} tokens", short_count(tokens)));
    }
    if report.redacted > 0 {
        parts.push(format!("{} redacted", report.redacted));
    }
    let mut line = parts.join(" · ");
    if report.spilled {
        line.push_str(" (GPU busy)");
    }
    line
}

/// 950 -> "950", 1234 -> "1.2k", 15000 -> "15k".
pub(super) fn short_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{}k", n / 1000),
    }
}

/// The route in the spelling used by the golden routing test and eval cases.
pub fn route_label(route: &Route) -> String {
    match route {
        Route::Direct => "Direct".to_string(),
        Route::Model => "Model".to_string(),
        Route::Tool { name, .. } => format!("Tool({})", name),
    }
}

pub(super) fn status_label(status: &OutcomeStatus) -> &'static str {
    match status {
        OutcomeStatus::Succeeded => "executed",
        OutcomeStatus::Failed => "failed",
        OutcomeStatus::Malformed => "malformed",
        OutcomeStatus::Denied => "denied",
        OutcomeStatus::Declined => "declined",
        OutcomeStatus::NeedsConfirmation => "needs_confirmation",
        OutcomeStatus::Withheld => "withheld",
        OutcomeStatus::Untrusted => "untrusted",
        OutcomeStatus::Deferred => "deferred",
    }
}
