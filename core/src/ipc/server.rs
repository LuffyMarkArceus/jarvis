//! Unix socket IPC server

use crate::daemon::Daemon;
use crate::error::{JarvisError, Result};
use jarvis_common::ipc::{Request, Response};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info};

pub struct IpcServer {
    socket_path: PathBuf,
    daemon: Arc<Mutex<Daemon>>,
}

impl IpcServer {
    pub fn new(daemon: Arc<Mutex<Daemon>>, socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            daemon,
        }
    }

    pub async fn start(&self) -> Result<()> {
        // Remove old socket if exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .map_err(|e| JarvisError::Ipc(format!("Failed to remove old socket: {}", e)))?;
        }

        // Ensure directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| JarvisError::Ipc(format!("Failed to create socket dir: {}", e)))?;
        }

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| JarvisError::Ipc(format!("Failed to bind socket: {}", e)))?;

        info!("IPC server listening on {:?}", self.socket_path);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let daemon = Arc::clone(&self.daemon);
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, daemon).await {
                            error!("Error handling client: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                }
            }
        }
    }
}

async fn handle_client(stream: UnixStream, daemon: Arc<Mutex<Daemon>>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        // Try to parse request
        let request: Request = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                error!("Failed to parse request: {}", e);
                line.clear();
                continue;
            }
        };

        // Handle request
        let response = handle_request(request, &daemon).await;

        // Send response
        let response_json = serde_json::to_string(&response)
            .map_err(|e| JarvisError::Ipc(format!("Failed to serialize response: {}", e)))?;
        writer
            .write_all(format!("{}\n", response_json).as_bytes())
            .await?;
        writer.flush().await?;

        line.clear();
    }

    Ok(())
}

async fn handle_request(request: Request, daemon: &Arc<Mutex<Daemon>>) -> Response {
    let daemon_lock = daemon.lock().await;

    match request.method.as_str() {
        "ask" => {
            let prompt = match request.params.get("prompt").and_then(|p| p.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(request.id, "Missing 'prompt' parameter".to_string()),
            };
            match daemon_lock.ask(&prompt).await {
                Ok(response) => Response::ok(request.id, serde_json::json!({"response": response})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "status" => {
            let sid = daemon_lock.session_id().await;
            Response::ok(
                request.id,
                serde_json::json!({
                    "status": "running",
                    "tools": daemon_lock.list_tools(),
                    "session_id": sid,
                }),
            )
        }
        "tools.list" => Response::ok(
            request.id,
            serde_json::json!({
                "tools": daemon_lock.list_tools(),
            }),
        ),
        "session.list" => {
            let limit = request
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;
            match daemon_lock.list_sessions(limit).await {
                Ok(sessions) => Response::ok(request.id, serde_json::json!({"sessions": sessions})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.new" => match daemon_lock.new_session().await {
            Ok(sid) => Response::ok(request.id, serde_json::json!({"session_id": sid})),
            Err(e) => Response::err(request.id, e.to_string()),
        },
        "session.switch" => {
            let sid = match request.params.get("session_id").and_then(|p| p.as_str()) {
                Some(p) => p.to_string(),
                None => {
                    return Response::err(request.id, "Missing 'session_id' parameter".to_string())
                }
            };
            match daemon_lock.set_session(&sid).await {
                Ok(_) => Response::ok(request.id, serde_json::json!({"session_id": sid})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.current" => {
            let sid = daemon_lock.session_id().await;
            Response::ok(request.id, serde_json::json!({"session_id": sid}))
        }
        "session.delete" => {
            let sid = match request.params.get("session_id").and_then(|p| p.as_str()) {
                Some(p) => p.to_string(),
                None => {
                    return Response::err(request.id, "Missing 'session_id' parameter".to_string())
                }
            };
            match daemon_lock.delete_session(&sid).await {
                Ok(_) => Response::ok(request.id, serde_json::json!({"ok": true})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.show" => {
            let sid = match request.params.get("session_id").and_then(|p| p.as_str()) {
                Some(p) => p.to_string(),
                None => {
                    return Response::err(request.id, "Missing 'session_id' parameter".to_string())
                }
            };
            match daemon_lock.get_session_messages(&sid).await {
                Ok(messages) => Response::ok(request.id, serde_json::json!({"messages": messages})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.prune" => match daemon_lock.prune_empty_sessions().await {
            Ok(count) => Response::ok(request.id, serde_json::json!({"pruned": count})),
            Err(e) => Response::err(request.id, e.to_string()),
        },
        "ping" => Response::ok(request.id, serde_json::json!({"pong": true})),
        _ => Response::err(request.id, format!("Unknown method: {}", request.method)),
    }
}
