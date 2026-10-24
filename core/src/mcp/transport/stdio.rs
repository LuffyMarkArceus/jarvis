use crate::error::{JarvisError, Result};
use crate::mcp::types::{JsonRpcRequest, JsonRpcResponse};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

pub struct StdioTransport {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    _child: Arc<Mutex<Child>>,
}

impl StdioTransport {
    pub async fn start(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| JarvisError::Tool(format!("Failed to start MCP server: {}", e)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| JarvisError::Tool("MCP server has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| JarvisError::Tool("MCP server has no stdout".into()))?;
        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&line) {
                    if let Some(id) = resp.id.as_ref().and_then(|v| v.as_u64()) {
                        let mut p = pending_clone.lock().await;
                        if let Some(tx) = p.remove(&id) {
                            let _ = tx.send(resp);
                            continue;
                        }
                    }
                    // Notifications/responses without pending waiter are ignored for now
                }
            }
        });
        Ok(Self {
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            _child: Arc::new(Mutex::new(child)),
        })
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<JsonRpcResponse> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(id)),
            method: method.to_string(),
            params,
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut stdin = self.stdin.lock().await;
        let line = serde_json::to_string(&req).map_err(|e| JarvisError::Tool(e.to_string()))?;
        stdin
            .write_all(format!("{}\n", line).as_bytes())
            .await
            .map_err(|e| JarvisError::Tool(format!("Failed to write to MCP server: {}", e)))?;
        stdin
            .flush()
            .await
            .map_err(|e| JarvisError::Tool(e.to_string()))?;
        drop(stdin);
        let resp = tokio::time::timeout(std::time::Duration::from_secs(60), rx)
            .await
            .map_err(|_| JarvisError::Tool("MCP request timed out".into()))?
            .map_err(|_| JarvisError::Tool("MCP server closed connection".into()))?;
        if let Some(err) = resp.error {
            return Err(JarvisError::Tool(format!(
                "MCP error {}: {}",
                err.code, err.message
            )));
        }
        Ok(resp)
    }

    pub async fn notify(&self, method: &str, params: Option<serde_json::Value>) -> Result<()> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.to_string(),
            params,
        };
        let mut stdin = self.stdin.lock().await;
        let line = serde_json::to_string(&req).map_err(|e| JarvisError::Tool(e.to_string()))?;
        stdin
            .write_all(format!("{}\n", line).as_bytes())
            .await
            .map_err(|e| JarvisError::Tool(e.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|e| JarvisError::Tool(e.to_string()))?;
        Ok(())
    }
}
