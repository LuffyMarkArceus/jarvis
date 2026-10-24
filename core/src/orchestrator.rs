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
use crate::config::Config;
use crate::context::{self, ContextPlan};
use crate::error::{JarvisError, Result};
use crate::mcp::client::McpClient;
use crate::memory::{MemoryManager, Message};
use crate::model::{ChatMessage, ModelToolCall, OllamaClient};
use crate::permissions::{PermissionAction, PermissionManager};
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
use serde_json::Value;
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

pub struct Orchestrator {
    config: Config,
    client: OllamaClient,
    tools: ToolRegistry,
    memory: Option<MemoryManager>,
    session_id: String,
    permissions: PermissionManager,
    mcp: McpClient,
}

impl Orchestrator {
    pub fn new() -> Self {
        Self::with_config(Config::load())
    }

    pub fn with_config(config: Config) -> Self {
        let client = OllamaClient::new(&config.model.base_url);
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
        let permissions = PermissionManager::new(&config).expect("Failed to init permissions");
        let mcp = McpClient::new();
        // Start MCP servers in the background: a slow or broken server must
        // not delay the first prompt, and discovery results simply appear in
        // the cache when they are ready.
        let mcp_startup = mcp.clone();
        let cfg_clone = config.clone();
        tokio::spawn(async move {
            if let Err(e) = mcp_startup.start_from_config(&cfg_clone).await {
                warn!("MCP startup failed: {}", e);
            }
        });
        Self {
            config,
            client,
            tools,
            memory,
            session_id,
            permissions,
            mcp,
        }
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
        if cancel.is_some_and(Cancellation::is_cancelled) {
            return Err(JarvisError::Cancelled);
        }

        // Bound the log line: a pasted document should not fill the log.
        info!("Processing: {}", context::truncate_with_marker(input, 200));

        // Snapshot history *before* persisting the new user message so the
        // prompt never contains the request twice.
        let history = self.history_for_prompt();

        if let Some(ref m) = self.memory {
            let _ = m.add_message(&self.session_id, "user", input);
        }

        let route = router::route(input);
        info!("route: {:?}", route);

        let answer = match route {
            // Pure conversation: no tools involved at all.
            Route::Direct => self.converse(input, &history, cancel, sink).await?,

            // A confidently identified tool call: run it and report the facts.
            Route::Tool { name, args } => {
                if cancel.is_some_and(Cancellation::is_cancelled) {
                    return Err(JarvisError::Cancelled);
                }
                // The tool is the whole turn: once it has run, its result is
                // the answer even if the user has just cancelled. Dropping it
                // would hide an action that really happened.
                let outcome = self.execute_call(name, args, confirm).await;
                // A confirmation that was denied *because* the user cancelled
                // is not a tool result worth persisting: the turn never ran.
                // If the tool really ran, its report is still the answer.
                if cancel.is_some_and(Cancellation::is_cancelled) && outcome.status.is_blocked() {
                    return Err(JarvisError::Cancelled);
                }
                self.answer_from_outcome(&outcome)
            }

            // Ambiguous: let the model pick from the catalogue.
            Route::Model => {
                self.model_with_tools(input, &history, confirm, cancel)
                    .await?
            }
        };

        self.persist_assistant(&answer);
        Ok(answer)
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
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
    ) -> Result<String> {
        let plan = context::assemble(
            &self.conversation_system_prompt(),
            input,
            history,
            &self.config.context,
            0,
        );
        self.log_plan(&plan);
        let options = self.model_options();
        let result = match sink {
            Some(sink) => {
                self.client
                    .chat_turn_streaming(
                        &self.config.model.default_model,
                        Some(&plan.system),
                        &plan.messages,
                        None,
                        Some(options),
                        cancel,
                        sink,
                    )
                    .await
            }
            None => {
                self.client
                    .chat_turn_cancellable(
                        &self.config.model.default_model,
                        Some(&plan.system),
                        &plan.messages,
                        None,
                        Some(options),
                        cancel,
                    )
                    .await
            }
        };
        let turn = match result {
            Ok(turn) => turn,
            // A provider failure has a deterministic fallback; a user
            // cancelling the turn must not turn into one.
            Err(JarvisError::Cancelled) => return Err(JarvisError::Cancelled),
            Err(e) => {
                info!("Model unavailable: {}, using fallback", e);
                return Ok(self.fallback_answer(input));
            }
        };
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
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
    ) -> Result<String> {
        let lower = input.to_lowercase();
        let mut tools = Vec::new();

        // Always include system info for general queries, but filter aggressively
        let need_fs = lower.contains("file")
            || lower.contains("dir")
            || lower.contains("list")
            || lower.contains("read")
            || lower.contains("write")
            || lower.contains("edit")
            || lower.contains("search")
            || lower.contains("/")
            || lower.contains("~");
        let need_shell = lower.contains("run")
            || lower.contains("exec")
            || lower.contains("command")
            || lower.contains("install")
            || lower.contains("open ")
            || lower.contains("launch");
        let need_process = lower.contains("process")
            || lower.contains("cpu")
            || lower.contains("memory")
            || lower.contains("ram");
        let need_fetch = lower.contains("fetch")
            || lower.contains("http")
            || lower.contains("url")
            || lower.contains("website")
            || lower.contains("download")
            || lower.contains("wikipedia");
        let need_system = lower.contains("os")
            || lower.contains("system")
            || lower.contains("hostname")
            || lower.contains("arch")
            || lower.contains("spec");

        if need_fs {
            for t in &self.tools.list() {
                if t.starts_with("filesystem.") {
                    // Find the tool schema
                }
            }
            // Add native filesystem tools
            for tool_name in [
                "filesystem.list_dir",
                "filesystem.read",
                "filesystem.search",
                "filesystem.stat",
                "filesystem.write",
                "filesystem.edit",
            ] {
                if let Some(schema) = self
                    .tools
                    .schemas()
                    .iter()
                    .find(|s| s["function"]["name"].as_str() == Some(tool_name))
                {
                    tools.push(schema.clone());
                }
            }
            // Add relevant MCP filesystem tools
            for t in self.mcp.list_tools() {
                if t.server == "filesystem"
                    && (t.name.contains("read")
                        || t.name.contains("list")
                        || t.name.contains("search")
                        || t.name.contains("write")
                        || t.name.contains("edit")
                        || t.name == "get_file_info")
                {
                    tools.push(serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.full_name,
                            "description": t.description,
                            "parameters": t.schema,
                        }
                    }));
                }
            }
        }
        if need_shell {
            if let Some(schema) = self
                .tools
                .schemas()
                .iter()
                .find(|s| s["function"]["name"].as_str() == Some("shell.run"))
            {
                tools.push(schema.clone());
            }
        }
        if need_process {
            if let Some(schema) = self
                .tools
                .schemas()
                .iter()
                .find(|s| s["function"]["name"].as_str() == Some("process.list"))
            {
                tools.push(schema.clone());
            }
        }
        if need_fetch {
            for t in self.mcp.list_tools() {
                if t.server == "fetch" {
                    tools.push(serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.full_name,
                            "description": t.description,
                            "parameters": t.schema,
                        }
                    }));
                }
            }
        }
        if need_system {
            if let Some(schema) = self
                .tools
                .schemas()
                .iter()
                .find(|s| s["function"]["name"].as_str() == Some("system.info"))
            {
                tools.push(schema.clone());
            }
        }

        // If no specific tools needed, include minimal set
        if tools.is_empty() {
            if let Some(schema) = self
                .tools
                .schemas()
                .iter()
                .find(|s| s["function"]["name"].as_str() == Some("system.info"))
            {
                tools.push(schema.clone());
            }
        }
        let reserved = self.tool_schema_tokens(&tools);
        let plan = context::assemble(
            &self.tool_system_prompt(),
            input,
            history,
            &self.config.context,
            reserved,
        );
        self.log_plan(&plan);

        let system = plan.system;
        let mut messages = plan.messages;
        // How many leading messages are history. Everything after them is the
        // current request and the tool trace, which must never be dropped.
        let mut history_len = messages.len().saturating_sub(1);
        let mut trace: Vec<ToolOutcome> = Vec::new();

        for _ in 0..MAX_ITERATIONS {
            if cancel.is_some_and(Cancellation::is_cancelled) {
                return self.cancelled_result(&trace);
            }

            // Tool results grow the prompt. Shed the oldest history until it
            // fits again rather than letting the provider truncate the front.
            self.fit_prompt(&system, &mut messages, &mut history_len, reserved);

            let result = self
                .client
                .chat_turn_cancellable(
                    &self.config.model.default_model,
                    Some(&system),
                    &messages,
                    Some(&tools),
                    Some(self.model_options()),
                    cancel,
                )
                .await;
            let turn = match result {
                Ok(t) => t,
                Err(JarvisError::Cancelled) => return self.cancelled_result(&trace),
                Err(e) => {
                    info!("Model unavailable: {}, using fallback", e);
                    return Ok(self.fallback_answer(input));
                }
            };
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
                    let outcome = self.execute_native_call(call, confirm).await;
                    messages.push(ChatMessage::tool_result(
                        &outcome.name,
                        &self.prompt_tool_output(&outcome.output),
                    ));
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
                    let outcome = self.execute_text_call(call, confirm).await;
                    messages.push(ChatMessage::text(
                        "user",
                        &format!(
                            "Result of {}:\n{}",
                            outcome.name,
                            self.prompt_tool_output(&outcome.output)
                        ),
                    ));
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
            return Ok(answer);
        }

        Ok(render_execution_report(&trace))
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

    // ---------------------------------------------------------------- prompts

    /// Provider tuning options for a single request.
    ///
    /// `num_ctx` is set explicitly instead of relying on the provider's
    /// default, which changes between versions and is what decides how much
    /// of the prompt survives.
    fn model_options(&self) -> Value {
        serde_json::json!({
            "temperature": self.config.model.temperature,
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
    fn tool_system_prompt(&self) -> String {
        let mut p = self.conversation_system_prompt();
        p.push_str("- Only report actions that appear in the tool results. Never claim an action succeeded unless it appears there.\n\n");
        p.push_str("Tools you can call:\n");
        for (name, desc) in self.tools.describe() {
            p.push_str(&format!("- {}: {}\n", name, desc));
        }
        // MCP tools are listed under the same names the model must call.
        for t in self.mcp.list_tools() {
            p.push_str(&format!("- {}: {}\n", t.full_name, t.description));
        }
        p
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

    fn fallback_answer(&self, input: &str) -> String {
        format!(
            "I could not reach the local model, so I cannot answer that yet.\n\n\
             You asked: {}\n\n\
             Tools available: {}",
            input,
            self.tools.list().join(", ")
        )
    }

    fn empty_answer(&self, input: &str) -> String {
        format!(
            "I did not get a usable answer back from the model for: {}\n\n\
             Try rephrasing, or run `jarvis status` to check the model connection.",
            input
        )
    }

    fn persist_assistant(&self, content: &str) {
        if let Some(ref m) = self.memory {
            let _ = m.add_message(&self.session_id, "assistant", content);
        }
    }

    // -------------------------------------------------------------- execution

    /// Execute a native tool call through the permission gate and registry.
    async fn execute_native_call(
        &self,
        call: &ModelToolCall,
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> ToolOutcome {
        let name = call.function.name.clone();
        let args = normalize_arguments(call.function.arguments.clone());
        self.execute_call(name, args, confirm).await
    }

    /// Execute a tool call parsed out of the model's text.
    async fn execute_text_call(
        &self,
        call: &parser::ToolCall,
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> ToolOutcome {
        let args = normalize_arguments(call.args.clone());
        self.execute_call(call.name.clone(), args, confirm).await
    }

    /// Run a single tool call through the permission gate and the registry.
    async fn execute_call(
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
        if required.is_empty() {
            return args;
        }
        if required.iter().all(|k| args.get(k).is_some()) {
            return args;
        }
        if let Some(params) = args.get("parameters").and_then(|p| p.as_object()) {
            let nested = Value::Object(params.clone());
            if required.iter().all(|k| nested.get(k).is_some()) {
                return nested;
            }
        }
        args
    }
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------ helpers

/// Tool arguments sometimes arrive as a JSON-encoded string.
fn normalize_arguments(args: Value) -> Value {
    match &args {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        _ => args,
    }
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
        "system.info" => {
            let parts: Vec<String> = ["hostname", "os", "arch"]
                .iter()
                .filter_map(|k| {
                    res.get(*k)
                        .and_then(|v| v.as_str())
                        .map(|v| format!("{}={}", k, v))
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
