//! Tool registry and execution system

pub mod filesystem;
pub mod parser;
pub mod shell;
pub mod system;

use crate::error::Result;
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, args: Value) -> Result<Value>;
}

pub struct ToolRegistry {
    tools: std::collections::HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: std::collections::HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn list(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    pub async fn execute(&self, name: &str, args: Value) -> Result<Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| crate::error::JarvisError::Tool(format!("Unknown tool: {}", name)))?;
        tool.execute(args).await
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
