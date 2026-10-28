//! Agent orchestrator - planning, tool selection, execution loop.
//!
//! # Design notes
//!
//! ## Routing
//!
//! Tool selection is done by [`crate::router`], not by the model. Small
//! local models cannot reliably decide *whether* a request needs a tool, and
//! a wrong answer here is expensive: an unnecessary tool call burns a model
//! round-trip and pollutes the answer, while a missing one silently fails
//! the request. Routing in code is exact, fast, testable and auditable.
//!
//! ## Memory
//!
//! The orchestrator keeps two deliberately separate stores of information:
//!
//! * **Persistent memory** (`memory.db`) holds *only* clean, user-facing
//!   turns: the user's request and the final answer.
//! * **Scratchpad** (in-memory, scoped to a single `run()` call) holds the
//!   tool trace of the current turn.
//!
//! Keeping them apart is the single most important invariant in the agent
//! loop. If raw tool-call JSON (`{"tool_calls": [...]}`) is written to
//! memory, the next turn reads the model back its own stale instructions as
//! "history" and concludes it already performed the action. That is exactly
//! how "I opened Firefox" gets said about a command that was blocked.
//!
//! ## Honesty
//!
//! **When an action is blocked, the model does not get to narrate the
//! outcome.** A blocked tool produces a deterministic report rendered by
//! this module. There is no code path where a blocked action can be
//! described as a success, because the model's free text is never used for
//! that turn.
//!
//! Tool *results* are rendered deterministically too. The model is used for
//! conversation, where it adds value; it is not used to describe facts it
//! could get wrong.
//!
//! ## Confirmation
//!
//! A tool with an `ask` policy needs a human decision. The orchestrator does
//! not know how to reach a human, so it takes an optional
//! [`ConfirmationHandler`]: the daemon supplies one backed by the client
//! socket, the CLI supplies one backed by the terminal. When no handler is
//! present (a headless run) the call is denied, never silently allowed.

use crate::cancel::Cancellation;
use crate::config::{Config, ModelConfig};
use crate::context::{self, ContextPlan};
use crate::egress::redact::Redactor;
use crate::egress::{
    self, Concern, DataPolicy, Destination, Label, Locality, Origin, Policy, Verdict,
};
use crate::error::{JarvisError, Result};
use crate::mcp::client::McpClient;
use crate::memory::{self, MemoryManager, Message};
use crate::model::{ChatMessage, ChatTurn, ModelFunction, ModelToolCall};
use crate::permissions::{PermissionAction, PermissionManager};
use crate::planner::{self, OfferedTool, TurnPlan};
use crate::provider::{self, ChatRequest, Provider};
use crate::router::{self, Route};
use crate::tools::{
    filesystem::{
        EditFileTool, ListDirTool, ReadFileTool, SearchFilesTool, StatFileTool, WriteFileTool,
    },
    parser,
    shell::RunShellTool,
    system::{GetSystemInfoTool, ListProcessesTool},
    ToolRegistry,
};
use crate::verify;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use uuid::Uuid;

/// Asked when a tool call needs the user's approval before it runs.
///
/// Implementations block until they have an answer. Returning `false` is
/// always safe: the call is treated as declined and nothing executes.
#[async_trait::async_trait]
pub trait ConfirmationHandler: Send + Sync {
    /// `true` if the user approved running `tool` with `args`.
    async fn confirm(&self, tool: &str, args: &Value) -> bool;

    /// Pick one of `choices`, or `None` for no. A handler that only knows
    /// yes/no answers through `confirm`, and its yes is the first choice -
    /// which is why the first choice must always be the narrowest yes.
    async fn choose(&self, tool: &str, args: &Value, choices: &[Choice]) -> Option<char> {
        if self.confirm(tool, args).await {
            choices.first().map(|c| c.key)
        } else {
            None
        }
    }
}

pub use jarvis_common::ipc::Choice;

/// The name under which sending to a remote provider is confirmed. Not a
/// real tool: it reuses the confirmation path every client already has.
pub const EGRESS_TOOL: &str = "egress.send";

/// Asked when a remote request was not sent: answer with the local model?
pub const EGRESS_LOCAL_TOOL: &str = "egress.local";

/// `https://openrouter.ai/api/v1` -> `openrouter.ai`.
fn host_of(base_url: &str) -> String {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| base_url.to_string())
}

/// A recalled message's stored label, or the label for not knowing.
fn recalled_label(result: &memory::RecallResult) -> Label {
    result
        .label
        .clone()
        .unwrap_or_else(|| Label::unknown(&result.role))
}

/// The parts of a model request that do not depend on the tier.
struct Request<'a> {
    system: &'a str,
    messages: &'a [ChatMessage],
    tools: Option<&'a [Value]>,
}

/// Where requests with these settings go. A config that cannot be read was
/// already rejected at startup (`provider::from_config`); if one gets here
/// anyway, it is treated as remote with an undeclared policy, which asks
/// rather than sends.
fn destination_of(model: &ModelConfig) -> Destination {
    egress::destination(model).unwrap_or(Destination {
        locality: Locality::Remote,
        data_policy: DataPolicy::Unknown,
    })
}

/// What came back from [`Orchestrator::ask_model`].
enum ModelReply {
    Turn(ChatTurn),
    /// The request was not sent; this explains why, as the answer.
    Withheld(String),
}

fn describe_concern(concern: &Concern) -> String {
    match (concern.origin, &concern.path) {
        (Origin::File, Some(path)) => format!("the contents of {}", path),
        (Origin::File, None) => "file contents".into(),
        (Origin::Shell, _) => "command output".into(),
        (Origin::System, _) => "information about this machine".into(),
        (Origin::Other, _) => "earlier or unrecognised content of unknown origin".into(),
        (origin, _) => format!("{:?} content", origin).to_lowercase(),
    }
}

/// Maximum number of model round-trips inside a single user turn.
const MAX_ITERATIONS: usize = 4;

/// Maximum number of directory entries shown in a reply.
const MAX_LIST_ENTRIES: usize = 200;

/// Maximum number of characters of file content shown in a reply.
const MAX_READ_CHARS: usize = 4000;

/// Where a single tool invocation ended up.
#[derive(Debug, Clone, PartialEq)]
enum OutcomeStatus {
    Succeeded,
    Failed,
    /// Explicitly denied by the permission policy.
    Denied,
    /// The user was asked and said no.
    Declined,
    /// Requires confirmation; auto-denied because the agent runs headless.
    NeedsConfirmation,
}

impl OutcomeStatus {
    fn is_blocked(&self) -> bool {
        matches!(
            self,
            OutcomeStatus::Denied | OutcomeStatus::Declined | OutcomeStatus::NeedsConfirmation
        )
    }

    fn label(&self) -> &'static str {
        match self {
            OutcomeStatus::Succeeded => "executed",
            OutcomeStatus::Failed => "failed",
            OutcomeStatus::Denied => "denied by policy",
            OutcomeStatus::Declined => "declined",
            OutcomeStatus::NeedsConfirmation => {
                "requires confirmation (auto-denied in non-interactive mode)"
            }
        }
    }
}

/// One tool invocation and its result, kept only for the current turn.
#[derive(Debug, Clone)]
struct ToolOutcome {
    name: String,
    args: Value,
    status: OutcomeStatus,
    output: String,
    /// The structured result, when the tool ran.
    ///
    /// `output` is the compact string that goes to the model and the audit
    /// log; `result` is the untouched JSON. New renderers read `result`
    /// instead of re-parsing `output`, which older ones have to do because
    /// they predate this field.
    result: Option<Value>,
    /// The command pattern that decided the outcome, if one did.
    pattern: Option<String>,
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
    /// `executed`, `failed`, `denied`, `declined`, `needs_confirmation`, or
    /// `not_offered` for a call outside the turn's plan.
    pub status: &'static str,
}

/// Collects a [`TurnReport`] while a turn runs.
///
/// Passed down explicitly rather than stored on the orchestrator: the daemon
/// shares one orchestrator, and a field would mix up two turns running at
/// once.
#[derive(Default)]
struct TurnRecorder {
    calls: std::sync::Mutex<Vec<ToolCallRecord>>,
    model: std::sync::Mutex<(u32, Option<u64>, Option<u64>)>,
    /// Everything this turn's answer may contain: the tools it ran and the
    /// context the model was given. Stored with the answer.
    label: std::sync::Mutex<Label>,
    /// One redactor per turn, so a placeholder means the same value in every
    /// request of the turn.
    redactor: std::sync::Mutex<Redactor>,
    /// What the user agreed to send this turn; a new concern asks again.
    approved: std::sync::Mutex<Vec<Concern>>,
    /// Whether any request of this turn went to a remote provider.
    remote: std::sync::atomic::AtomicBool,
    /// The model and provider that answered, for the footer: with a local
    /// fallback it is not the configured one.
    used: std::sync::Mutex<Option<(String, String)>>,
    /// A request of this turn was not sent.
    withheld: std::sync::atomic::AtomicBool,
    /// Why the model gave no answer, when it did not.
    model_error: std::sync::Mutex<Option<String>>,
}

impl TurnRecorder {
    fn used(&self, model: &ModelConfig, provider: &str, remote: bool) {
        *self.used.lock().unwrap() = Some((model.default_model.clone(), provider.to_string()));
        if remote {
            self.remote
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn model_failed(&self, error: &JarvisError) {
        *self.model_error.lock().unwrap() = Some(error.to_string());
    }

    fn saw(&self, label: &Label) {
        self.label.lock().unwrap().absorb(label);
    }

    fn label(&self) -> Label {
        self.label.lock().unwrap().clone()
    }

    /// The labels of a request about to go to the model.
    fn saw_request(&self, system: &Label, messages: &[ChatMessage]) {
        self.saw(system);
        for label in egress::label(messages) {
            self.saw(&label);
        }
    }

    fn tool(&self, outcome: &ToolOutcome, status: &'static str) {
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

    fn model_turn(&self, turn: &ChatTurn) {
        let mut model = self.model.lock().unwrap();
        model.0 += 1;
        if let Some(n) = turn.prompt_tokens {
            model.1 = Some(model.1.unwrap_or(0) + n);
        }
        if let Some(n) = turn.completion_tokens {
            model.2 = Some(model.2.unwrap_or(0) + n);
        }
    }

    fn finish(self, answer: String, plan: &TurnPlan) -> TurnReport {
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
fn short_count(n: u64) -> String {
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

fn status_label(status: &OutcomeStatus) -> &'static str {
    match status {
        OutcomeStatus::Succeeded => "executed",
        OutcomeStatus::Failed => "failed",
        OutcomeStatus::Denied => "denied",
        OutcomeStatus::Declined => "declined",
        OutcomeStatus::NeedsConfirmation => "needs_confirmation",
    }
}

pub struct Orchestrator {
    config: Config,
    /// The `[model]` tier: local unless configured otherwise.
    client: Arc<dyn Provider>,
    /// The `[remote]` tier, when configured.
    remote: Option<Arc<dyn Provider>>,
    /// Whether this session's turns go to the remote tier.
    use_remote: std::sync::atomic::AtomicBool,
    tools: ToolRegistry,
    memory: Option<MemoryManager>,
    session_id: String,
    permissions: PermissionManager,
    mcp: McpClient,
}

impl Orchestrator {
    pub fn new() -> Result<Self> {
        Self::with_config(Config::load())
    }

    /// Fails if the configured provider cannot be built. A bad provider name
    /// is not recovered from by switching to Ollama: for a user who chose a
    /// hosted model that would quietly change where prompts go, and for one
    /// who chose local-only it would hide a typo behind a working assistant.
    pub fn with_config(config: Config) -> Result<Self> {
        let client = provider::from_config(&config)?;
        let remote = match &config.remote {
            Some(r) => Some(provider::from_model(&r.model)?),
            None => None,
        };
        let permissions = PermissionManager::new(&config)?;
        let orchestrator = Self::with_parts(config, client, permissions).with_remote(remote);
        // Start MCP servers in the background: a slow or broken server must
        // not delay the first prompt, and discovery results simply appear in
        // the cache when they are ready.
        let mcp_startup = orchestrator.mcp.clone();
        let cfg_clone = orchestrator.config.clone();
        tokio::spawn(async move {
            if let Err(e) = mcp_startup.start_from_config(&cfg_clone).await {
                warn!("MCP startup failed: {}", e);
            }
        });
        Ok(orchestrator)
    }

    /// Build from explicit parts, without starting MCP servers.
    ///
    /// `with_config` is the production entry point. This one lets the eval
    /// harness and tests supply a scripted provider and a throwaway
    /// permission policy instead of the user's real ones, and start MCP
    /// with [`Orchestrator::start_mcp`] when they need it to be ready.
    pub fn with_parts(
        config: Config,
        client: Arc<dyn Provider>,
        permissions: PermissionManager,
    ) -> Self {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(ListDirTool));
        tools.register(Box::new(ReadFileTool));
        tools.register(Box::new(SearchFilesTool));
        tools.register(Box::new(StatFileTool));
        tools.register(Box::new(WriteFileTool));
        tools.register(Box::new(EditFileTool));
        tools.register(Box::new(RunShellTool));
        tools.register(Box::new(GetSystemInfoTool));
        tools.register(Box::new(ListProcessesTool));
        let session_id = Uuid::new_v4().to_string();
        let memory = MemoryManager::new(config.clone()).ok();
        if let Some(ref m) = memory {
            let _ = m.create_session(&session_id);
        }
        Self {
            config,
            client,
            remote: None,
            use_remote: std::sync::atomic::AtomicBool::new(false),
            tools,
            memory,
            session_id,
            permissions,
            mcp: McpClient::new(),
        }
    }

    /// Attach the `[remote]` tier's provider. New sessions start on it only
    /// if `[remote] default = true`.
    pub fn with_remote(mut self, client: Option<Arc<dyn Provider>>) -> Self {
        let default = self.config.remote.as_ref().is_some_and(|r| r.default);
        self.use_remote = std::sync::atomic::AtomicBool::new(client.is_some() && default);
        self.remote = client;
        self
    }

    /// Switch this session between the local and remote tier.
    pub fn set_remote(&self, on: bool) -> Result<()> {
        if on && self.remote.is_none() {
            return Err(JarvisError::Config(
                "no remote model is configured: add a [remote] section to config.toml".into(),
            ));
        }
        self.use_remote
            .store(on, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    pub fn uses_remote(&self) -> bool {
        self.use_remote.load(std::sync::atomic::Ordering::Relaxed) && self.remote.is_some()
    }

    /// The model settings and provider this session's turns go to.
    fn tier(&self) -> (&ModelConfig, &Arc<dyn Provider>) {
        match (&self.config.remote, &self.remote) {
            (Some(cfg), Some(client)) if self.uses_remote() => (&cfg.model, client),
            _ => (&self.config.model, &self.client),
        }
    }

    /// The model to offer when a remote request was not sent: `[model]`,
    /// if this session is on the remote tier and `[model]` is local.
    fn local_fallback(&self) -> Option<(&ModelConfig, &Arc<dyn Provider>)> {
        (self.uses_remote() && !destination_of(&self.config.model).is_remote())
            .then_some((&self.config.model, &self.client))
    }

    /// Start the configured MCP servers and wait until discovery finishes.
    ///
    /// A web eval case that ran before discovery would be planned without
    /// the search tools and fail for a reason unrelated to the model.
    pub async fn start_mcp(&self) -> Result<()> {
        self.mcp.start_from_config(&self.config).await
    }

    /// The provider and model a turn will be sent to, for `jarvis status`.
    pub fn model_info(&self) -> (&'static str, &str, &str) {
        let (cfg, client) = self.tier();
        (client.name(), &cfg.default_model, &cfg.base_url)
    }

    /// Where this orchestrator's requests go. A config that cannot be read
    /// was already rejected at startup (`provider::from_config`); if one gets
    /// here anyway, it is treated as remote with an undeclared policy, which
    /// asks rather than sends.
    pub fn destination(&self) -> Destination {
        destination_of(self.tier().0)
    }

    /// The owner's policy for remote requests, with their allow-list.
    fn egress_policy(&self) -> Policy {
        let home = dirs::home_dir().unwrap_or_default();
        let mut policy = Policy::defaults(&home);
        let from_prompts = egress::allowed_folders(&self.allow_file());
        policy.file_allow = self
            .config
            .egress
            .file_allow
            .iter()
            .chain(from_prompts.iter())
            .map(|d| std::path::PathBuf::from(crate::router::expand_tilde(d)))
            .collect();
        policy
    }

    /// Where "always allow this folder" answers are kept.
    pub fn allow_file(&self) -> std::path::PathBuf {
        self.config.general.data_dir.join(egress::ALLOW_FILE)
    }

    /// Every model request goes through here: the egress check, redaction,
    /// the call, and putting personal data back into what comes back.
    ///
    /// A local tier gets the request unchanged. A remote one gets it only if
    /// the policy allows everything in it or the user approves, and always
    /// with secrets and personal data replaced by placeholders. When it is
    /// not sent, the user is asked whether the local model should answer.
    #[allow(clippy::too_many_arguments)]
    async fn ask_model(
        &self,
        system: &str,
        system_label: &Label,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder,
    ) -> Result<ModelReply> {
        rec.saw_request(system_label, messages);
        let (cfg, client) = self.tier();
        let request = Request {
            system,
            messages,
            tools,
        };
        if !destination_of(cfg).is_remote() {
            return self
                .send(cfg, client, request, false, cancel, sink, rec)
                .await;
        }

        let mut labels = vec![system_label.clone()];
        labels.extend(egress::label(messages));
        let refused = match egress::check(&labels, &self.egress_policy()) {
            Verdict::Send => None,
            Verdict::Never(concerns) => Some(self.withheld(cfg, client, &concerns, true)),
            Verdict::Ask(concerns) => {
                let new: Vec<Concern> = {
                    let approved = rec.approved.lock().unwrap();
                    concerns
                        .into_iter()
                        .filter(|c| !approved.contains(c))
                        .collect()
                };
                if new.is_empty() || self.approve_sending(cfg, client, &new, confirm).await {
                    rec.approved.lock().unwrap().extend(new);
                    None
                } else {
                    Some(self.withheld(cfg, client, &new, false))
                }
            }
        };

        let Some(why) = refused else {
            return self
                .send(cfg, client, request, true, cancel, sink, rec)
                .await;
        };
        // The owner's rule: when nothing could be sent, ask each time whether
        // the local model should answer instead. Nobody to ask means no.
        if let (Some((local_cfg, local)), Some(confirm)) = (self.local_fallback(), confirm) {
            let args = json!({ "model": local_cfg.default_model, "reason": why });
            if confirm.confirm(EGRESS_LOCAL_TOOL, &args).await {
                return self
                    .send(local_cfg, local, request, false, cancel, sink, rec)
                    .await;
            }
        }
        rec.withheld
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(ModelReply::Withheld(why))
    }

    /// Ask whether `concerns` may be sent: once, or (for files) always from
    /// their folders, which writes the allow file.
    async fn approve_sending(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        concerns: &[Concern],
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> bool {
        let Some(confirm) = confirm else {
            return false;
        };
        let folders = egress::folders_to_allow(concerns);
        let mut choices = vec![Choice {
            key: 'y',
            label: "send once".into(),
        }];
        if !folders.is_empty() {
            choices.push(Choice {
                key: 'a',
                label: format!("always allow {}", folders.join(", ")),
            });
        }
        let args = json!({
            "provider": client.name(),
            "url": cfg.base_url,
            "would_send": concerns.iter().map(describe_concern).collect::<Vec<_>>(),
        });
        match confirm.choose(EGRESS_TOOL, &args, &choices).await {
            Some('a') => {
                for folder in &folders {
                    if let Err(e) = egress::allow_folder(&self.allow_file(), folder) {
                        warn!("could not save allowed folder {}: {}", folder, e);
                    }
                }
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Send one request to one tier, redacting and restoring when `remote`.
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        request: Request<'_>,
        remote: bool,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder,
    ) -> Result<ModelReply> {
        let redacted = remote.then(|| {
            let mut redactor = rec.redactor.lock().unwrap();
            let system = redactor.redact(request.system);
            let messages: Vec<ChatMessage> = request
                .messages
                .iter()
                .map(|m| {
                    let mut m = m.clone();
                    m.content = redactor.redact(&m.content);
                    m
                })
                .collect();
            (system, messages, redactor.count() > 0)
        });
        let (system, messages) = match &redacted {
            Some((s, m, _)) => (s.as_str(), m.as_slice()),
            None => (request.system, request.messages),
        };
        let chat = ChatRequest {
            model: &cfg.default_model,
            system: Some(system),
            messages,
            tools: request.tools,
            options: Some(self.model_options(cfg)),
        };
        // A placeholder can be split across streamed chunks and shown before
        // it is restored; a redacted remote answer is shown whole instead.
        let stream_to = sink.filter(|_| !redacted.as_ref().is_some_and(|r| r.2));
        let mut turn = match stream_to {
            Some(sink) => client.chat_turn_streaming(chat, cancel, sink).await?,
            None => client.chat_turn(chat, cancel).await?,
        };
        // For a remote turn the footer names where the data went (the host),
        // not the wire format: "openai" for an OpenRouter model misleads.
        let provider = if remote {
            host_of(&cfg.base_url)
        } else {
            client.name().to_string()
        };
        rec.used(cfg, &provider, remote);
        if redacted.is_some() {
            let redactor = rec.redactor.lock().unwrap();
            turn.message.content = redactor.restore(&turn.message.content);
            for call in turn.message.tool_calls.iter_mut().flatten() {
                let args = redactor.restore(&call.function.arguments.to_string());
                if let Ok(restored) = serde_json::from_str(&args) {
                    call.function.arguments = restored;
                }
            }
        }
        Ok(ModelReply::Turn(turn))
    }

    /// The answer for a request that was not sent.
    fn withheld(
        &self,
        cfg: &ModelConfig,
        client: &Arc<dyn Provider>,
        concerns: &[Concern],
        never: bool,
    ) -> String {
        let items: Vec<String> = concerns.iter().map(describe_concern).collect();
        let why = if never {
            "which is never sent to a remote provider"
        } else {
            "which was not approved for sending"
        };
        format!(
            "Nothing was sent to {} ({}). This request would have included {}, {}.\n\n\
             Ask without that content, use /local, or allow a folder with `jarvis egress allow <folder>`.",
            client.name(),
            cfg.base_url,
            items.join(", "),
            why
        )
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn set_session(&mut self, session_id: String) {
        self.session_id = session_id.clone();
        if let Some(ref m) = self.memory {
            let _ = m.create_session(&session_id);
        }
    }

    /// Handle one user turn with no way to reach a human.
    ///
    /// Any tool with an `ask` policy is denied. Use [`Orchestrator::run_with`]
    /// when a front-end is present and can prompt.
    pub async fn run(&self, input: &str) -> Result<String> {
        self.run_with(input, None).await
    }

    /// Handle one user turn, asking `confirm` before an `ask`-policy tool
    /// runs.
    pub async fn run_with(
        &self,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> Result<String> {
        self.run_with_options(input, confirm, None, None).await
    }

    /// Handle one user turn with cancellation and optional token streaming.
    ///
    /// `sink` receives the assistant text of a *direct* conversation as the
    /// model produces it. A tool-capable turn is not streamed: the model may
    /// answer with a tool call, and text shown before that decision would be
    /// pulled back from the screen. The caller still receives the complete
    /// answer as the return value.
    ///
    /// Cancellation is checked between model calls and inside them. A tool
    /// that has already started always runs to completion.
    pub async fn run_with_options(
        &self,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
    ) -> Result<String> {
        self.run_turn(input, confirm, cancel, sink)
            .await
            .map(|report| report.answer)
    }

    /// One turn's answer and its footer, for clients that show both.
    pub async fn run_with_footer(
        &self,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
    ) -> Result<(String, String)> {
        let report = self.run_turn(input, confirm, cancel, sink).await?;
        let footer = footer(&report);
        Ok((report.answer, footer))
    }

    /// Handle one user turn and report what happened in it, not just the
    /// answer. The eval harness grades this report.
    pub async fn run_turn(
        &self,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
    ) -> Result<TurnReport> {
        if cancel.is_some_and(Cancellation::is_cancelled) {
            return Err(JarvisError::Cancelled);
        }
        let rec = TurnRecorder::default();

        // Bound the log line: a pasted document should not fill the log.
        info!("Processing: {}", context::truncate_with_marker(input, 200));

        // Snapshot history *before* persisting the new user message so the
        // prompt never contains the request twice.
        let history = self.history_for_prompt();

        if let Some(ref m) = self.memory {
            let _ = m.add_labeled_message(
                &self.session_id,
                "user",
                input,
                Some(&Label::of(Origin::User)),
            );
        }
        rec.saw(&Label::of(Origin::User));

        let route = router::route(input);
        // One decision, one owner: the planner narrows the route and produces
        // the exact tool set this turn may use. Both the provider request and
        // the system prompt are rendered from `plan.tools`, so the model can
        // never be told about a tool it cannot call.
        let plan = planner::plan(input, route, &self.tools, &self.mcp.list_tools());
        info!("route: {:?} tools: {:?}", plan.route, plan.names());

        let answer = match &plan.route {
            // Pure conversation: no tools involved at all.
            Route::Direct => {
                self.converse(input, &history, confirm, cancel, sink, &rec)
                    .await?
            }

            // A confidently identified tool call: run it and report the facts.
            Route::Tool { name, args } => {
                if cancel.is_some_and(Cancellation::is_cancelled) {
                    return Err(JarvisError::Cancelled);
                }
                // The tool is the whole turn: once it has run, its result is
                // the answer even if the user has just cancelled. Dropping it
                // would hide an action that really happened.
                let outcome = self
                    .execute_call(name.clone(), args.clone(), confirm, &rec)
                    .await;
                // A confirmation that was denied *because* the user cancelled
                // is not a tool result worth persisting: the turn never ran.
                // If the tool really ran, its report is still the answer.
                if cancel.is_some_and(Cancellation::is_cancelled) && outcome.status.is_blocked() {
                    return Err(JarvisError::Cancelled);
                }
                self.answer_from_outcome(&outcome)
            }

            // Ambiguous: let the model pick from the tools this plan offers.
            Route::Model => {
                self.model_with_tools(input, &history, &plan, confirm, cancel, &rec)
                    .await?
            }
        };

        self.persist_assistant(&answer, &rec.label());
        Ok(rec.finish(answer, &plan))
    }

    pub fn list_tools(&self) -> Vec<String> {
        let mut tools: Vec<String> = self
            .tools
            .list()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        // MCP tools appear once discovery completes; before that this is
        // simply the native catalogue.
        tools.extend(self.mcp.list_tools().into_iter().map(|t| t.full_name));
        tools.sort();
        tools
    }

    // ------------------------------------------------------------ conversations

    /// Plain conversation. The model is given no tools, so it cannot try to
    /// use one - which is exactly what we want for "who are you?".
    async fn converse(
        &self,
        input: &str,
        history: &[Message],
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder,
    ) -> Result<String> {
        let (system_prompt, recall_label) =
            self.with_recall(input, self.conversation_system_prompt());
        let plan = context::assemble(&system_prompt, input, history, &self.config.context, 0);
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
    async fn model_with_tools(
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

        for _ in 0..MAX_ITERATIONS {
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
                for call in calls {
                    let outcome = self.execute_native_call(call, plan, confirm, rec).await;
                    messages.push(
                        ChatMessage::tool_result(
                            &outcome.name,
                            &self.prompt_tool_output(&outcome.output),
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

            // Tool calls embedded in the reply text.
            let text_calls = parser::parse_tool_calls(&turn.message.content);
            if !text_calls.is_empty() {
                messages.push(ChatMessage::text("assistant", &turn.message.content));
                for call in &text_calls {
                    let outcome = self.execute_text_call(call, plan, confirm, rec).await;
                    // Sent with the user role, but it is the tool's output:
                    // labelled as such, so egress does not take it for the
                    // user's own words.
                    messages.push(
                        ChatMessage::text(
                            "user",
                            &format!(
                                "Result of {}:\n{}",
                                outcome.name,
                                self.prompt_tool_output(&outcome.output)
                            ),
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
            return Ok(self.grounded_answer(answer, input, &trace, plan.web));
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
    async fn web_first_steps(
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
            .execute_offered(search, json!({ "query": input }), plan, confirm, rec)
            .await;
        let succeeded = outcome.status == OutcomeStatus::Succeeded;
        let seen = self.prompt_tool_output(&outcome.output);
        push_synthetic_exchange(messages, &outcome, &seen);
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
            .execute_offered(fetch, json!({ "url": url }), plan, confirm, rec)
            .await;
        let seen = self.prompt_tool_output(&outcome.output);
        push_synthetic_exchange(messages, &outcome, &seen);
        outcomes.push(outcome);
        outcomes
    }

    /// What to return when the user cancels a tool-capable turn.
    ///
    /// With no tool trace there is nothing to show, so the cancellation is
    /// propagated and nothing is persisted. Once a tool has run, its result
    /// is real: report it and say plainly that the model never explained it.
    fn cancelled_result(&self, trace: &[ToolOutcome]) -> Result<String> {
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
    fn answer_from_outcome(&self, outcome: &ToolOutcome) -> String {
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
    fn grounded_answer(
        &self,
        answer: String,
        input: &str,
        trace: &[ToolOutcome],
        web: bool,
    ) -> String {
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

    // ---------------------------------------------------------------- prompts

    /// Provider tuning options for a single request.
    ///
    /// `num_ctx` is set explicitly instead of relying on the provider's
    /// default, which changes between versions and is what decides how much
    /// of the prompt survives.
    fn model_options(&self, model: &ModelConfig) -> Value {
        serde_json::json!({
            "temperature": model.temperature,
            "num_ctx": self.config.context.num_ctx,
        })
    }

    /// Tokens the tool schemas will cost in this request.
    fn tool_schema_tokens(&self, schemas: &[Value]) -> usize {
        schemas
            .iter()
            .map(|schema| context::estimate_tokens(&schema.to_string()) + 8)
            .sum()
    }

    /// A tool result as the model should see it: bounded, and honest about
    /// what was cut. The user-facing rendering uses the full output.
    fn prompt_tool_output(&self, output: &str) -> String {
        context::truncate_with_marker(output, self.config.context.max_tool_result_chars)
    }

    /// Record what was sent and what it cost. This is the measurement that
    /// keeps the estimate honest against the provider's real token counts.
    fn log_plan(&self, plan: &ContextPlan) {
        let budget = self.config.context.prompt_budget();
        info!(
            "context: est_prompt_tokens={} budget={} messages={} condensed={} omitted={} request_truncated={}",
            plan.estimated_tokens,
            budget,
            plan.messages.len(),
            plan.condensed_messages,
            plan.omitted_messages,
            plan.request_truncated
        );
        if plan.estimated_tokens > budget {
            warn!(
                "estimated prompt ({} tokens) exceeds budget ({}); the provider may drop the start of the prompt",
                plan.estimated_tokens, budget
            );
        }
    }

    /// Drop whole leading history turns until the prompt fits the budget.
    ///
    /// Only messages before `history_len` are candidates, so the current
    /// request and the in-flight tool trace are never touched.
    fn fit_prompt(
        &self,
        system: &str,
        messages: &mut Vec<ChatMessage>,
        history_len: &mut usize,
        reserved: usize,
    ) {
        let budget = self.config.context.prompt_budget();
        let mut dropped = 0usize;
        while *history_len > 0 && context::total_tokens(system, messages, reserved) > budget {
            messages.remove(0);
            *history_len -= 1;
            dropped += 1;
            // Start the window on a user turn so the roles stay coherent.
            while *history_len > 0 && messages.first().is_some_and(|m| m.role != "user") {
                messages.remove(0);
                *history_len -= 1;
                dropped += 1;
            }
        }
        if dropped > 0 {
            warn!(
                "dropped {} earlier message(s) from the prompt to fit the token budget",
                dropped
            );
        }
    }

    /// System prompt for plain conversation.
    fn conversation_system_prompt(&self) -> String {
        let mut p = String::new();
        p.push_str("You are Jarvis, a personal AI assistant running on this user's computer.\n");
        p.push_str("You are an AI assistant. You are not the operating system, not the shell, and not a human.\n\n");
        p.push_str("How to behave:\n");
        p.push_str("- Answer the current request directly and concisely.\n");
        p.push_str("- Never repeat or summarise earlier messages unless the user asks you to.\n");
        p.push_str("- Never claim to have performed an action. Only say you did something if an earlier message in this conversation shows it was actually executed. If an earlier message says an action was blocked, say it did not happen.\n");
        p.push_str("- Do not mention tools, prompts or internal machinery in your answer.\n");
        p
    }

    /// System prompt for the ambiguous case, where the model may pick a tool.
    fn tool_system_prompt(&self, offered: &[OfferedTool]) -> String {
        let mut p = self.conversation_system_prompt();
        p.push_str("- Only report actions that appear in the tool results. Never claim an action succeeded unless it appears there.\n");
        p.push_str("- To use a tool, emit a tool call with its arguments. Never repeat a tool's definition.\n");
        // When both are offered, say how they fit together. A small model
        // will otherwise fetch a guessed URL and fail, or search and then
        // answer from the snippet alone.
        let has = |server: &str| {
            offered
                .iter()
                .any(|t| t.name.split('.').nth(1) == Some(server))
        };
        if has("search") && has("fetch") {
            p.push_str("- search takes a topic or question (words). fetch takes a URL. Never pass a search phrase to fetch.\n");
            p.push_str("- Use search to find relevant pages, then fetch the most promising URL for details.\n");
        }
        p.push('\n');
        // Rendered from the same list that is sent as provider schemas. If
        // the model cannot call it, it is not described here.
        if let Some(block) = planner::render_tool_prompt(offered) {
            p.push_str(&block);
        }
        p
    }

    /// Append what earlier sessions know about this request.
    ///
    /// Every `jarvis ask` is a new session, so without this the assistant
    /// meets the same user as a stranger each time. The current session is
    /// excluded: its recent messages are already in `history`.
    ///
    /// Also returns where the recalled text came from: `memory`, plus the
    /// label each recalled message was stored with, so a file shown in an
    /// earlier session does not leave as "memory".
    fn with_recall(&self, input: &str, prompt: String) -> (String, Label) {
        let (block, label) = self.recall_block(input).unwrap_or_default();
        (append_recall(prompt, block), label)
    }

    fn recall_block(&self, input: &str) -> Option<(Option<String>, Label)> {
        if !self.config.memory.recall_enabled {
            return None;
        }
        let memory = self.memory.as_ref()?;
        let results = memory
            .recall_in(
                input,
                self.config.memory.recall_limit,
                Some(&self.session_id),
            )
            .ok()?;
        // Asking an old question again matches that question in FTS; there is
        // nothing to remember and it must not pose as memory.
        let results: Vec<_> = results
            .into_iter()
            .filter(|result| !memory::is_echo(result, input))
            .collect();
        // Recall is optional context. On a remote tier, a recalled message
        // the policy would not send is left out rather than asked about: a
        // prompt on every question (every message stored before labels is
        // unknown) would be answered by reflex. Required content - what the
        // user asked about - is still asked about in ask_model.
        let results: Vec<_> = if self.destination().is_remote() {
            let policy = self.egress_policy();
            let before = results.len();
            let kept: Vec<_> = results
                .into_iter()
                .filter(|r| {
                    let mut label = recalled_label(r);
                    label.absorb(&Label::of(Origin::Memory));
                    egress::check(&[label], &policy) == Verdict::Send
                })
                .collect();
            if kept.len() < before {
                info!(
                    "recall: {} message(s) left out of a remote request by policy",
                    before - kept.len()
                );
            }
            kept
        } else {
            results
        };
        let block = memory::render_recall(&results, self.config.memory.recall_max_chars);
        let mut label = Label::default();
        if block.is_some() {
            info!("recall: {} earlier message(s) added", results.len());
            label.absorb(&Label::of(Origin::Memory));
            for result in &results {
                label.absorb(&recalled_label(result));
            }
        }
        Some((block, label))
    }

    /// Recent history, oldest first. How much of it reaches the model is
    /// decided by [`crate::context::assemble`], not here.
    fn history_for_prompt(&self) -> Vec<Message> {
        let Some(ref m) = self.memory else {
            return Vec::new();
        };
        m.get_recent_messages(&self.session_id, self.config.context.max_history_fetch)
            .unwrap_or_default()
    }

    /// The answer when the model did not respond. Names the model and where
    /// it is: "the local model" was wrong once a remote tier existed.
    fn fallback_answer(&self, error: &JarvisError) -> String {
        let (cfg, _) = self.tier();
        format!(
            "I could not reach {} ({}), so I cannot answer that yet.\n\nReason: {}",
            cfg.default_model,
            host_of(&cfg.base_url),
            error
        )
    }

    fn empty_answer(&self, input: &str) -> String {
        format!(
            "I did not get a usable answer back from the model for: {}\n\n\
             Try rephrasing, or run `jarvis status` to check the model connection.",
            input
        )
    }

    fn persist_assistant(&self, content: &str, label: &Label) {
        if let Some(ref m) = self.memory {
            let _ = m.add_labeled_message(&self.session_id, "assistant", content, Some(label));
        }
    }

    // -------------------------------------------------------------- execution

    /// Execute a native tool call through the permission gate and registry.
    async fn execute_native_call(
        &self,
        call: &ModelToolCall,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder,
    ) -> ToolOutcome {
        let name = call.function.name.clone();
        let args = normalize_arguments(call.function.arguments.clone());
        self.execute_offered(name, args, plan, confirm, rec).await
    }

    /// Execute a tool call parsed out of the model's text.
    async fn execute_text_call(
        &self,
        call: &parser::ToolCall,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder,
    ) -> ToolOutcome {
        let args = normalize_arguments(call.args.clone());
        self.execute_offered(call.name.clone(), args, plan, confirm, rec)
            .await
    }

    /// Run a model-requested call only if this turn's plan offered it.
    ///
    /// A hallucinated tool name must not become an execution path around the
    /// planner. This is a correctness guard, not a permission check: the
    /// permission system still runs for every call that gets this far.
    async fn execute_offered(
        &self,
        name: String,
        args: Value,
        plan: &TurnPlan,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder,
    ) -> ToolOutcome {
        if !plan.allows(&name) {
            let available = if plan.tools.is_empty() {
                "none".to_string()
            } else {
                plan.names().join(", ")
            };
            let outcome = ToolOutcome {
                name,
                args,
                status: OutcomeStatus::Failed,
                output: format!(
                    "That tool is not available for this request. Available tools: {}.",
                    available
                ),
                result: None,
                pattern: None,
            };
            rec.tool(&outcome, "not_offered");
            return outcome;
        }
        self.execute_call(name, args, confirm, rec).await
    }

    /// Run a single tool call and record it in the turn report.
    async fn execute_call(
        &self,
        name: String,
        args: Value,
        confirm: Option<&dyn ConfirmationHandler>,
        rec: &TurnRecorder,
    ) -> ToolOutcome {
        let outcome = self.gate_and_run(name, args, confirm).await;
        rec.tool(&outcome, status_label(&outcome.status));
        outcome
    }

    /// Run a single tool call through the permission gate and the registry.
    async fn gate_and_run(
        &self,
        name: String,
        args: Value,
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> ToolOutcome {
        let args = self.sanitize_args(&name, args);
        let verdict = self.permissions.check_permission(&name, &args);

        match verdict.action {
            PermissionAction::Deny => {
                let msg = match &verdict.pattern {
                    Some(p) => format!(
                        "Tool {} denied by pattern \"{}\". Args: {}",
                        name,
                        p,
                        compact_args(&args)
                    ),
                    None => format!(
                        "Tool {} denied by policy. Args: {}",
                        name,
                        compact_args(&args)
                    ),
                };
                let _ = self.permissions.log_audit(
                    Some(&self.session_id),
                    &name,
                    &args,
                    "denied",
                    Some(&msg),
                );
                ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Denied,
                    output: msg,
                    result: None,
                    pattern: verdict.pattern,
                }
            }
            PermissionAction::Ask => match confirm {
                Some(handler) => {
                    if handler.confirm(&name, &args).await {
                        self.run_tool(name, args, "confirmed", verdict.pattern)
                            .await
                    } else {
                        let msg = format!(
                            "Tool {} was declined by the user. Args: {}",
                            name,
                            compact_args(&args)
                        );
                        let _ = self.permissions.log_audit(
                            Some(&self.session_id),
                            &name,
                            &args,
                            "declined_by_user",
                            Some(&msg),
                        );
                        ToolOutcome {
                            name,
                            args,
                            status: OutcomeStatus::Declined,
                            output: msg,
                            result: None,
                            pattern: verdict.pattern,
                        }
                    }
                }
                None => {
                    let msg = format!(
                        "Tool {} requires confirmation and was auto-denied in non-interactive mode. Args: {}",
                        name,
                        compact_args(&args)
                    );
                    let _ = self.permissions.log_audit(
                        Some(&self.session_id),
                        &name,
                        &args,
                        "ask_auto_denied",
                        Some(&msg),
                    );
                    ToolOutcome {
                        name,
                        args,
                        status: OutcomeStatus::NeedsConfirmation,
                        output: msg,
                        result: None,
                        pattern: verdict.pattern,
                    }
                }
            },
            PermissionAction::Allow => self.run_tool(name, args, "allowed", verdict.pattern).await,
        }
    }

    /// Execute an approved tool and record the outcome.
    ///
    /// `decision` is what lands in the audit log, so a call a human approved
    /// is distinguishable after the fact from one the policy allowed
    /// outright.
    async fn run_tool(
        &self,
        name: String,
        args: Value,
        decision: &str,
        pattern: Option<String>,
    ) -> ToolOutcome {
        // Check if it's an MCP tool (namespaced as mcp.server.tool)
        if name.starts_with("mcp.") {
            let parts: Vec<&str> = name.split('.').collect();
            if parts.len() >= 3 {
                let server = parts[1].to_string();
                let tool_name = parts[2..].join(".");
                match self.mcp.call_tool(&server, &tool_name, args.clone()).await {
                    Ok(res) => {
                        let _ = self.permissions.log_audit(
                            Some(&self.session_id),
                            &name,
                            &args,
                            decision,
                            Some(&res),
                        );
                        let res_val = serde_json::json!({ "result": res });
                        ToolOutcome {
                            name,
                            args,
                            status: OutcomeStatus::Succeeded,
                            output: res.clone(),
                            result: Some(res_val),
                            pattern,
                        }
                    }
                    Err(e) => {
                        let msg = format!("Tool {} failed: {}", name, e);
                        let _ = self.permissions.log_audit(
                            Some(&self.session_id),
                            &name,
                            &args,
                            &format!("{}_failed", decision),
                            Some(&msg),
                        );
                        ToolOutcome {
                            name,
                            args,
                            status: OutcomeStatus::Failed,
                            output: msg,
                            result: None,
                            pattern,
                        }
                    }
                }
            } else {
                let msg = format!("Invalid MCP tool name: {}", name);
                ToolOutcome {
                    name,
                    args,
                    status: OutcomeStatus::Failed,
                    output: msg,
                    result: None,
                    pattern,
                }
            }
        } else {
            match self.tools.execute(&name, args.clone()).await {
                Ok(res) => {
                    let compact = compact_output(&name, &res);
                    let _ = self.permissions.log_audit(
                        Some(&self.session_id),
                        &name,
                        &args,
                        decision,
                        Some(&compact),
                    );
                    ToolOutcome {
                        name,
                        args,
                        status: OutcomeStatus::Succeeded,
                        output: compact,
                        result: Some(res),
                        pattern,
                    }
                }
                Err(e) => {
                    let msg = format!("Tool {} failed: {}", name, e);
                    let _ = self.permissions.log_audit(
                        Some(&self.session_id),
                        &name,
                        &args,
                        &format!("{}_failed", decision),
                        Some(&msg),
                    );
                    ToolOutcome {
                        name,
                        args,
                        status: OutcomeStatus::Failed,
                        output: msg,
                        result: None,
                        pattern,
                    }
                }
            }
        }
    }

    /// Some models echo the tool schema back as the arguments object, e.g.
    /// `{"type": "function", "function": "list_dir", "parameters": {...}}`.
    /// Unwrap that so the real arguments reach the tool.
    fn sanitize_args(&self, tool: &str, args: Value) -> Value {
        let required = if tool.starts_with("mcp.") {
            self.mcp
                .list_tools()
                .into_iter()
                .find(|t| t.full_name == tool)
                .and_then(|t| t.schema.get("required").and_then(|r| r.as_array()).cloned())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        } else {
            self.tools.required_args(tool)
        };
        unwrap_echoed_schema(args, &required)
    }
}

// ------------------------------------------------------------------ helpers

/// The real arguments, when a model nested them inside an echo of the tool
/// schema: `{"parameters": {...}}` or `{"properties": {...}, "type": "object"}`.
/// Arguments that already have every required key are left alone.
///
/// A nested object is used only if every required key holds a value. When the
/// model echoes the schema itself, `properties.path` is `{"type": "string"}`;
/// passing that on would turn "no arguments" into a wrong argument.
fn unwrap_echoed_schema(args: Value, required: &[String]) -> Value {
    if required.is_empty() {
        return args;
    }
    if required.iter().all(|k| args.get(k).is_some()) {
        return args;
    }
    let is_value = |v: &Value| !(v.is_object() && v.get("type").is_some());
    for wrapper in ["parameters", "properties"] {
        if let Some(nested) = args.get(wrapper).and_then(|p| p.as_object()) {
            if required.iter().all(|k| nested.get(k).is_some_and(is_value)) {
                return Value::Object(nested.clone());
            }
        }
    }
    args
}

/// Tool arguments sometimes arrive as a JSON-encoded string.
fn normalize_arguments(args: Value) -> Value {
    match &args {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        _ => args,
    }
}

/// Record a tool call the orchestrator made and its result, in the shape the
/// provider expects from a model-initiated call.
fn push_synthetic_exchange(messages: &mut Vec<ChatMessage>, outcome: &ToolOutcome, seen: &str) {
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
fn first_url(text: &str) -> Option<String> {
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

/// Append the recall block to a system prompt, if there is one.
///
/// The blank line keeps it visually separate from the instructions above;
/// without it a small model reads the remembered text as another rule.
fn append_recall(mut prompt: String, block: Option<String>) -> String {
    let Some(block) = block else {
        return prompt;
    };
    prompt.push('\n');
    prompt.push_str(&block);
    prompt
}

/// Does the request already contain the address it wants read?
///
/// When it does, searching first would be busywork: the user said which page
/// to read, and `fetch` is the whole job.
fn input_names_url(input: &str) -> bool {
    let lower = input.to_lowercase();
    lower.contains("http://") || lower.contains("https://")
}

/// Clean up a model response before it becomes the user-facing answer.
fn final_answer(response: &str) -> String {
    let cleaned = parser::strip_tool_artifacts(response);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        response.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// Render a compact, model-friendly version of a tool's raw JSON result.
fn compact_output(tool: &str, res: &Value) -> String {
    match tool {
        "filesystem.list_dir" => {
            let Some(entries) = res.get("entries").and_then(|e| e.as_array()) else {
                return serde_json::to_string(res).unwrap_or_default();
            };
            if entries.is_empty() {
                return "(empty directory)".to_string();
            }
            entries
                .iter()
                .map(|e| {
                    let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let is_dir = e.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false);
                    if is_dir {
                        format!("{}/", name)
                    } else {
                        let size = e.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                        format!("{} ({} bytes)", name, size)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "filesystem.read" => res
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim_end()
            .to_string(),
        "filesystem.write" | "filesystem.edit" => match res.get("path").and_then(|p| p.as_str()) {
            Some(path) => format!("wrote {}", path),
            None => serde_json::to_string(res).unwrap_or_default(),
        },
        // Every field the tool returns. A list of names here once dropped
        // memory_total when the tool gained it.
        "system.info" => {
            let parts: Vec<String> = res
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some(format!("{}={}", k, s)),
                    Value::Null => None,
                    other => Some(format!("{}={}", k, other)),
                })
                .collect();
            if parts.is_empty() {
                serde_json::to_string(res).unwrap_or_default()
            } else {
                parts.join(", ")
            }
        }
        "shell.run" => {
            if res
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return format!(
                    "started in the background: {}",
                    res.get("command").and_then(|c| c.as_str()).unwrap_or("")
                );
            }
            let stdout = res
                .get("stdout")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim_end();
            let stderr = res
                .get("stderr")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            let status = res.get("status").and_then(|s| s.as_i64()).unwrap_or(0);
            let mut out = String::new();
            if !stdout.is_empty() {
                out.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("stderr: ");
                out.push_str(stderr);
            }
            if out.is_empty() {
                out.push_str(&format!("exit status {}", status));
            }
            out
        }
        "filesystem.search" => {
            let Some(matches) = res.get("matches").and_then(|m| m.as_array()) else {
                return serde_json::to_string(res).unwrap_or_default();
            };
            if matches.is_empty() {
                return format!(
                    "no matches ({} entries searched)",
                    res.get("scanned").and_then(|s| s.as_u64()).unwrap_or(0)
                );
            }
            let mut out = matches
                .iter()
                .map(|m| {
                    let path = m.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    match m.get("line").and_then(|l| l.as_u64()) {
                        Some(line) => format!("{}:{}", path, line),
                        None => path.to_string(),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if res
                .get("truncated")
                .and_then(|t| t.as_bool())
                .unwrap_or(false)
            {
                out.push_str("\n(results were capped)");
            }
            out
        }
        "filesystem.stat" => {
            let path = res.get("path").and_then(|p| p.as_str()).unwrap_or("?");
            let kind = if res.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false) {
                "directory"
            } else {
                "file"
            };
            format!(
                "{} is a {}, {} bytes",
                path,
                kind,
                res.get("size").and_then(|s| s.as_u64()).unwrap_or(0)
            )
        }
        "process.list" => {
            let Some(processes) = res.get("processes").and_then(|p| p.as_array()) else {
                return serde_json::to_string(res).unwrap_or_default();
            };
            processes
                .iter()
                .map(|p| {
                    format!(
                        "{} {} cpu={:.1}% mem={:.1}%",
                        p.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                        p.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                        p.get("cpu_percent").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        p.get("memory_percent")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => match res.as_object() {
            Some(map) if !map.is_empty() => {
                let parts: Vec<String> = map
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| format!("{}={}", k, s)))
                    .collect();
                if parts.is_empty() {
                    serde_json::to_string(res).unwrap_or_default()
                } else {
                    parts.join(", ")
                }
            }
            _ => serde_json::to_string(res).unwrap_or_default(),
        },
    }
}

/// Render the user-facing reply for a successful tool call. Deterministic:
/// every fact in the reply comes straight from the tool result.
fn render_tool_output(outcome: &ToolOutcome) -> String {
    if outcome.status == OutcomeStatus::Failed {
        return outcome.output.clone();
    }

    let mut out = String::new();
    match outcome.name.as_str() {
        "filesystem.list_dir" => {
            let path = outcome.args["path"].as_str().unwrap_or(".");
            let lines: Vec<&str> = outcome.output.lines().collect();
            out.push_str(&format!("Contents of {}:\n", path));
            for line in lines.iter().take(MAX_LIST_ENTRIES) {
                out.push_str(&format!("  {}\n", line));
            }
            if lines.len() > MAX_LIST_ENTRIES {
                out.push_str(&format!(
                    "  ... and {} more entries\n",
                    lines.len() - MAX_LIST_ENTRIES
                ));
            }
            out.push_str(&format!("\n{} entries.\n", lines.len()));
        }
        "filesystem.read" => {
            let path = outcome.args["path"].as_str().unwrap_or("");
            out.push_str(&format!("Contents of {}:\n", path));
            if outcome.output.chars().count() > MAX_READ_CHARS {
                out.push_str(&truncate(&outcome.output, MAX_READ_CHARS));
                out.push_str("\n... (truncated)\n");
            } else {
                out.push_str(&outcome.output);
                out.push('\n');
            }
        }
        "shell.run" => {
            let cmd = outcome.args["command"].as_str().unwrap_or("");
            let background = outcome.args["background"].as_bool().unwrap_or(false);
            if background {
                out.push_str(&format!("Started in the background: {}\n", cmd));
            } else {
                out.push_str(&format!("$ {}\n", cmd));
                if !outcome.output.is_empty() {
                    out.push_str(&outcome.output);
                    out.push('\n');
                }
            }
        }
        "system.info" => {
            out.push_str(&format!("System: {}\n", outcome.output));
        }
        "filesystem.search" => {
            let root = outcome.args["path"].as_str().unwrap_or(".");
            let res = outcome.result.as_ref();
            let matches = res
                .and_then(|r| r.get("matches"))
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();

            if matches.is_empty() {
                out.push_str(&format!("No matches under {}.\n", root));
                let scanned = res
                    .and_then(|r| r.get("scanned"))
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0);
                out.push_str(&format!("Searched {} entries.\n", scanned));
            } else {
                out.push_str(&format!("{} match(es) under {}:\n", matches.len(), root));
                for m in &matches {
                    let path = m.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    match (
                        m.get("line").and_then(|l| l.as_u64()),
                        m.get("preview").and_then(|p| p.as_str()),
                    ) {
                        (Some(line), Some(preview)) => {
                            out.push_str(&format!("  {}:{}\n      {}\n", path, line, preview));
                        }
                        _ => out.push_str(&format!("  {}\n", path)),
                    }
                }
                if res
                    .and_then(|r| r.get("truncated"))
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false)
                {
                    out.push_str(
                        "\nThe search stopped at its result limit; narrow it to see the rest.\n",
                    );
                }
            }
        }
        "filesystem.stat" => {
            let res = outcome.result.as_ref();
            let field = |key: &str| res.and_then(|r| r.get(key)).cloned().unwrap_or(Value::Null);
            let path = field("path");
            let path = path.as_str().unwrap_or("?");
            let kind = if field("is_dir").as_bool().unwrap_or(false) {
                "directory"
            } else {
                "file"
            };
            out.push_str(&format!("{}:\n", path));
            out.push_str(&format!("  type: {}\n", kind));
            out.push_str(&format!(
                "  size: {} bytes\n",
                field("size").as_u64().unwrap_or(0)
            ));
            if let Some(modified) = field("modified").as_str() {
                out.push_str(&format!("  modified: {}\n", modified));
            }
            if let Some(permissions) = field("permissions").as_str() {
                out.push_str(&format!("  permissions: {}\n", permissions));
            }
        }
        "process.list" => {
            let res = outcome.result.as_ref();
            let processes = res
                .and_then(|r| r.get("processes"))
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            if processes.is_empty() {
                out.push_str("Could not read the process list.\n");
            } else {
                out.push_str(&format!(
                    "{:<8} {:<28} {:>6} {:>6}\n",
                    "PID", "PROCESS", "CPU%", "MEM%"
                ));
                for p in &processes {
                    out.push_str(&format!(
                        "{:<8} {:<28} {:>6.1} {:>6.1}\n",
                        p.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                        p.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                        p.get("cpu_percent").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        p.get("memory_percent")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    ));
                }
                // `ps` reports CPU time averaged over the process lifetime.
                // Presenting that as current load would be a lie the user
                // could not detect from the numbers alone.
                out.push_str("\nCPU% is each process's lifetime average, not a live sample.\n");
            }
        }
        _ => {
            out.push_str(&format!("{}:\n", outcome.name));
            out.push_str(&outcome.output);
            out.push('\n');
        }
    }
    out
}

/// Compact, length-limited rendering of tool arguments.
fn compact_args(args: &Value) -> String {
    let s = serde_json::to_string(args).unwrap_or_default();
    truncate(&s, 300)
}

/// Truncate on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

/// Deterministic report used when at least one action was blocked.
///
/// This is the anti-hallucination guarantee: the model's free text is not
/// consulted at all for these turns.
fn render_blocked_report(trace: &[ToolOutcome]) -> String {
    let blocked: Vec<&ToolOutcome> = trace.iter().filter(|t| t.status.is_blocked()).collect();
    let done: Vec<&ToolOutcome> = trace.iter().filter(|t| !t.status.is_blocked()).collect();

    let mut out = String::new();
    if done.is_empty() {
        // Say *why* nothing ran. "Blocked by policy" is wrong when the user
        // themselves clicked no.
        let only_declined = blocked.iter().all(|t| t.status == OutcomeStatus::Declined);
        if only_declined {
            out.push_str("I did not run that - you declined the confirmation prompt.\n\n");
        } else {
            out.push_str("I did not run that - it was blocked by the permission policy.\n\n");
        }
    } else {
        out.push_str("I ran part of your request, but one action was not completed.\n\n");
    }

    for t in &blocked {
        out.push_str(&format!("  {}\n", t.name));
        if let Some(pattern) = &t.pattern {
            out.push_str(&format!("    matched: \"{}\"\n", pattern));
        }
        out.push_str(&format!("    args:   {}\n", compact_args(&t.args)));
        out.push_str(&format!("    status: {}\n", t.status.label()));
    }

    if !done.is_empty() {
        out.push_str("\nExecuted successfully:\n");
        for t in &done {
            out.push_str(&format!("  {}\n", t.name));
            out.push_str(&format!("    args: {}\n", compact_args(&t.args)));
            if !t.output.is_empty() {
                out.push_str(&format!("    {}\n", indent(&t.output, 4)));
            }
        }
    }

    // Only suggest loosening the policy when the policy is what stopped it,
    // and only at tool level: when a *pattern* denied the call, telling the
    // user to allow the whole tool is the opposite of useful advice.
    let tool_level: Vec<&ToolOutcome> = blocked
        .iter()
        .copied()
        .filter(|t| t.pattern.is_none() && t.status != OutcomeStatus::Declined)
        .collect();
    if !tool_level.is_empty() {
        let mut names: Vec<&str> = tool_level.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        out.push_str(&format!(
            "\nNo action was taken for the blocked item(s). To permit them, run:\n  jarvis permissions allow {}\n",
            names.join(" ")
        ));
    }
    out
}

/// Deterministic report used when the iteration budget runs out.
fn render_execution_report(trace: &[ToolOutcome]) -> String {
    if trace.is_empty() {
        return "I could not complete that request.".to_string();
    }

    let mut out = String::from("Here is what actually happened:\n\n");
    for t in trace {
        out.push_str(&format!("  {}\n", t.name));
        out.push_str(&format!("    args:   {}\n", compact_args(&t.args)));
        out.push_str(&format!("    status: {}\n", t.status.label()));
        if !t.output.is_empty() {
            out.push_str(&format!("    {}\n", indent(&t.output, 4)));
        }
    }
    out
}

/// Indent every line of `text` by `n` spaces.
fn indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", pad, l)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// If any action was blocked, render the deterministic report.
fn blocked_report(trace: &[ToolOutcome]) -> Option<String> {
    if trace.iter().any(|t| t.status.is_blocked()) {
        Some(render_blocked_report(trace))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn path_and_content() -> Vec<String> {
        vec!["path".into(), "content".into()]
    }

    fn turn(
        model: &str,
        provider: &str,
        tokens: Option<(u64, u64)>,
        remote: bool,
        redacted: usize,
    ) -> TurnReport {
        TurnReport {
            model_calls: u32::from(!model.is_empty()),
            model: model.into(),
            provider: provider.into(),
            prompt_tokens: tokens.map(|t| t.0),
            completion_tokens: tokens.map(|t| t.1),
            remote,
            redacted,
            ..Default::default()
        }
    }

    #[test]
    fn the_footer_says_what_answered_and_where() {
        assert_eq!(
            footer(&turn("qwen2.5:3b", "ollama", Some((1100, 134)), false, 0)),
            "qwen2.5:3b · local · 1.2k tokens"
        );
        assert_eq!(
            footer(&turn("gemma", "openai", Some((30_000, 500)), true, 2)),
            "gemma · remote: openai · 30k tokens · 2 redacted"
        );
        assert_eq!(footer(&turn("m", "p", None, false, 0)), "m · local");
    }

    #[test]
    fn a_deterministic_answer_says_no_model_was_used() {
        let mut report = turn("", "", None, false, 0);
        report.calls.push(ToolCallRecord {
            name: "system.info".into(),
            args: json!({}),
            status: "executed",
        });
        assert_eq!(footer(&report), "answered by system.info · no model");
    }

    #[test]
    fn every_system_info_field_reaches_the_answer() {
        // The renderer used to name hostname, os and arch, and silently
        // dropped memory_total when the tool started returning it.
        let res = json!({
            "hostname": "box", "os": "linux", "arch": "x86_64",
            "memory_total": "15.5 GiB", "cpu_threads": 4, "cpu_model": null
        });
        let out = compact_output("system.info", &res);
        for part in ["hostname=box", "memory_total=15.5 GiB", "cpu_threads=4"] {
            assert!(out.contains(part), "{} missing from {}", part, out);
        }
        assert!(
            !out.contains("cpu_model"),
            "null fields are left out: {}",
            out
        );
    }

    #[test]
    fn arguments_with_every_required_key_are_left_alone() {
        let args = json!({"path": "/tmp/a", "content": "hi"});
        assert_eq!(
            unwrap_echoed_schema(args.clone(), &path_and_content()),
            args
        );
    }

    #[test]
    fn arguments_nested_in_parameters_are_unwrapped() {
        let args = json!({"type": "function", "parameters": {"path": "/tmp/a", "content": "hi"}});
        assert_eq!(
            unwrap_echoed_schema(args, &path_and_content()),
            json!({"path": "/tmp/a", "content": "hi"})
        );
    }

    #[test]
    fn arguments_nested_in_properties_are_unwrapped() {
        // llama3.2:1b, eval case fs.write.approved.
        let args = json!({
            "properties": {"content": "hello", "path": "/tmp/notes.md"},
            "required": ["path", "content"],
            "type": "object"
        });
        assert_eq!(
            unwrap_echoed_schema(args, &path_and_content()),
            json!({"content": "hello", "path": "/tmp/notes.md"})
        );
    }

    #[test]
    fn an_echoed_schema_without_values_is_not_unwrapped() {
        // Same model, next call: the schema itself, no values. Unwrapping it
        // would hand the tool `{"type": "string"}` as a path.
        let args = json!({
            "properties": {
                "path": {"description": "File path", "type": "string"},
                "content": {"description": "Text to write", "type": "string"}
            },
            "required": ["path", "content"],
            "type": "object"
        });
        assert_eq!(
            unwrap_echoed_schema(args.clone(), &path_and_content()),
            args
        );
    }

    #[test]
    fn the_first_url_is_the_top_result() {
        let output = "Backend: wikipedia\nQuery: rust\n\n1. Rust\n   https://en.wikipedia.org/wiki/Rust\n   text\n2. Other\n   https://other.example\n";
        assert_eq!(
            first_url(output).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust")
        );
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_url() {
        let output = "see https://example.com/page.";
        assert_eq!(
            first_url(output).as_deref(),
            Some("https://example.com/page")
        );
    }

    #[test]
    fn a_url_containing_parentheses_is_kept_whole() {
        let output = "1. Rust\n   https://en.wikipedia.org/wiki/Rust_(programming_language)\n";
        assert_eq!(
            first_url(output).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(programming_language)")
        );
    }

    #[test]
    fn a_markdown_wrapper_is_trimmed() {
        let output = "[result](https://example.com/page)";
        assert_eq!(
            first_url(output).as_deref(),
            Some("https://example.com/page")
        );
    }

    #[test]
    fn text_without_a_url_yields_none() {
        assert_eq!(first_url("no links here"), None);
        assert_eq!(first_url("https://"), None);
    }

    #[test]
    fn a_recall_block_is_separated_from_the_instructions() {
        let with = append_recall("Be brief.".to_string(), Some("Earlier: x".to_string()));
        assert_eq!(with, "Be brief.\nEarlier: x");
    }

    #[test]
    fn no_recall_leaves_the_prompt_untouched() {
        assert_eq!(append_recall("Be brief.".to_string(), None), "Be brief.");
    }

    #[test]
    fn a_request_with_a_scheme_already_names_its_url() {
        assert!(input_names_url("what does https://example.com say"));
        assert!(input_names_url("read HTTP://EXAMPLE.COM"));
        assert!(!input_names_url("what is the latest version of rust"));
    }
}
