//! Unix socket IPC server
//!
//! One connection is handled by one task. A request normally produces one
//! response, but a tool with an `ask` policy makes the daemon send a
//! confirmation prompt to the client and wait for the answer before it can
//! finish. The prompt and the answer travel on the same socket as tagged
//! [`Message`]s, so the client never has to open a second connection.

use crate::confirm::confirmation_prompt;
use crate::daemon::Daemon;
use crate::error::{JarvisError, Result};
use crate::orchestrator::ConfirmationHandler;
use jarvis_common::ipc::{ConfirmationRequest, Message, Request, Response};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info};
use uuid::Uuid;

/// How long the daemon waits for a client to answer a confirmation prompt.
///
/// A client that has gone away (closed laptop, killed terminal) must not
/// hold the orchestrator lock forever, and the safe default is "no".
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

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
        // Remove a socket left behind by a crash, but never one that a live
        // daemon is still listening on: unlinking it would make that daemon
        // unreachable and leave two servers running with one address.
        if self.socket_path.exists() {
            if UnixStream::connect(&self.socket_path).await.is_ok() {
                return Err(JarvisError::Ipc(format!(
                    "another daemon is already listening on {}",
                    self.socket_path.display()
                )));
            }
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

/// Sends confirmation prompts to the client and reads its answers.
struct SocketConfirmer {
    reader: Arc<Mutex<BufReader<OwnedReadHalf>>>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
}

#[async_trait::async_trait]
impl ConfirmationHandler for SocketConfirmer {
    async fn confirm(&self, tool: &str, args: &Value) -> bool {
        let request = ConfirmationRequest {
            id: Uuid::new_v4(),
            tool: tool.to_string(),
            args: args.clone(),
            prompt: confirmation_prompt(tool, args),
        };
        let id = request.id;

        {
            let mut writer = self.writer.lock().await;
            let line = Message::ConfirmationRequest(request).encode();
            if let Err(e) = writer.write_all(format!("{}\n", line).as_bytes()).await {
                error!("Failed to send confirmation prompt: {}", e);
                return false;
            }
            if let Err(e) = writer.flush().await {
                error!("Failed to flush confirmation prompt: {}", e);
                return false;
            }
        }

        let answer = tokio::time::timeout(CONFIRM_TIMEOUT, async {
            let mut reader = self.reader.lock().await;
            let mut line = String::new();
            loop {
                line.clear();
                let read = reader.read_line(&mut line).await.ok()?;
                if read == 0 {
                    // The client hung up without answering.
                    return None;
                }
                match Message::decode(&line) {
                    Ok(Message::ConfirmationReply(reply)) if reply.id == id => {
                        return Some(reply.approved)
                    }
                    // Anything else is stale or unrelated; keep waiting for
                    // the answer to *this* prompt.
                    Ok(_) => continue,
                    Err(e) => {
                        error!("Ignoring unparseable message during confirmation: {}", e);
                        continue;
                    }
                }
            }
        })
        .await;

        match answer {
            Ok(Some(approved)) => approved,
            Ok(None) => {
                info!("Client disconnected while confirming {}", tool);
                false
            }
            Err(_) => {
                info!(
                    "Timed out after {}s waiting for confirmation of {}",
                    CONFIRM_TIMEOUT.as_secs(),
                    tool
                );
                false
            }
        }
    }
}

async fn handle_client(stream: UnixStream, daemon: Arc<Mutex<Daemon>>) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let reader = Arc::new(Mutex::new(BufReader::new(reader)));
    let writer = Arc::new(Mutex::new(writer));
    let mut line = String::new();

    loop {
        line.clear();
        let read = {
            let mut guard = reader.lock().await;
            guard.read_line(&mut line).await?
        };
        if read == 0 {
            break;
        }

        let request = match Message::decode(&line) {
            Ok(Message::Request(request)) => request,
            Ok(other) => {
                error!("Ignoring unexpected message from client: {:?}", other);
                continue;
            }
            Err(e) => {
                error!("Failed to parse message: {}", e);
                continue;
            }
        };

        let confirmer = SocketConfirmer {
            reader: Arc::clone(&reader),
            writer: Arc::clone(&writer),
        };
        let response = handle_request(request, &daemon, Some(&confirmer)).await;

        let mut guard = writer.lock().await;
        guard
            .write_all(format!("{}\n", Message::Response(response).encode()).as_bytes())
            .await?;
        guard.flush().await?;
    }

    Ok(())
}

async fn handle_request(
    request: Request,
    daemon: &Arc<Mutex<Daemon>>,
    confirm: Option<&dyn ConfirmationHandler>,
) -> Response {
    let daemon_lock = daemon.lock().await;

    match request.method.as_str() {
        "ask" => {
            let prompt = match request.params.get("prompt").and_then(|p| p.as_str()) {
                Some(p) => p.to_string(),
                None => return Response::err(request.id, "Missing 'prompt' parameter".to_string()),
            };
            match daemon_lock.ask_with(&prompt, confirm).await {
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
