//! Agent orchestrator - planning, tool selection, execution loop

use crate::config::Config;
use crate::error::Result;
use crate::model::OllamaClient;
use crate::tools::{filesystem::ListDirTool, shell::RunShellTool, system::GetSystemInfoTool, Tool, ToolRegistry};
use tracing::info;

pub struct Orchestrator {
    config: Config,
    client: OllamaClient,
    tools: ToolRegistry,
}

impl Orchestrator {
    pub fn new() -> Self {
        let config = Config::default();
        let client = OllamaClient::new(&config.model.base_url);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(ListDirTool));
        tools.register(Box::new(RunShellTool));
        tools.register(Box::new(GetSystemInfoTool));
        Self {
            config,
            client,
            tools,
        }
    }

    pub fn with_config(config: Config) -> Self {
        let client = OllamaClient::new(&config.model.base_url);
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(ListDirTool));
        tools.register(Box::new(RunShellTool));
        tools.register(Box::new(GetSystemInfoTool));
        Self {
            config,
            client,
            tools,
        }
    }

    pub async fn run(&self, input: &str) -> Result<String> {
        info!("Processing: {}", input);
        // Try to use model, fallback to simple response if it fails
        match self
            .client
            .chat(&self.config.model.default_model, input)
            .await
        {
            Ok(response) => Ok(response),
            Err(e) => {
                info!("Model unavailable: {}, using fallback", e);
                Ok(format!(
                    "Hello, I'm Jarvis. Core is under construction.\n\nI received: {}\n\nTools available: {:?}\n\n(Note: Model call failed, using fallback)",
                    input,
                    self.tools.list()
                ))
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
