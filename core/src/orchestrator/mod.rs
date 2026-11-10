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

mod execute;
mod gateway;
mod outcome;
mod prompt;
mod render;
mod turn;

use execute::*;
use gateway::*;
use render::*;
use turn::*;

use outcome::*;
pub use outcome::{footer, route_label, ToolCallRecord, TurnReport};
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
}

pub use jarvis_common::ipc::Choice;

/// The name under which sending to a remote provider is confirmed. Not a
/// real tool: it reuses the confirmation path every client already has.
pub const EGRESS_TOOL: &str = "egress.send";

/// Asked when a remote request was not sent: answer with the local model?
pub const EGRESS_LOCAL_TOOL: &str = "egress.local";

/// Asked before a web call carries something this turn may not send.
pub const EGRESS_WEB_TOOL: &str = "egress.web";

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
    /// Sources the owner trusted with `[t]`, per conversation.
    trust: SessionTrust,
    /// Copies of files before Jarvis changes them. `None` if the folder
    /// could not be opened: writes still happen, and prompts say there is
    /// no copy.
    undo: Option<UndoStore>,
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
        tools.register(Box::new(DeleteFileTool));
        tools.register(Box::new(RunShellTool {
            work_dir: config.tools.work_dir.clone(),
            sandbox: crate::sandbox::ready(&config)
                .then(|| crate::sandbox::Sandbox::for_config(&config)),
            key_vars: crate::sandbox::key_vars(&config),
        }));
        tools.register(Box::new(GetSystemInfoTool));
        tools.register(Box::new(ListProcessesTool));
        let session_id = Uuid::new_v4().to_string();
        let undo = match UndoStore::for_config(&config) {
            Ok(store) => Some(store),
            Err(e) => {
                warn!("undo is unavailable: {}", e);
                None
            }
        };
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
            trust: SessionTrust::default(),
            undo,
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

    /// What this conversation trusts (`[t]`), for `/trust`.
    pub fn trusted(&self) -> Vec<String> {
        self.trust.list(&self.session_id)
    }

    /// Forget this conversation's trust, for `/trust clear`.
    pub fn clear_trust(&self) {
        self.trust.clear(&self.session_id)
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
            Route::Direct if router::asks_to_undo(input) => UNDO_FROM_CHAT.to_string(),
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
                    .execute_call(name.clone(), args.clone(), confirm, &[], &rec)
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
}

#[cfg(test)]
mod tests;
