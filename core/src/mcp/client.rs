use crate::config::Config;
use crate::error::{JarvisError, Result};
use crate::mcp::transport::stdio::StdioTransport;
use crate::mcp::types::{DiscoveredTool, McpTool};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Clone)]
pub struct McpClient {
    transports: Arc<Mutex<HashMap<String, Arc<StdioTransport>>>>,
    tools: Arc<Mutex<Vec<DiscoveredTool>>>,
}

impl McpClient {
    pub fn new() -> Self {
        Self {
            transports: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn start_from_config(&self, config: &Config) -> Result<()> {
        let mcp_config = match &config.mcp {
            Some(c) => c,
            None => return Ok(()),
        };
        for server in &mcp_config.servers {
            if !server.enabled {
                continue;
            }
            info!("Starting MCP server: {}", server.name);
            let transport = StdioTransport::start(&server.command, &server.args, &server.env).await?;
            let transport = Arc::new(transport);
            // Initialize
            let _init = transport
                .request(
                    "initialize",
                    Some(serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "jarvis",
                            "version": "0.1.0"
                        }
                    })),
                )
                .await?;
            let _ = transport.notify("notifications/initialized", None).await;
            // Discover tools
            let resp = transport.request("tools/list", None).await?;
            if let Some(result) = resp.result {
                if let Some(tools) = result.get("tools").and_then(|t| t.as_array()) {
                    let mut all_tools = self.tools.lock().await;
                    for tool in tools {
                        if let Ok(mcp_tool) = serde_json::from_value::<McpTool>(tool.clone()) {
                            let full_name = format!("mcp.{}.{}", server.name, mcp_tool.name);
                            all_tools.push(DiscoveredTool {
                                server: server.name.clone(),
                                name: mcp_tool.name.clone(),
                                full_name: full_name.clone(),
                                description: mcp_tool
                                    .description
                                    .clone()
                                    .unwrap_or_else(|| format!("MCP tool {}", full_name)),
                                schema: mcp_tool.input_schema.clone().unwrap_or(serde_json::json!({"type": "object"})),
                            });
                            info!("Discovered MCP tool: {}", full_name);
                        }
                    }
                }
            }
            self.transports.lock().await.insert(server.name.clone(), transport);
        }
        Ok(())
    }

    pub async fn list_tools(&self) -> Vec<DiscoveredTool> {
        self.tools.lock().await.clone()
    }

    pub async fn call_tool(&self, server: &str, tool: &str, args: Value) -> Result<String> {
        let transports = self.transports.lock().await;
        let transport = transports
            .get(server)
            .cloned()
            .ok_or_else(|| JarvisError::Tool(format!("MCP server '{}' not found", server)))?;
        drop(transports);
        let resp = transport
            .request(
                "tools/call",
                Some(serde_json::json!({
                    "name": tool,
                    "arguments": args,
                })),
            )
            .await?;
        let result = resp.result.unwrap_or(Value::Null);
        // MCP returns content array
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            let mut out = String::new();
            for item in content {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                } else if let Some(data) = item.get("data") {
                    out.push_str(&data.to_string());
                }
            }
            if out.is_empty() {
                return Ok(result.to_string());
            }
            return Ok(out);
        }
        Ok(result.to_string())
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}
