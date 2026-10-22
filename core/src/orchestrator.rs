//! Agent orchestrator - planning, tool selection, execution loop

use crate::config::Config;
use crate::error::Result;
use crate::memory::MemoryManager;
use crate::model::OllamaClient;
use crate::permissions::{PermissionAction, PermissionManager};
use crate::tools::{
    filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
    parser,
    shell::RunShellTool,
    system::GetSystemInfoTool,
    ToolRegistry,
};
use tracing::info;
use uuid::Uuid;

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
        let config = Config::default();
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
        let permissions = PermissionManager::new(&config).unwrap_or_else(|_| {
            // Fallback - create minimal
            PermissionManager::new(&config).expect("Failed to init permissions")
        });
        Self {
            config,
            client,
            tools,
            memory,
            session_id,
            permissions,
        }
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

    pub async fn run(&self, input: &str) -> Result<String> {
        info!("Processing: {}", input);

        // Save user message
        if let Some(ref m) = self.memory {
            let _ = m.add_message(&self.session_id, "user", input);
        }

        let mut current_context = input.to_string();
        let mut iterations = 0;
        let max_iterations = 4;

        loop {
            iterations += 1;
            if iterations > max_iterations {
                break;
            }

            // Build context with history (token-aware truncation)
            let mut context = String::new();
            context.push_str("You are Jarvis, a helpful personal assistant. You have access to tools. When you need to use a tool, respond with a JSON block containing tool_calls. You can use any of these formats:\n");
            context.push_str(
                "1. Custom: {\"tool_calls\": [{\"name\": \"tool.name\", \"args\": {...}}]}\n",
            );
            context.push_str("2. OpenAI style: {\"tool_calls\": [{\"type\": \"function\", \"function\": {\"name\": \"...\", \"arguments\": \"{...}\"}}]}\n");
            context.push_str("3. Anthropic style: {\"content\": [{\"type\": \"tool_use\", \"name\": \"...\", \"input\": {...}}]}\n");
            context.push_str(&format!("Available tools: {:?}\n\n", self.tools.list()));
            if let Some(ref m) = self.memory {
                if let Ok(history) =
                    m.get_recent_messages(&self.session_id, self.config.context.max_history_fetch)
                {
                    if !history.is_empty() {
                        context.push_str("Previous conversation:\n");
                        let mut messages_to_include: Vec<_> = Vec::new();
                        for msg in history.iter() {
                            if msg.role == "user"
                                && msg.content == input
                                && msg.id == history.last().map(|h| h.id).unwrap_or(-1)
                                && iterations == 1
                            {
                                continue;
                            }
                            messages_to_include.push(msg);
                        }
                        let mut total_chars = 0;
                        let mut included = Vec::new();
                        for msg in messages_to_include.iter().rev() {
                            let msg_chars = msg.content.len() + msg.role.len() + 3;
                            if total_chars + msg_chars > self.config.context.max_context_chars
                                && !included.is_empty()
                            {
                                break;
                            }
                            included.push(*msg);
                            total_chars += msg_chars;
                        }
                        included.reverse();
                        for msg in included.iter() {
                            context.push_str(&format!("{}: {}\n", msg.role, msg.content));
                        }
                        context.push('\n');
                    }
                }
            }
            if iterations == 1 {
                context.push_str(&format!("User: {}\nAssistant:", current_context));
            } else {
                context.push_str(&format!("{}\nAssistant:", current_context));
            }

            // Try to use model
            let response = match self
                .client
                .chat(&self.config.model.default_model, &context)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    info!("Model unavailable: {}, using fallback", e);
                    let fallback = format!(
                        "Hello, I'm Jarvis. Core is under construction.\n\nI received: {}\n\nTools available: {:?}\n\n(Note: Model call failed, using fallback)",
                        input,
                        self.tools.list()
                    );
                    if let Some(ref m) = self.memory {
                        let _ = m.add_message(&self.session_id, "assistant", &fallback);
                    }
                    return Ok(fallback);
                }
            };

            // Check for tool calls
            let tool_calls = parser::parse_tool_calls(&response);
            if tool_calls.is_empty() {
                if let Some(ref m) = self.memory {
                    let _ = m.add_message(&self.session_id, "assistant", &response);
                }
                return Ok(response);
            }

            // Execute tool calls
            let mut tool_results = String::new();
            tool_results.push_str("\n\nTool Results:\n");
            for tc in &tool_calls {
                let perm = self.permissions.check_permission(&tc.name, &tc.args);
                match perm {
                    PermissionAction::Deny => {
                        let msg = format!("Tool {} denied by policy\n", tc.name);
                        tool_results.push_str(&msg);
                        let _ = self.permissions.log_audit(
                            Some(&self.session_id),
                            &tc.name,
                            &tc.args,
                            "denied",
                            Some(&msg),
                        );
                    }
                    PermissionAction::Ask => {
                        let msg = format!(
                            "Tool {} requires confirmation (auto-denied in non-interactive mode). Args: {}\n",
                            tc.name, tc.args
                        );
                        tool_results.push_str(&msg);
                        let _ = self.permissions.log_audit(
                            Some(&self.session_id),
                            &tc.name,
                            &tc.args,
                            "ask_auto_denied",
                            Some(&msg),
                        );
                    }
                    PermissionAction::Allow => {
                        match self.tools.execute(&tc.name, tc.args.clone()).await {
                            Ok(res) => {
                                let res_str =
                                    serde_json::to_string_pretty(&res).unwrap_or_default();
                                tool_results.push_str(&format!(
                                    "Tool {} succeeded:\n{}\n\n",
                                    tc.name, res_str
                                ));
                                let _ = self.permissions.log_audit(
                                    Some(&self.session_id),
                                    &tc.name,
                                    &tc.args,
                                    "allowed",
                                    Some(&res_str),
                                );
                            }
                            Err(e) => {
                                let msg = format!("Tool {} failed: {}\n", tc.name, e);
                                tool_results.push_str(&msg);
                                let _ = self.permissions.log_audit(
                                    Some(&self.session_id),
                                    &tc.name,
                                    &tc.args,
                                    "allowed_failed",
                                    Some(&msg),
                                );
                            }
                        }
                    }
                }
            }

            current_context = format!("{}{}", response, tool_results);
            if let Some(ref m) = self.memory {
                let _ = m.add_message(&self.session_id, "assistant", &response);
            }
        }

        if let Some(ref m) = self.memory {
            let _ = m.add_message(&self.session_id, "assistant", &current_context);
        }
        Ok(current_context)
    }

    pub fn list_tools(&self) -> Vec<&str> {
        self.tools.list()
    }
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}
