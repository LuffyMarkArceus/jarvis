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
use crate::confirm::choice_prompt;
use crate::daemon::Daemon;
use crate::error::{JarvisError, Result};
use crate::orchestrator::ConfirmationHandler;
use jarvis_common::ipc::{
    Choice, Chunk, ConfirmationReply, ConfirmationRequest, Message, Progress, Request, Response,
};
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
/// leave its conversation answering forever, and the safe default is "no".
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

pub struct IpcServer {
    socket_path: PathBuf,
    daemon: Arc<Daemon>,
    turns: TurnRegistry,
}

impl IpcServer {
    pub fn new(daemon: Arc<Daemon>, socket_path: PathBuf) -> Self {
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
    confirmations: Arc<std::sync::Mutex<HashMap<Uuid, oneshot::Sender<ConfirmationReply>>>>,
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
        self.ask(tool, args, &[]).await.is_some_and(|r| r.approved)
    }

    async fn choose(&self, tool: &str, args: &Value, choices: &[Choice]) -> Option<char> {
        self.ask(tool, args, choices)
            .await
            .and_then(|reply| reply.selected(choices))
    }
}

impl SocketConfirmer {
    /// Send a prompt and wait for the client's reply. `None` on cancel,
    /// disconnect or timeout - all of which mean no.
    async fn ask(&self, tool: &str, args: &Value, choices: &[Choice]) -> Option<ConfirmationReply> {
        let prompt = choice_prompt(tool, args, choices);
        let request = ConfirmationRequest {
            preview: None,
            id: Uuid::new_v4(),
            tool: tool.to_string(),
            args: args.clone(),
            prompt,
            choices: choices.to_vec(),
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
            return None;
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
                Ok(Ok(reply)) => Some(reply),
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
        answer
    }
}

async fn handle_client(stream: UnixStream, daemon: Arc<Daemon>, turns: TurnRegistry) -> Result<()> {
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
                        let _ = sender.send(reply);
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

/// The conversation a request is for. Conversation requests must say: a
/// default would be shared state under another name (Milestone 6.5a).
fn session_of(request: &Request) -> std::result::Result<String, Response> {
    match request.params.get("session").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(Response::err(
            request.id,
            format!(
                "'{}' needs a \"session\": the conversation it is for",
                request.method
            ),
        )),
    }
}

/// The reply to a model or tier switch: what the conversation uses now.
async fn switched(id: Uuid, daemon: &Daemon, session: &str) -> Response {
    let (provider, model, _) = daemon.model_info(session).await.unwrap_or_default();
    let remote = daemon.uses_remote(session).await.unwrap_or(false);
    Response::ok(
        id,
        serde_json::json!({
            "tier": if remote { "remote" } else { "local" },
            "model": model,
            "provider": provider,
            "privacy": daemon.privacy(session).await.unwrap_or_default(),
        }),
    )
}

async fn handle_request(
    request: Request,
    daemon: &Arc<Daemon>,
    turns: &TurnRegistry,
    turn: Option<(Cancellation, TurnGuard)>,
    conn: &Conn,
) -> Response {
    // `ask` runs a turn and may stream, so it must not hold the reader loop
    // or be serialised behind anything else in this function.
    if request.method == "ask" {
        return handle_ask(request, daemon, turns, turn, conn).await;
    }

    let daemon: &Daemon = daemon;

    match request.method.as_str() {
        "status" => {
            let (provider, model, base_url) = daemon.default_model().await;
            Response::ok(
                request.id,
                serde_json::json!({
                    "status": "running",
                    "provider": provider,
                    "model": model,
                    "base_url": base_url,
                    "tier": if daemon.default_remote() { "remote" } else { "local" },
                    "privacy": daemon.default_privacy().await,
                    "tools": daemon.list_tools().await,
                    "last_session": daemon.last_session(),
                    "mode": daemon.default_mode(),
                    "mode_label": daemon.default_mode().label(),
                }),
            )
        }
        "tools.list" => Response::ok(
            request.id,
            serde_json::json!({
                "tools": daemon.list_tools().await,
            }),
        ),
        "session.list" => {
            let limit = request
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;
            match daemon.list_sessions(limit).await {
                Ok(sessions) => Response::ok(request.id, serde_json::json!({"sessions": sessions})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.new" => match daemon.new_session().await {
            Ok((id, model, remote)) => {
                let mode = daemon.mode_of(&id).unwrap_or_default();
                Response::ok(
                    request.id,
                    serde_json::json!({
                        "session_id": id,
                        "model": model,
                        "tier": if remote { "remote" } else { "local" },
                        "mode": mode,
                        "mode_label": mode.label(),
                    }),
                )
            }
            Err(e) => Response::err(request.id, e.to_string()),
        },
        "session.last" => Response::ok(
            request.id,
            serde_json::json!({ "session_id": daemon.last_session() }),
        ),
        "steer" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            let id = request
                .params
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let Some(text) = request.params.get("text").and_then(Value::as_str) else {
                return Response::err(request.id, "Missing 'text' parameter".to_string());
            };
            match daemon.steer(&session, id, text) {
                Ok(took) => Response::ok(
                    request.id,
                    serde_json::json!({ "status": if took { "steering" } else { "not_running" } }),
                ),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "approve" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            use crate::orchestrator::Approve;
            let which = match request.params.get("which") {
                None => Approve::List,
                Some(Value::Number(n)) => Approve::One(n.as_u64().unwrap_or(0) as usize),
                Some(Value::String(w)) => match w.as_str() {
                    "list" | "" => Approve::List,
                    "all" => Approve::All,
                    "none" => Approve::None,
                    n => match n.parse::<usize>() {
                        Ok(n) => Approve::One(n),
                        Err(_) => {
                            return Response::err(
                                request.id,
                                format!("'{n}' is not something /approve knows: all, none, a number, or nothing to list"),
                            )
                        }
                    },
                },
                Some(other) => {
                    return Response::err(request.id, format!("{other} is not something /approve knows"))
                }
            };
            match daemon.approve(&session, which).await {
                Ok((text, remaining)) => Response::ok(
                    request.id,
                    serde_json::json!({ "text": text, "remaining": remaining }),
                ),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "mode.set" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            match daemon.set_mode(&session, &request.params) {
                Ok(mode) => {
                    let mut result =
                        serde_json::json!({ "mode": mode, "mode_label": mode.label() });
                    if mode.sandbox && !daemon.sandbox_ready() {
                        result["note"] =
                            "the sandbox is not available here: commands ask and run outside"
                                .into();
                    }
                    Response::ok(request.id, result)
                }
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "tier.set" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            let remote = request.params.get("tier").and_then(Value::as_str) == Some("remote");
            match daemon.set_remote(&session, remote).await {
                Ok(()) => switched(request.id, daemon, &session).await,
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "model.list" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            match daemon.models(&session).await {
                Ok(models) => Response::ok(request.id, serde_json::json!({ "models": models })),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "model.set" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            let query = request
                .params
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match daemon.set_model(&session, query).await {
                Ok(_) => switched(request.id, daemon, &session).await,
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "feedback" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            let kind = match request.params.get("kind").and_then(Value::as_str) {
                Some(k @ ("bad" | "good")) => k.to_string(),
                _ => return Response::err(request.id, "kind must be 'bad' or 'good'".into()),
            };
            let note = request
                .params
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match daemon.feedback(&session, &kind, note).await {
                Ok(()) => Response::ok(request.id, serde_json::json!({ "kind": kind })),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "trust.list" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            match daemon.trusted(&session).await {
                Ok(trusted) => Response::ok(request.id, serde_json::json!({ "trusted": trusted })),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "trust.clear" => {
            let session = match session_of(&request) {
                Ok(s) => s,
                Err(r) => return r,
            };
            match daemon.clear_trust(&session).await {
                Ok(()) => Response::ok(request.id, serde_json::json!({ "trusted": [] })),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.delete" => {
            let sid = match request.params.get("session_id").and_then(|p| p.as_str()) {
                Some(p) => p.to_string(),
                None => {
                    return Response::err(request.id, "Missing 'session_id' parameter".to_string())
                }
            };
            match daemon.delete_session(&sid).await {
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
            if let Err(e) = daemon.conversation(&sid) {
                return Response::err(request.id, e.to_string());
            }
            // A window opening a conversation is about to ask in it.
            daemon.warm_up(&sid).await;
            match daemon.get_session_messages(&sid).await {
                Ok(messages) => {
                    let messages: Vec<serde_json::Value> = messages
                        .iter()
                        .map(|m| {
                            let mut v = serde_json::to_value(m).unwrap_or_default();
                            if let Some(model) = &m.model {
                                v["footer"] = daemon.answer_footer(model).into();
                            }
                            v
                        })
                        .collect();
                    Response::ok(
                        request.id,
                        serde_json::json!({
                            "messages": messages,
                            "model": daemon.model_info(&sid).await.map(|m| m.1).unwrap_or_default(),
                            "tier": if daemon.uses_remote(&sid).await.unwrap_or(false) { "remote" } else { "local" },
                            "mode": daemon.mode_of(&sid).unwrap_or_default(),
                            "mode_label": daemon.mode_of(&sid).unwrap_or_default().label(),
                        }),
                    )
                }
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "session.prune" => match daemon.prune_empty_sessions().await {
            Ok(count) => Response::ok(request.id, serde_json::json!({"pruned": count})),
            Err(e) => Response::err(request.id, e.to_string()),
        },
        "memory.recall" => {
            let query = request
                .params
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = request
                .params
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as usize;
            match daemon.recall_memory(query, limit).await {
                Ok(results) => Response::ok(request.id, serde_json::json!({"results": results})),
                Err(e) => Response::err(request.id, e.to_string()),
            }
        }
        "memory.backfill" => match daemon.backfill_memory().await {
            Ok(count) => Response::ok(request.id, serde_json::json!({"indexed": count})),
            Err(e) => Response::err(request.id, e.to_string()),
        },
        "memory.stats" => match daemon.memory_stats().await {
            Ok(stats) => Response::ok(
                request.id,
                serde_json::json!({
                    "total_messages": stats.total_messages,
                    "total_sessions": stats.total_sessions,
                    "db_size_bytes": stats.db_size_bytes,
                }),
            ),
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
    daemon: &Arc<Daemon>,
    turns: &TurnRegistry,
    turn: Option<(Cancellation, TurnGuard)>,
    conn: &Conn,
) -> Response {
    let prompt = match request.params.get("prompt").and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => return Response::err(request.id, "Missing 'prompt' parameter".to_string()),
    };
    let session = match session_of(&request) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // `/go`: leave Plan mode for this turn and the ones after (6.6).
    if let Some(autonomy) = request.params.get("autonomy") {
        if let Err(e) = daemon.set_mode(&session, &serde_json::json!({ "autonomy": autonomy })) {
            return Response::err(request.id, e.to_string());
        }
    }

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
    // Progress ("asking nemotron (step 3)") goes out through the same task,
    // so it is written in order with the chunks.
    let (progress, mut steps) = mpsc::unbounded_channel::<String>();
    // Each step as it starts and ends (6.7b), through the same task too.
    let (step_tx, mut step_rx) = mpsc::unbounded_channel::<jarvis_common::ipc::StepEvent>();
    let writer = conn.clone();
    let chunk_task = tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                Some(text) = chunks.recv() => Message::Chunk(Chunk { id, text }),
                Some(text) = steps.recv() => Message::Progress(Progress { id, text }),
                Some(e) = step_rx.recv() => Message::Step(jarvis_common::ipc::Step {
                    id,
                    step: e.step,
                    kind: e.kind.to_string(),
                    name: e.name,
                    summary: e.summary,
                    state: e.state.to_string(),
                    detail: e.detail,
                    ms: e.ms,
                }),
                else => break,
            };
            if let Err(e) = writer.send(&message).await {
                error!("Failed to send chunk: {}", e);
                // Keep draining: the turn still has to finish and its answer
                // must not be lost because a chunk could not be written.
            }
        }
    });

    let (result, (unused, used)) = {
        let daemon: &Daemon = daemon;
        let confirmer = SocketConfirmer {
            conn: conn.clone(),
            cancel: cancel.clone(),
        };
        daemon
            .ask_steered(
                &session,
                &prompt,
                Some(&confirmer),
                Some(&cancel),
                Some(&sink),
                Some(&progress),
                Some(&step_tx),
            )
            .await
    };

    drop(sink);
    drop(progress);
    drop(step_tx);
    let _ = chunk_task.await;

    // The window keeps each steer it sent until told how it went (6.7a).
    // Both lists on every reply: the window settles each steer it sent.
    let unused_json: Vec<serde_json::Value> = unused
        .iter()
        .map(|s| serde_json::json!({ "id": s.id, "text": s.text }))
        .collect();
    match result {
        Ok((response, footer, timing)) => Response::ok(
            id,
            serde_json::json!({
                "response": response,
                "footer": footer,
                "timing": timing,
                "unused_steers": unused_json,
                "used_steers": used,
            }),
        ),
        // Cancellation is a client action, not a failure of the request.
        Err(JarvisError::Cancelled) => Response::ok(
            id,
            serde_json::json!({
                "cancelled": true,
                "response": "",
                "unused_steers": unused_json,
                "used_steers": used,
            }),
        ),
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
