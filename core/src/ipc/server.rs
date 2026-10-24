//! Unix socket IPC server
//!
//! Each connection has one reader task and one shared writer. A request is
//! handled in its own task so the reader stays free while a turn runs: that
//! is what makes it possible to deliver a [`Message::Cancel`] (or a
//! confirmation answer) on the same socket, at any point during the turn.
//!
//! Several requests from one connection can therefore be in flight at once.
//! They are still serialised by the daemon lock, which owns the single active
//! session. Every message that belongs to a request carries its id, so
//! interleaving is unambiguous.

use crate::cancel::{Cancellation, TurnGuard, TurnRegistry};
use crate::confirm::confirmation_prompt;
use crate::daemon::Daemon;
use crate::error::{JarvisError, Result};
use crate::orchestrator::ConfirmationHandler;
use jarvis_common::ipc::{Chunk, ConfirmationRequest, Message, Request, Response};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
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
    turns: TurnRegistry,
}

impl IpcServer {
    pub fn new(daemon: Arc<Mutex<Daemon>>, socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            daemon,
            turns: TurnRegistry::default(),
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
                    let turns = self.turns.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, daemon, turns).await {
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

/// The write side of one connection, shared by turn tasks and prompts.
#[derive(Clone)]
struct Conn {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    /// Prompts that are waiting for an answer, keyed by their id. The reader
    /// loop routes replies here instead of letting the confirmer read the
    /// socket, which would compete with cancellation messages.
    confirmations: Arc<std::sync::Mutex<HashMap<Uuid, oneshot::Sender<bool>>>>,
}

impl Conn {
    fn new(writer: OwnedWriteHalf) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            confirmations: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn send(&self, message: &Message) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(format!("{}\n", message.encode()).as_bytes())
            .await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Sends confirmation prompts to the client and waits for the routed answer.
struct SocketConfirmer {
    conn: Conn,
    cancel: Cancellation,
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
        let (sender, receiver) = oneshot::channel();
        self.conn
            .confirmations
            .lock()
            .expect("confirmation map poisoned")
            .insert(id, sender);

        if let Err(e) = self.conn.send(&Message::ConfirmationRequest(request)).await {
            error!("Failed to send confirmation prompt: {}", e);
            self.conn
                .confirmations
                .lock()
                .expect("confirmation map poisoned")
                .remove(&id);
            return false;
        }

        let mut cancel = self.cancel.clone();
        let answer = tokio::select! {
            // A cancel also dismisses the prompt: the tool is not approved,
            // and the turn ends instead of waiting out the timeout.
            _ = cancel.cancelled() => {
                info!("Cancelled while confirming {}", tool);
                None
            }
            result = tokio::time::timeout(CONFIRM_TIMEOUT, receiver) => match result {
                Ok(Ok(approved)) => Some(approved),
                Ok(Err(_)) => {
                    info!("Client disconnected while confirming {}", tool);
                    None
                }
                Err(_) => {
                    info!(
                        "Timed out after {}s waiting for confirmation of {}",
                        CONFIRM_TIMEOUT.as_secs(),
                        tool
                    );
                    None
                }
            },
        };

        self.conn
            .confirmations
            .lock()
            .expect("confirmation map poisoned")
            .remove(&id);
        answer.unwrap_or(false)
    }
}

async fn handle_client(
    stream: UnixStream,
    daemon: Arc<Mutex<Daemon>>,
    turns: TurnRegistry,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let conn = Conn::new(writer);
    // Requests started by this connection. If the client disappears, their
    // turns are cancelled rather than left holding the daemon lock.
    let active: Arc<std::sync::Mutex<HashSet<Uuid>>> = Default::default();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break;
        }

        match Message::decode(&line) {
            Ok(Message::Request(request)) => {
                let id = request.id;
                // Register a turn *here*, before the handler task is spawned.
                // A cancel can arrive on the very next line; if registration
                // happened inside the spawned task it could race with this
                // reader and the cancel would be lost.
                let turn = if request.method == "ask" {
                    Some(turns.begin(id))
                } else {
                    None
                };
                active.lock().expect("active set poisoned").insert(id);
                let daemon = Arc::clone(&daemon);
                let turns = turns.clone();
                let conn = conn.clone();
                let active = Arc::clone(&active);
                tokio::spawn(async move {
                    let response = handle_request(request, &daemon, &turns, turn, &conn).await;
                    active.lock().expect("active set poisoned").remove(&id);
                    if let Err(e) = conn.send(&Message::Response(response)).await {
                        error!("Failed to send response: {}", e);
                    }
                });
            }
            Ok(Message::ConfirmationReply(reply)) => {
                let sender = conn
                    .confirmations
                    .lock()
                    .expect("confirmation map poisoned")
                    .remove(&reply.id);
                match sender {
                    Some(sender) => {
                        // The confirmer may have timed out or been cancelled
                        // between our lookup and this send; that is fine.
                        let _ = sender.send(reply.approved);
                    }
                    None => info!("Ignoring reply for unknown prompt {}", reply.id),
                }
            }
            Ok(Message::Cancel(cancel)) => {
                if turns.cancel(cancel.id) {
                    info!("Cancelling turn {}", cancel.id);
                } else {
                    // The turn finished between the client deciding to
                    // cancel and the message arriving.
                    info!("Cancel for unknown or finished turn {}", cancel.id);
                }
            }
            Ok(other) => {
                error!("Ignoring unexpected message from client: {:?}", other);
            }
            Err(e) => {
                error!("Failed to parse message: {}", e);
            }
        }
    }

    for id in active.lock().expect("active set poisoned").drain() {
        turns.cancel(id);
    }
    conn.confirmations
        .lock()
        .expect("confirmation map poisoned")
        .clear();
    Ok(())
}

async fn handle_request(
    request: Request,
    daemon: &Arc<Mutex<Daemon>>,
    turns: &TurnRegistry,
    turn: Option<(Cancellation, TurnGuard)>,
    conn: &Conn,
) -> Response {
    // `ask` runs a turn and may stream, so it must not hold the reader loop
    // or be serialised behind anything else in this function.
    if request.method == "ask" {
        return handle_ask(request, daemon, turns, turn, conn).await;
    }

    let daemon_lock = daemon.lock().await;

    match request.method.as_str() {
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

/// Run one `ask` turn: register it for cancellation, stream its chunks, and
/// finish with the authoritative response.
async fn handle_ask(
    request: Request,
    daemon: &Arc<Mutex<Daemon>>,
    turns: &TurnRegistry,
    turn: Option<(Cancellation, TurnGuard)>,
    conn: &Conn,
) -> Response {
    let prompt = match request.params.get("prompt").and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => return Response::err(request.id, "Missing 'prompt' parameter".to_string()),
    };

    let id = request.id;
    // Normally registered by the reader loop. The fallback keeps the function
    // correct if it is ever called from somewhere else.
    let (cancel, _guard) = match turn {
        Some(turn) => turn,
        None => turns.begin(id),
    };

    // Chunks are queued and written by their own task so the orchestrator
    // never blocks on socket I/O. The response is sent only after this task
    // has drained, which keeps the wire order: chunks, then the answer.
    let (sink, mut chunks) = mpsc::unbounded_channel::<String>();
    let writer = conn.clone();
    let chunk_task = tokio::spawn(async move {
        while let Some(text) = chunks.recv().await {
            if let Err(e) = writer.send(&Message::Chunk(Chunk { id, text })).await {
                error!("Failed to send chunk: {}", e);
                // Keep draining: the turn still has to finish and its answer
                // must not be lost because a chunk could not be written.
            }
        }
    });

    let result = {
        let daemon_lock = daemon.lock().await;
        let confirmer = SocketConfirmer {
            conn: conn.clone(),
            cancel: cancel.clone(),
        };
        daemon_lock
            .ask_streaming(&prompt, Some(&confirmer), Some(&cancel), Some(&sink))
            .await
    };

    drop(sink);
    let _ = chunk_task.await;

    match result {
        Ok(response) => Response::ok(id, serde_json::json!({"response": response})),
        // Cancellation is a client action, not a failure of the request.
        Err(JarvisError::Cancelled) => {
            Response::ok(id, serde_json::json!({"cancelled": true, "response": ""}))
        }
        Err(e) => Response::err(id, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_targets_only_its_own_turn() {
        let turns = TurnRegistry::default();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let (a, _a_guard) = turns.begin(first);
        let (b, _b_guard) = turns.begin(second);

        turns.cancel(first);

        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
    }
}
