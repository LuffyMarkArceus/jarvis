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
use crate::config::{Config, ModelConfig, Spill};
use crate::context::{self, ContextPlan};
use crate::conversation::{Conversation, LastTurn, Pinned};
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
use crate::taint::{self, SessionTrust, Source};
use crate::tools::{
    filesystem::{
        DeleteFileTool, EditFileTool, ListDirTool, ReadFileTool, SearchFilesTool, StatFileTool,
        WriteFileTool,
    },
    parser,
    shell::RunShellTool,
    system::{GetSystemInfoTool, ListProcessesTool},
    ToolRegistry,
};
use crate::undo::UndoStore;
use crate::verify;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use uuid::Uuid;

mod autopilot;
mod execute;
mod gateway;
mod outcome;
mod prompt;
mod render;
mod turn;
mod turnlog;

use execute::*;
use gateway::*;
use render::*;
use turn::*;
use turnlog::*;

pub use autopilot::Approve;
use outcome::*;
pub use outcome::{
    footer, route_label, ModelTiming, ToolCallRecord, ToolTiming, TurnReport, TurnTiming,
};
pub use render::REPORT_OPENINGS;

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

    /// The whole answer, a reason included. A handler that cannot give a
    /// reason answers through `confirm` or `choose`.
    async fn answer(&self, tool: &str, args: &Value, choices: &[Choice]) -> Answer {
        let choice = if choices.is_empty() {
            self.confirm(tool, args).await.then_some('y')
        } else {
            self.choose(tool, args, choices).await
        };
        Answer {
            approved: choice.is_some(),
            choice,
            reason: None,
        }
    }
}

pub use jarvis_common::ipc::Choice;

/// An answer to a prompt: yes or no, which choice, and - with a no - why
/// (6.7b), which reaches the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub approved: bool,
    pub choice: Option<char>,
    pub reason: Option<String>,
}

/// The name under which sending to a remote provider is confirmed. Not a
/// real tool: it reuses the confirmation path every client already has.
pub const EGRESS_TOOL: &str = "egress.send";

/// Asked when a remote request was not sent: answer with the local model?
pub const EGRESS_LOCAL_TOOL: &str = "egress.local";

/// Asked, with `[remote] spill = "ask"`, before a call the busy GPU would
/// hold up goes to the remote tier instead.
pub const EGRESS_SPILL_TOOL: &str = "egress.spill";

/// Asked before a web call carries something this turn may not send.
pub const EGRESS_WEB_TOOL: &str = "egress.web";

/// A model `/model` can switch to.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelChoice {
    pub name: String,
    /// On the `[remote]` tier.
    pub remote: bool,
    /// The model answering this conversation now.
    pub current: bool,
}

pub struct Orchestrator {
    config: Config,
    /// The `[model]` tier: local unless configured otherwise.
    client: Arc<dyn Provider>,
    /// The `[remote]` tier, when configured.
    remote: Option<Arc<dyn Provider>>,
    tools: ToolRegistry,
    memory: Option<MemoryManager>,
    /// Sources the owner trusted with `[t]`, per conversation.
    trust: SessionTrust,
    /// Copies of files before Jarvis changes them. `None` if the folder
    /// could not be opened: writes still happen, and prompts say there is
    /// no copy.
    undo: Option<UndoStore>,
    permissions: PermissionManager,
    mcp: McpClient,
    /// The queue every local model call waits in (6.5b).
    lane: Arc<crate::lane::LocalLane>,
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
        tools.register(Box::new(DeleteFileTool));
        tools.register(Box::new(RunShellTool {
            work_dir: config.tools.work_dir.clone(),
            sandbox: crate::sandbox::ready(&config)
                .then(|| crate::sandbox::Sandbox::for_config(&config)),
            key_vars: crate::sandbox::key_vars(&config),
        }));
        tools.register(Box::new(GetSystemInfoTool));
        tools.register(Box::new(ListProcessesTool));
        let undo = match UndoStore::for_config(&config) {
            Ok(store) => Some(store),
            Err(e) => {
                warn!("undo is unavailable: {}", e);
                None
            }
        };
        let memory = MemoryManager::new(config.clone()).ok();
        let config_slots = config.model.slots;
        Self {
            config,
            client,
            remote: None,
            tools,
            memory,
            trust: SessionTrust::default(),
            undo,
            permissions,
            mcp: McpClient::new(),
            lane: crate::lane::LocalLane::new(config_slots),
        }
    }

    /// The local lane, for status and tests.
    pub fn lane(&self) -> &Arc<crate::lane::LocalLane> {
        &self.lane
    }

    /// An orchestrator is shared by every turn at once (6.5b).
    #[allow(dead_code)]
    const SHARED: fn() = || {
        fn shared<T: Send + Sync>() {}
        shared::<Orchestrator>();
    };

    /// Attach the `[remote]` tier's provider. New sessions start on it only
    /// if `[remote] default = true`.
    pub fn with_remote(mut self, client: Option<Arc<dyn Provider>>) -> Self {
        self.remote = client;
        self
    }

    /// A new conversation, with its `sessions` row: on the remote tier if
    /// `[remote] default` is set and a remote tier exists.
    pub fn new_conversation(&self) -> Arc<Conversation> {
        let id = Uuid::new_v4().to_string();
        if let Some(ref m) = self.memory {
            let _ = m.create_session(&id);
        }
        Conversation::new(id, self.starts_remote())
    }

    /// The files conversation `session` changed since `since` (6.7b).
    pub fn turn_changes(
        &self,
        session: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Vec<jarvis_common::ipc::FileChange> {
        match &self.undo {
            Some(store) => crate::changes::turn_changes(store, session, since),
            None => Vec::new(),
        }
    }

    /// The mode a new conversation starts in (`[modes]`).
    pub fn default_mode(&self) -> crate::modes::Mode {
        crate::modes::Mode::from_config(&self.config.modes)
    }

    /// Whether a new conversation starts on the remote tier: only when one
    /// exists and `[remote] default` is set.
    pub fn starts_remote(&self) -> bool {
        self.remote.is_some() && self.config.remote.as_ref().is_some_and(|r| r.default)
    }

    /// Switch `conv` between the local and remote tier.
    pub fn set_remote(&self, conv: &Conversation, on: bool) -> Result<()> {
        if on && self.remote.is_none() {
            return Err(JarvisError::Config(
                "no remote model is configured: add a [remote] section to config.toml".into(),
            ));
        }
        conv.set_remote(on);
        self.warm_up(conv);
        Ok(())
    }

    pub fn uses_remote(&self, conv: &Conversation) -> bool {
        self.on_remote(&conv.pin())
    }

    fn on_remote(&self, settings: &Pinned) -> bool {
        settings.remote && self.remote.is_some()
    }

    /// The conversation's tier now: for settings and status. A turn uses
    /// [`Orchestrator::turn_tier`], fixed when it started.
    fn tier(&self, conv: &Conversation) -> (ModelConfig, Arc<dyn Provider>) {
        self.tier_of(&conv.pin())
    }

    /// The tier a turn's requests go to.
    fn turn_tier(&self, rec: &TurnRecorder<'_>) -> (ModelConfig, Arc<dyn Provider>) {
        self.tier_of(&rec.settings())
    }

    /// The model settings and provider `settings` name, with the model
    /// `/model` chose.
    fn tier_of(&self, settings: &Pinned) -> (ModelConfig, Arc<dyn Provider>) {
        let remote = self.on_remote(settings);
        let (cfg, client) = match (&self.config.remote, &self.remote) {
            (Some(cfg), Some(client)) if remote => (&cfg.model, client),
            _ => (&self.config.model, &self.client),
        };
        (with_choice(settings, cfg, remote), client.clone())
    }

    /// The model to offer when a remote request was not sent: `[model]`,
    /// if this turn is on the remote tier and `[model]` is local.
    fn local_fallback(&self, rec: &TurnRecorder<'_>) -> Option<(ModelConfig, Arc<dyn Provider>)> {
        let settings = rec.settings();
        (self.on_remote(&settings) && !destination_of(&self.config.model).is_remote()).then(|| {
            (
                with_choice(&settings, &self.config.model, false),
                self.client.clone(),
            )
        })
    }

    /// Load the local model this conversation uses, in the background, so
    /// the next question does not wait for it (measured: 25-42 s). Nothing
    /// on the remote tier; without a runtime (a plain test), nothing.
    pub fn warm_up(&self, conv: &Conversation) {
        // Loading another model while every slot is in use would evict the
        // one a turn is using.
        if self.uses_remote(conv) || self.lane.available() == 0 {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (cfg, client) = self.tier(conv);
        let options = self.model_options(&cfg);
        runtime.spawn(async move {
            if let Err(e) = client.warm(&cfg.default_model, &options).await {
                warn!("could not load {} ahead of time: {}", cfg.default_model, e);
            }
        });
    }

    /// "open X" from the router is a guess that X is an app. Confirm it from
    /// the installed apps and launch the app's own command ("vscode" runs
    /// `code`); if X is no app ("episode 001 of ..."), it is something to
    /// find and open, which is the model's job.
    fn resolve_launch(&self, route: Route) -> Route {
        let Route::Tool { name, args } = &route else {
            return route;
        };
        if name != "shell.run" || args["background"].as_bool() != Some(true) {
            return route;
        }
        let Some(wanted) = args["command"].as_str() else {
            return route;
        };
        let dirs = self
            .config
            .tools
            .app_dirs
            .clone()
            .unwrap_or_else(crate::apps::app_dirs);
        match crate::apps::find(&dirs, wanted) {
            Some(app) => {
                let mut args = args.clone();
                args["command"] = json!(app.command);
                Route::Tool {
                    name: name.clone(),
                    args,
                }
            }
            None => Route::Model,
        }
    }

    /// Every model `/model` can switch to: each tier's `default_model`, then
    /// its `models`, local first. `current` marks the one answering now.
    pub fn models(&self, conv: &Conversation) -> Vec<ModelChoice> {
        let mut tiers = vec![(&self.config.model, false)];
        if let (Some(cfg), Some(_)) = (&self.config.remote, &self.remote) {
            tiers.push((&cfg.model, true));
        }
        let now = self.tier(conv).0.default_model;
        let on_remote = self.uses_remote(conv);
        let mut out: Vec<ModelChoice> = Vec::new();
        for (cfg, remote) in tiers {
            for name in std::iter::once(&cfg.default_model).chain(cfg.models.iter()) {
                if out.iter().any(|m| m.remote == remote && &m.name == name) {
                    continue;
                }
                out.push(ModelChoice {
                    name: name.clone(),
                    remote,
                    current: remote == on_remote && *name == now,
                });
            }
        }
        out
    }

    /// Switch this conversation to the model `query` names: its exact name,
    /// or a part of exactly one name, any case. The model's tier comes with
    /// it. A name that is not configured, or matches several, changes
    /// nothing.
    pub fn set_model(&self, conv: &Conversation, query: &str) -> Result<ModelChoice> {
        let models = self.models(conv);
        let q = query.trim().to_lowercase();
        let exact: Vec<&ModelChoice> = models
            .iter()
            .filter(|m| m.name.to_lowercase() == q)
            .collect();
        let matches: Vec<&ModelChoice> = if !exact.is_empty() {
            exact
        } else {
            models
                .iter()
                .filter(|m| !q.is_empty() && m.name.to_lowercase().contains(&q))
                .collect()
        };
        let names = |list: &[&ModelChoice]| {
            list.iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let chosen = match matches.as_slice() {
            [one] => (*one).clone(),
            [] => {
                let all: Vec<&ModelChoice> = models.iter().collect();
                return Err(JarvisError::Config(format!(
                    "No model matches \"{}\". Available: {}. To add one, list it in `models = [...]` under [model] or [remote] in config.toml.",
                    query.trim(),
                    names(&all)
                )));
            }
            several => {
                return Err(JarvisError::Config(format!(
                    "\"{}\" matches several models: {}. Type more of the name.",
                    query.trim(),
                    names(several)
                )))
            }
        };
        conv.choose(chosen.remote, chosen.name.clone());
        self.set_remote(conv, chosen.remote)?;
        Ok(ModelChoice {
            current: true,
            ..chosen
        })
    }

    /// Start the configured MCP servers and wait until discovery finishes.
    ///
    /// A web eval case that ran before discovery would be planned without
    /// the search tools and fail for a reason unrelated to the model.
    pub async fn start_mcp(&self) -> Result<()> {
        self.mcp.start_from_config(&self.config).await
    }

    /// The provider and model a turn will be sent to, for `jarvis status`.
    pub fn model_info(&self, conv: &Conversation) -> (&'static str, String, String) {
        let (cfg, client) = self.tier(conv);
        (client.name(), cfg.default_model, cfg.base_url)
    }

    /// Where this orchestrator's requests go. A config that cannot be read
    /// was already rejected at startup (`provider::from_config`); if one gets
    /// here anyway, it is treated as remote with an undeclared policy, which
    /// asks rather than sends.
    pub fn destination(&self, conv: &Conversation) -> Destination {
        destination_of(&self.tier(conv).0)
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

    /// What this conversation trusts (`[t]`), for `/trust`.
    pub fn trusted(&self, conv: &Conversation) -> Vec<String> {
        self.trust.list(&conv.id)
    }

    /// Forget this conversation's trust, for `/trust clear`.
    pub fn clear_trust(&self, conv: &Conversation) {
        self.trust.clear(&conv.id)
    }

    /// One turn's answer and its footer, for clients that show both.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_footer(
        &self,
        conv: &Conversation,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        progress: Option<&UnboundedSender<String>>,
        steps: Option<&UnboundedSender<jarvis_common::ipc::StepEvent>>,
    ) -> Result<(String, String, TurnTiming)> {
        let report = self
            .run_turn_observed(conv, input, confirm, cancel, sink, progress, steps)
            .await?;
        let footer = footer(&report);
        Ok((report.answer, footer, report.timing))
    }

    /// Handle one user turn and report what happened in it, not just the
    /// answer. The eval harness grades this report.
    pub async fn run_turn(
        &self,
        conv: &Conversation,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
    ) -> Result<TurnReport> {
        self.run_turn_observed(conv, input, confirm, cancel, sink, None, None)
            .await
    }

    /// [`Self::run_turn`], also saying on `progress` what the turn is doing
    /// now - "asking nemotron (step 3)", "searching the web" - so a client
    /// can tell a long turn from a stuck one.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn_observed(
        &self,
        conv: &Conversation,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        progress: Option<&UnboundedSender<String>>,
        steps: Option<&UnboundedSender<jarvis_common::ipc::StepEvent>>,
    ) -> Result<TurnReport> {
        // Measured here, around everything, so every way a turn ends -
        // answered, cancelled, failed - is logged with its timing.
        let started = std::time::Instant::now();
        let mut rec = TurnRecorder::new(conv);
        rec.progress = progress.cloned();
        rec.steps = steps.cloned();
        // Autopilot never prompts: whatever would, is answered no and listed.
        let no_prompts = autopilot::NoPrompts {
            not_done: &rec.not_done,
        };
        let confirm = if rec.pinned.mode.autonomy == crate::modes::Autonomy::Autopilot {
            Some(&no_prompts as &dyn ConfirmationHandler)
        } else {
            confirm
        };
        let timed = confirm.map(TimedConfirm::new);
        let confirm = timed.as_ref().map(|t| t as &dyn ConfirmationHandler);
        let result = self
            .run_turn_inner(conv, input, confirm, cancel, sink, &rec)
            .await;
        let you_ms = timed.as_ref().map_or(0, TimedConfirm::waited_ms);
        let total_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok((answer, plan)) => {
                let mut report = rec.finish(answer, &plan);
                report.timing.total_ms = total_ms;
                report.timing.you_ms = you_ms;
                let found = self.turn_signals(&report);
                self.log_turn(conv, input, &report, found);
                Ok(report)
            }
            Err(e) => {
                let planned = rec.planned.lock().unwrap().clone();
                let mut report = rec.finish_unplanned(planned);
                report.timing.total_ms = total_ms;
                report.timing.you_ms = you_ms;
                let mut found = self.turn_signals(&report);
                found.push(if matches!(e, JarvisError::Cancelled) {
                    "cancelled"
                } else {
                    "error"
                });
                self.log_turn(conv, input, &report, found);
                Err(e)
            }
        }
    }

    /// One turn, from request to answer. [`Self::run_turn`] measures and
    /// logs it.
    async fn run_turn_inner(
        &self,
        conv: &Conversation,
        input: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        rec: &TurnRecorder<'_>,
    ) -> Result<(String, TurnPlan)> {
        if cancel.is_some_and(Cancellation::is_cancelled) {
            return Err(JarvisError::Cancelled);
        }

        // Bound the log line: a pasted document should not fill the log.
        info!("Processing: {}", context::truncate_with_marker(input, 200));

        // What the last turn left for /approve is about that turn, not this
        // one (6.6).
        conv.set_deferred(Vec::new());
        // Steers are this turn's; the window collects the last turn's first.
        conv.take_used_steers();

        // Snapshot history *before* persisting the new user message so the
        // prompt never contains the request twice.
        let history = self.history_for_prompt(conv);

        if let Some(ref m) = self.memory {
            let _ = m.add_labeled_message(&conv.id, "user", input, Some(&Label::of(Origin::User)));
        }
        rec.saw(&Label::of(Origin::User));

        let route = self.resolve_launch(router::route(input));
        // One decision, one owner: the planner narrows the route and produces
        // the exact tool set this turn may use. Both the provider request and
        // the system prompt are rendered from `plan.tools`, so the model can
        // never be told about a tool it cannot call.
        let mut plan = planner::plan(input, route, &self.tools, &self.mcp.list_tools());
        // Plan mode is offered only what looks: the shell too, read-only,
        // where the sandbox runs (6.6).
        let mode = rec.pinned.mode;
        if mode.autonomy == crate::modes::Autonomy::Plan {
            let read_only = mode.sandbox && crate::sandbox::ready(&self.config);
            plan.tools.retain(|t| {
                if t.name == "shell.run" {
                    read_only
                } else {
                    !taint::is_action(&t.name)
                }
            });
        }
        info!("route: {:?} tools: {:?}", plan.route, plan.names());
        *rec.planned.lock().unwrap() = Some((
            route_label(&plan.route),
            plan.names().into_iter().map(String::from).collect(),
        ));

        let answer = match &plan.route {
            // Pure conversation: no tools involved at all.
            Route::Direct if router::asks_to_undo(input) => UNDO_FROM_CHAT.to_string(),
            Route::Direct => {
                self.converse(input, &history, confirm, cancel, sink, rec)
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
                // A command the owner typed has no model to say whether it
                // writes: it counts as writing (6.6).
                let mut args = args.clone();
                if name == "shell.run" {
                    args["writes"] = json!(true);
                }
                let outcome = self
                    .execute_call(name.clone(), args, confirm, &[], rec)
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
                self.model_with_tools(input, &history, &plan, confirm, cancel, rec)
                    .await?
            }
        };

        // A model copying our untrusted marker is never shown, and one that
        // wraps made-up output in it, with no tool run, is called out.
        let ran = rec
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.status == "executed");
        let mut answer = taint::scrub_markers(&answer, ran);
        if rec.pinned.mode.autonomy == crate::modes::Autonomy::Autopilot {
            let not_done = rec.not_done.lock().unwrap().clone();
            if let Some(report) = autopilot::report(&conv.deferred(), &not_done) {
                answer = format!("{}\n\n{}", answer.trim_end(), report.trim_end());
            }
        }
        let model = rec
            .used
            .lock()
            .unwrap()
            .as_ref()
            .map(|(model, _)| model.clone());
        self.persist_assistant(conv, &answer, &rec.label(), model.as_deref());
        Ok((answer, plan))
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
}

#[cfg(test)]
mod tests;

/// `cfg` with the model chosen for its tier.
fn with_choice(settings: &Pinned, cfg: &ModelConfig, remote: bool) -> ModelConfig {
    let mut cfg = cfg.clone();
    if let Some(model) = settings.chosen[remote as usize].clone() {
        cfg.default_model = model;
    }
    cfg
}
