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

/// The names of the tools Jarvis ships with.
///
/// Kept next to the registry so the CLI can validate a permission rule
/// against the real tool catalogue instead of accepting a typo that would
/// never match anything.
pub fn known_tool_names() -> Vec<&'static str> {
    vec![
        "filesystem.list_dir",
        "filesystem.read",
        "filesystem.write",
        "filesystem.edit",
        "shell.run",
        "system.info",
    ]
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

    /// Returns tool names with their descriptions, for prompting the model.
    pub fn describe(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .tools
            .values()
            .map(|t| (t.name().to_string(), t.description().to_string()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Tool definitions in OpenAI/Ollama function format, for the native
    /// function-calling interface.
    pub fn schemas(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.schema(),
                    }
                })
            })
            .collect();
        out.sort_by(|a, b| {
            a["function"]["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["function"]["name"].as_str().unwrap_or(""))
        });
        out
    }

    /// The required argument names of a tool, per its schema.
    pub fn required_args(&self, name: &str) -> Vec<String> {
        let Some(tool) = self.tools.get(name) else {
            return Vec::new();
        };
        tool.schema()
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
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
