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

use crate::config::Config;
use crate::error::Result;
use crate::memory::{MemoryManager, Message};
use crate::model::{ChatMessage, ModelToolCall, OllamaClient};
use crate::permissions::{PermissionAction, PermissionManager};
use crate::router::{self, Route};
use crate::tools::{
    filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
    parser,
    shell::RunShellTool,
    system::GetSystemInfoTool,
    ToolRegistry,
};
use serde_json::Value;
use tracing::info;
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
}

pub struct Orchestrator {
    config: Config,
    client: OllamaClient,
    tools: ToolRegistry,
    memory: Option<MemoryManager>,
    session_id: String,
    permissions: PermissionManager,
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
        tools.register(Box::new(WriteFileTool));
        tools.register(Box::new(EditFileTool));
        tools.register(Box::new(RunShellTool));
        tools.register(Box::new(GetSystemInfoTool));
        let session_id = Uuid::new_v4().to_string();
        let memory = MemoryManager::new(config.clone()).ok();
        if let Some(ref m) = memory {
            let _ = m.create_session(&session_id);
        }
        let permissions = PermissionManager::new(&config).expect("Failed to init permissions");
        Self {
            config,
            client,
            tools,
            memory,
            session_id,
            permissions,
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
        info!("Processing: {}", input);

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
            Route::Direct => self.converse(input, &history).await,

            // A confidently identified tool call: run it and report the facts.
            Route::Tool { name, args } => {
                let outcome = self.execute_call(name, args, confirm).await;
                self.answer_from_outcome(&outcome)
            }

            // Ambiguous: let the model pick from the catalogue.
            Route::Model => self.model_with_tools(input, &history, confirm).await,
        };

        self.persist_assistant(&answer);
        Ok(answer)
    }

    pub fn list_tools(&self) -> Vec<&str> {
        self.tools.list()
    }

    // ------------------------------------------------------------ conversations

    /// Plain conversation. The model is given no tools, so it cannot try to
    /// use one - which is exactly what we want for "who are you?".
    async fn converse(&self, input: &str, history: &[Message]) -> String {
        let messages = self.build_messages(input, history);
        let turn = match self
            .client
            .chat_turn_with_options(
                &self.config.model.default_model,
                Some(&self.conversation_system_prompt()),
                &messages,
                None,
                Some(self.model_options()),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                info!("Model unavailable: {}, using fallback", e);
                return self.fallback_answer(input);
            }
        };
        info!("model turn: {:?}", turn.content);

        let answer = final_answer(&turn.content);
        if answer.is_empty() {
            self.empty_answer(input)
        } else {
            answer
        }
    }

    /// The ambiguous case: the model may call a tool, and the result is
    /// rendered deterministically.
    async fn model_with_tools(
        &self,
        input: &str,
        history: &[Message],
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> String {
        let mut messages = self.build_messages(input, history);
        let tools = self.tools.schemas();
        let mut trace: Vec<ToolOutcome> = Vec::new();

        for _ in 0..MAX_ITERATIONS {
            let turn = match self
                .client
                .chat_turn_with_options(
                    &self.config.model.default_model,
                    Some(&self.tool_system_prompt()),
                    &messages,
                    Some(&tools),
                    Some(self.model_options()),
                )
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    info!("Model unavailable: {}, using fallback", e);
                    return self.fallback_answer(input);
                }
            };
            info!(
                "model turn: content={:?} tool_calls={}",
                turn.content,
                turn.tool_calls.is_some()
            );

            // Native function calling.
            if let Some(calls) = turn.tool_calls.as_ref().filter(|c| !c.is_empty()) {
                messages.push(ChatMessage::assistant_with_tool_calls(
                    &turn.content,
                    calls.clone(),
                ));
                for call in calls {
                    let outcome = self.execute_native_call(call, confirm).await;
                    messages.push(ChatMessage::tool_result(&outcome.name, &outcome.output));
                    trace.push(outcome);
                }
                if let Some(report) = blocked_report(&trace) {
                    return report;
                }
                continue;
            }

            // Tool calls embedded in the reply text.
            let text_calls = parser::parse_tool_calls(&turn.content);
            if !text_calls.is_empty() {
                messages.push(ChatMessage::text("assistant", &turn.content));
                for call in &text_calls {
                    let outcome = self.execute_text_call(call, confirm).await;
                    messages.push(ChatMessage::text(
                        "user",
                        &format!("Result of {}:\n{}", outcome.name, outcome.output),
                    ));
                    trace.push(outcome);
                }
                if let Some(report) = blocked_report(&trace) {
                    return report;
                }
                continue;
            }

            // A plain answer.
            let answer = final_answer(&turn.content);
            if answer.is_empty() {
                return if trace.is_empty() {
                    self.empty_answer(input)
                } else {
                    render_execution_report(&trace)
                };
            }
            return answer;
        }

        render_execution_report(&trace)
    }

    /// Turn a single tool outcome into the user-facing answer.
    fn answer_from_outcome(&self, outcome: &ToolOutcome) -> String {
        if outcome.status.is_blocked() {
            return render_blocked_report(std::slice::from_ref(outcome));
        }
        render_tool_output(outcome)
    }

    // ---------------------------------------------------------------- prompts

    /// Provider tuning options for a single request.
    fn model_options(&self) -> Value {
        serde_json::json!({ "temperature": self.config.model.temperature })
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
        p
    }

    /// The conversation handed to the model: history, then the request.
    fn build_messages(&self, input: &str, history: &[Message]) -> Vec<ChatMessage> {
        let mut messages: Vec<ChatMessage> = Vec::new();
        for msg in history {
            messages.push(ChatMessage::text(&msg.role, &msg.content));
        }
        messages.push(ChatMessage::text("user", input));
        messages
    }

    /// Recent history, trimmed to the context budget (oldest first when
    /// rendered).
    fn history_for_prompt(&self) -> Vec<Message> {
        let Some(ref m) = self.memory else {
            return Vec::new();
        };
        let Ok(messages) =
            m.get_recent_messages(&self.session_id, self.config.context.max_history_fetch)
        else {
            return Vec::new();
        };

        let budget = self.config.context.max_context_chars;
        let mut total = 0usize;
        let mut kept: Vec<&Message> = Vec::new();
        for msg in messages.iter().rev() {
            let size = msg.content.len() + msg.role.len() + 3;
            if total + size > budget && !kept.is_empty() {
                break;
            }
            total += size;
            kept.push(msg);
        }
        kept.reverse();
        kept.into_iter().cloned().collect()
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

        match self.permissions.check_permission(&name, &args) {
            PermissionAction::Deny => {
                let msg = format!(
                    "Tool {} denied by policy. Args: {}",
                    name,
                    compact_args(&args)
                );
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
                }
            }
            PermissionAction::Ask => match confirm {
                Some(handler) => {
                    if handler.confirm(&name, &args).await {
                        self.run_tool(name, args, "confirmed").await
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
                    }
                }
            },
            PermissionAction::Allow => self.run_tool(name, args, "allowed").await,
        }
    }

    /// Execute an approved tool and record the outcome.
    ///
    /// `decision` is what lands in the audit log, so a call a human approved
    /// is distinguishable after the fact from one the policy allowed
    /// outright.
    async fn run_tool(&self, name: String, args: Value, decision: &str) -> ToolOutcome {
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
                }
            }
        }
    }

    /// Some models echo the tool schema back as the arguments object, e.g.
    /// `{"type": "function", "function": "list_dir", "parameters": {...}}`.
    /// Unwrap that so the real arguments reach the tool.
    fn sanitize_args(&self, tool: &str, args: Value) -> Value {
        let required = self.tools.required_args(tool);
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

    // Only suggest loosening the policy when the policy is what stopped it.
    // After an explicit "no" the useful guidance is nothing at all.
    let policy_blocked: Vec<&ToolOutcome> = blocked
        .iter()
        .copied()
        .filter(|t| t.status != OutcomeStatus::Declined)
        .collect();
    if !policy_blocked.is_empty() {
        let mut names: Vec<&str> = policy_blocked.iter().map(|t| t.name.as_str()).collect();
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
