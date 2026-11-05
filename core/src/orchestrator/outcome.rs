//! What a turn did: tool outcomes, the turn report and its recorder, the footer.

use super::*;

/// Where a single tool invocation ended up.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum OutcomeStatus {
    Succeeded,
    Failed,
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
            OutcomeStatus::Denied => "denied by policy",
            OutcomeStatus::Declined => "declined",
            OutcomeStatus::NeedsConfirmation => {
                "requires confirmation (auto-denied in non-interactive mode)"
            }
            OutcomeStatus::Withheld => "not sent",
            OutcomeStatus::Untrusted => {
                "not run: it followed untrusted content and nobody could be asked"
            }
        }
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
    /// The model did not respond (unreachable, rate limited); the answer is
    /// a fallback, not the model's.
    pub model_error: Option<String>,
    /// The model and provider that answered; empty when no model was used.
    pub model: String,
    pub provider: String,
    /// Distinct values replaced by placeholders before sending.
    pub redacted: usize,
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: Value,
    /// `executed`, `failed`, `denied`, `declined`, `needs_confirmation`,
    /// `withheld` (not sent, for what it would send off this machine), or
    /// `not_offered` for a call outside the turn's plan.
    pub status: &'static str,
}

/// Collects a [`TurnReport`] while a turn runs.
///
/// Passed down explicitly rather than stored on the orchestrator: the daemon
/// shares one orchestrator, and a field would mix up two turns running at
/// once.
#[derive(Default)]
pub(super) struct TurnRecorder {
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
}

impl TurnRecorder {
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

    pub(super) fn finish(self, answer: String, plan: &TurnPlan) -> TurnReport {
        let (model_calls, prompt_tokens, completion_tokens) = *self.model.lock().unwrap();
        let redacted = self.redactor.lock().unwrap().count();
        let (model, provider) = self.used.lock().unwrap().clone().unwrap_or_default();
        TurnReport {
            model_error: self.model_error.lock().unwrap().take(),
            withheld: self.withheld.load(std::sync::atomic::Ordering::Relaxed),
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
    parts.join(" · ")
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
        OutcomeStatus::Denied => "denied",
        OutcomeStatus::Declined => "declined",
        OutcomeStatus::NeedsConfirmation => "needs_confirmation",
        OutcomeStatus::Withheld => "withheld",
        OutcomeStatus::Untrusted => "untrusted",
    }
}
