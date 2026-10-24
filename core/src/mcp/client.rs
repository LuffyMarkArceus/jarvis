//! MCP client: starts configured stdio servers, discovers their tools, and
//! calls them.
//!
//! The discovered tool list is kept in a `std::sync::RwLock` rather than a
//! tokio mutex. It is a read-mostly cache that the orchestrator needs on
//! synchronous paths (`list_tools`, prompt assembly, schema checks), and
//! holding an async lock across those call sites would be needless
//! ceremony. Only ownership of the live transports needs async locking.

use crate::config::Config;
use crate::error::{JarvisError, Result};
use crate::mcp::transport::stdio::StdioTransport;
use crate::mcp::types::{DiscoveredTool, McpTool};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone)]
pub struct McpClient {
    transports: Arc<Mutex<HashMap<String, Arc<StdioTransport>>>>,
    tools: Arc<RwLock<Vec<DiscoveredTool>>>,
}

impl McpClient {
    pub fn new() -> Self {
        Self {
            transports: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Start every enabled server and discover its tools.
    ///
    /// A server that fails to start is logged and skipped: one broken MCP
    /// entry in the config must not take the whole assistant down.
    pub async fn start_from_config(&self, config: &Config) -> Result<()> {
        let Some(mcp_config) = &config.mcp else {
            return Ok(());
        };

        for server in &mcp_config.servers {
            if !server.enabled {
                continue;
            }
            info!("Starting MCP server: {}", server.name);
            match self.start_server(server).await {
                Ok(count) => info!("MCP server {} ready ({} tools)", server.name, count),
                Err(e) => warn!("MCP server {} failed to start: {}", server.name, e),
            }
        }
        Ok(())
    }

    async fn start_server(&self, server: &crate::config::McpServerConfig) -> Result<usize> {
        let transport =
            Arc::new(StdioTransport::start(&server.command, &server.args, &server.env).await?);

        transport
            .request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "jarvis",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
            )
            .await?;
        transport.notify("notifications/initialized", None).await?;

        let resp = transport.request("tools/list", None).await?;
        let mut discovered = Vec::new();

        if let Some(tools) = resp
            .result
            .as_ref()
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
        {
            for tool in tools {
                let Ok(mcp_tool) = serde_json::from_value::<McpTool>(tool.clone()) else {
                    warn!(
                        "MCP server {} returned an unparseable tool entry",
                        server.name
                    );
                    continue;
                };
                let full_name = format!("mcp.{}.{}", server.name, mcp_tool.name);
                discovered.push(DiscoveredTool {
                    server: server.name.clone(),
                    name: mcp_tool.name.clone(),
                    full_name: full_name.clone(),
                    description: mcp_tool
                        .description
                        .unwrap_or_else(|| format!("MCP tool {}", full_name)),
                    schema: mcp_tool
                        .input_schema
                        .unwrap_or(serde_json::json!({"type": "object"})),
                });
                info!("Discovered MCP tool: {}", full_name);
            }
        }

        let count = discovered.len();
        if let Ok(mut tools) = self.tools.write() {
            tools.extend(discovered);
        }
        self.transports
            .lock()
            .await
            .insert(server.name.clone(), transport);
        Ok(count)
    }

    /// A snapshot of every discovered tool. Safe to call from sync code.
    pub fn list_tools(&self) -> Vec<DiscoveredTool> {
        self.tools.read().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn has_server(&self, server: &str) -> bool {
        self.transports
            .try_lock()
            .map(|t| t.contains_key(server))
            .unwrap_or(false)
    }

    pub async fn call_tool(&self, server: &str, tool: &str, args: Value) -> Result<String> {
        let transport = {
            let transports = self.transports.lock().await;
            transports
                .get(server)
                .cloned()
                .ok_or_else(|| JarvisError::Tool(format!("MCP server '{}' not found", server)))?
        };

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

        // `isError` means the tool ran and reported failure. Surfacing it as
        // an error keeps the orchestrator's "blocked/failed is never narrated
        // as success" guarantee intact.
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(JarvisError::Tool(flatten_content(&result)));
        }

        Ok(flatten_content(&result))
    }
}

/// MCP tool results are a `content` array of typed blocks. Flatten the
/// textual ones into one string for the model and the user.
fn flatten_content(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(|c| c.as_array()) else {
        return result.to_string();
    };

    let mut out = String::new();
    for item in content {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        } else if let Some(uri) = item.get("uri").and_then(|u| u.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(uri);
        } else if let Some(data) = item.get("data") {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&data.to_string());
        }
    }

    if out.is_empty() {
        result.to_string()
    } else {
        out
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}
