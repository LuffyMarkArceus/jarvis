//! Agent orchestrator - planning, tool selection, execution loop

use crate::config::Config;
use crate::error::Result;
use crate::memory::MemoryManager;
use crate::model::OllamaClient;
use crate::tools::{
    filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
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
        Self {
            config,
            client,
            tools,
            memory,
            session_id,
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
        Self {
            config,
            client,
            tools,
            memory,
            session_id,
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

        // Build context with history (token-aware truncation)
        let mut context = String::new();
        if let Some(ref m) = self.memory {
            if let Ok(history) = m.get_recent_messages(&self.session_id, 50) {
                if !history.is_empty() {
                    context.push_str("Previous conversation:\n");
                    let mut messages_to_include: Vec<_> = Vec::new();
                    for msg in history.iter() {
                        if msg.role == "user"
                            && msg.content == input
                            && msg.id == history.last().map(|h| h.id).unwrap_or(-1)
                        {
                            continue;
                        }
                        messages_to_include.push(msg);
                    }
                    let mut total_chars = 0;
                    let mut included = Vec::new();
                    for msg in messages_to_include.iter().rev() {
                        let msg_chars = msg.content.len() + msg.role.len() + 3;
                        if total_chars + msg_chars > 2000 && !included.is_empty() {
                            break;
                        }
                        included.push(*msg);
                        total_chars += msg_chars;
                    }
                    included.reverse();
                    for msg in included.iter() {
                        context.push_str(&format!("{}: {}\n", msg.role, msg.content));
                    }
                    if included.is_empty() && !messages_to_include.is_empty() {
                        let last_idx = messages_to_include.len().saturating_sub(1);
                        let last = messages_to_include[last_idx];
                        let truncated = if last.content.len() > 500 {
                            format!("{}...", &last.content[..500])
                        } else {
                            last.content.clone()
                        };
                        context.push_str(&format!("{}: {}\n", last.role, truncated));
                    }
                    context.push('\n');
                }
            }
        }
        context.push_str(&format!("User: {}\nAssistant:", input));

        // Try to use model, fallback to simple response if it fails
        match self
            .client
            .chat(&self.config.model.default_model, &context)
            .await
        {
            Ok(response) => {
                if let Some(ref m) = self.memory {
                    let _ = m.add_message(&self.session_id, "assistant", &response);
                }
                Ok(response)
            }
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
                Ok(fallback)
            }
        }
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
