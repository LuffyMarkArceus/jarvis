//! The test of record for Milestone 6.5a: two windows, two real socket
//! clients, one throw-away daemon. Real use (2026-11-18): `/new` in one
//! window moved every window, and a question went into a conversation its
//! window did not show. Never against the owner's daemon: a temp data folder
//! and scripted models.

mod common;

use jarvis_common::ipc::{ConfirmationReply, Message, Request, Response};
use jarvis_core::config::{Config, ModelConfig, RemoteConfig};
use jarvis_core::daemon::Daemon;
use jarvis_core::eval::RecordingProvider;
use jarvis_core::ipc::server::IpcServer;
use jarvis_core::memory::MemoryManager;
use jarvis_core::model::ChatTurn;
use jarvis_core::orchestrator::Orchestrator;
use jarvis_core::permissions::PermissionAction;
use jarvis_core::permissions::PermissionManager;
use jarvis_core::provider::{ChatRequest, Provider};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Answers by what was asked: a request naming notes.md writes it (which
/// prompts), "fail" is a model error, anything else is "an answer".
struct ByPrompt {
    dir: PathBuf,
}

#[async_trait::async_trait]
impl Provider for ByPrompt {
    fn name(&self) -> &'static str {
        "by-prompt"
    }

    async fn chat_turn(
        &self,
        request: ChatRequest<'_>,
        _cancel: Option<&jarvis_core::cancel::Cancellation>,
    ) -> jarvis_core::error::Result<ChatTurn> {
        let last = request.messages.last();
        let content = last.map(|m| m.content.as_str()).unwrap_or_default();
        if last.is_some_and(|m| m.role == "tool") {
            return Ok(common::answer("done"));
        }
        if content.contains("fail") {
            return Err(jarvis_core::error::JarvisError::Config(
                "the model fell over".into(),
            ));
        }
        if content.contains("notes.md") && request.tools.is_some() {
            return Ok(common::call(
                "filesystem.write",
                json!({ "path": self.dir.join("notes.md"), "content": "hi" }),
            ));
        }
        Ok(common::answer("an answer"))
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&jarvis_core::cancel::Cancellation>,
        _sink: &tokio::sync::mpsc::UnboundedSender<String>,
    ) -> jarvis_core::error::Result<ChatTurn> {
        self.chat_turn(request, cancel).await
    }
}

const WRITE: &str = "create a file called notes.md containing the word hi";

struct Home {
    dir: PathBuf,
    config: Config,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config_in(dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.general.data_dir = dir.to_path_buf();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(dir.to_path_buf());
    config.model.default_model = "llama3.2:1b".into();
    config.model.models = vec!["qwen2.5:3b".into()];
    config.remote = Some(RemoteConfig {
        model: ModelConfig {
            provider: "openai".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            default_model: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
            data_policy: Some("may_train".into()),
            ..ModelConfig::default()
        },
        default: false,
        ..RemoteConfig::default()
    });
    config
}

/// Start a daemon over `dir` (new or existing) and listen on its socket.
async fn start(dir: PathBuf) -> Home {
    std::fs::create_dir_all(&dir).unwrap();
    let config = config_in(&dir);
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    permissions
        .set_rule("filesystem.write", PermissionAction::Ask)
        .unwrap();
    let orch = Orchestrator::with_parts(
        config.clone(),
        Arc::new(ByPrompt { dir: dir.clone() }),
        permissions,
    )
    .with_remote(Some(Arc::new(RecordingProvider::replying(
        "a remote answer",
    ))));
    let daemon = Daemon::with_orchestrator(config.clone(), orch).unwrap();
    let socket = dir.join("jarvis.sock");
    let _ = std::fs::remove_file(&socket);
    let server = IpcServer::new(Arc::new(daemon), socket.clone());
    let task = tokio::spawn(async move {
        let _ = server.start().await;
    });
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Home {
        dir,
        config,
        server: Some(task),
    }
}

async fn fresh() -> Home {
    start(std::env::temp_dir().join(format!("jarvis-windows-{}", uuid::Uuid::new_v4()))).await
}

/// One window: a connection to the daemon.
struct Window {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    session: String,
}

impl Window {
    async fn open(home: &Home) -> Window {
        let stream = UnixStream::connect(home.dir.join("jarvis.sock"))
            .await
            .unwrap();
        let (read, writer) = stream.into_split();
        let mut w = Window {
            reader: BufReader::new(read),
            writer,
            session: String::new(),
        };
        let new = w.call("session.new", json!({})).await;
        assert!(new.ok, "{:?}", new.error);
        w.session = new.result.unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        w
    }

    async fn call(&mut self, method: &str, params: Value) -> Response {
        let request = Request::new(method, params);
        let id = request.id;
        let line = Message::Request(request).encode() + "\n";
        self.writer.write_all(line.as_bytes()).await.unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.unwrap();
            if let Ok(Message::Response(r)) = Message::decode(line.trim()) {
                if r.id == id {
                    return r;
                }
            }
        }
    }

    /// Send without waiting for the answer.
    async fn send(&mut self, method: &str, mut params: Value) -> uuid::Uuid {
        params["session"] = json!(self.session);
        let request = Request::new(method, params);
        let id = request.id;
        let line = Message::Request(request).encode() + "\n";
        self.writer.write_all(line.as_bytes()).await.unwrap();
        id
    }

    async fn next_message(&mut self) -> Message {
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.unwrap();
            if let Ok(message) = Message::decode(line.trim()) {
                return message;
            }
        }
    }

    /// Ask for a write and stop at its prompt; the prompt's id.
    async fn stop_at_a_prompt(&mut self) -> (uuid::Uuid, uuid::Uuid) {
        let ask = self.send("ask", json!({ "prompt": WRITE })).await;
        loop {
            match self.next_message().await {
                Message::ConfirmationRequest(c) => return (ask, c.id),
                Message::Response(r) if r.id == ask => panic!("no prompt: {r:?}"),
                _ => {}
            }
        }
    }

    async fn reply(&mut self, prompt: uuid::Uuid, approved: bool) {
        let line = Message::ConfirmationReply(ConfirmationReply {
            reason: None,
            id: prompt,
            approved,
            choice: None,
        })
        .encode()
            + "\n";
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn response_to(&mut self, id: uuid::Uuid) -> Response {
        loop {
            if let Message::Response(r) = self.next_message().await {
                if r.id == id {
                    return r;
                }
            }
        }
    }

    /// A conversation request, in this window's conversation.
    async fn in_mine(&mut self, method: &str, mut params: Value) -> Response {
        params["session"] = json!(self.session);
        self.call(method, params).await
    }
}

fn stored(home: &Home, session: &str) -> Vec<String> {
    MemoryManager::new(home.config.clone())
        .unwrap()
        .get_recent_messages(session, 50)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content)
        .collect()
}

fn current_model(r: &Response) -> String {
    r.result.as_ref().unwrap()["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["current"] == true)
        .map(|m| m["name"].as_str().unwrap().to_string())
        .unwrap()
}

#[tokio::test]
async fn a_window_asks_in_its_own_conversation() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    assert_ne!(a.session, b.session, "each window starts its own");
    let asked = b
        .in_mine("ask", json!({ "prompt": "B asks: what is 3 x 3?" }))
        .await;
    assert!(asked.ok, "{:?}", asked.error);
    assert_eq!(stored(&home, &b.session), vec!["B asks: what is 3 x 3?"]);
    assert!(
        stored(&home, &a.session).is_empty(),
        "A's conversation is untouched"
    );
    let _ = a.in_mine("model.list", json!({})).await;
}

#[tokio::test]
async fn a_model_switch_stays_in_its_conversation() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    let set = a.in_mine("model.set", json!({ "model": "qwen" })).await;
    assert!(set.ok, "{:?}", set.error);
    assert_eq!(
        current_model(&a.in_mine("model.list", json!({})).await),
        "qwen2.5:3b"
    );
    assert_eq!(
        current_model(&b.in_mine("model.list", json!({})).await),
        "llama3.2:1b"
    );
}

#[tokio::test]
async fn trust_and_feedback_stay_in_their_conversation() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    assert!(a.in_mine("ask", json!({ "prompt": "hello" })).await.ok);
    assert!(
        a.in_mine("feedback", json!({ "kind": "bad", "note": "" }))
            .await
            .ok
    );
    let b_bad = b
        .in_mine("feedback", json!({ "kind": "bad", "note": "" }))
        .await;
    assert!(!b_bad.ok, "B has no turn of its own to flag");
    let trusted = b.in_mine("trust.list", json!({})).await;
    assert_eq!(trusted.result.unwrap()["trusted"], json!([]));
}

#[tokio::test]
async fn a_conversation_request_without_a_session_is_refused() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    for method in [
        "ask",
        "model.list",
        "model.set",
        "tier.set",
        "feedback",
        "trust.list",
    ] {
        let r = a
            .call(
                method,
                json!({ "prompt": "hi", "model": "qwen", "tier": "local", "kind": "bad" }),
            )
            .await;
        assert!(!r.ok, "{method} ran without a session");
        assert!(r.error.unwrap_or_default().contains("session"), "{method}");
    }
}

#[tokio::test]
async fn a_conversation_survives_a_daemon_restart() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    assert!(a.in_mine("model.set", json!({ "model": "qwen" })).await.ok);
    assert!(
        a.in_mine("ask", json!({ "prompt": "remember me" }))
            .await
            .ok
    );
    let session = a.session.clone();
    let dir = home.dir.clone();
    let mut home = home;
    home.server.take().unwrap().abort();
    std::mem::forget(home); // the folder lives on for the new daemon
    let again = start(dir).await;
    let mut a = Window::open(&again).await;
    a.session = session.clone();
    let shown = a
        .in_mine("session.show", json!({ "session_id": session }))
        .await;
    assert!(shown.ok, "{:?}", shown.error);
    assert_eq!(
        current_model(&a.in_mine("model.list", json!({})).await),
        "qwen2.5:3b"
    );
    assert!(a.in_mine("ask", json!({ "prompt": "again" })).await.ok);
    assert_eq!(stored(&again, &session), vec!["remember me", "again"]);
}

#[tokio::test]
async fn a_deleted_conversation_is_not_recreated() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    let gone = a.session.clone();
    assert!(
        b.call("session.delete", json!({ "session_id": gone }))
            .await
            .ok
    );
    let asked = a.in_mine("ask", json!({ "prompt": "hello?" })).await;
    assert!(!asked.ok, "a deleted conversation was used");
    assert!(asked.error.unwrap_or_default().contains(&gone));
    assert!(stored(&home, &gone).is_empty());
}

#[tokio::test]
async fn two_windows_on_one_conversation_share_its_model() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    b.session = a.session.clone(); // /switch to A's conversation
    assert!(a.in_mine("model.set", json!({ "model": "qwen" })).await.ok);
    assert_eq!(
        current_model(&b.in_mine("model.list", json!({})).await),
        "qwen2.5:3b"
    );
}

#[tokio::test]
async fn pruning_leaves_open_conversations_alone() {
    // A window just opened is an empty conversation; /prune in another
    // window must not delete it out from under it.
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    assert!(b.call("session.prune", json!({})).await.ok);
    let asked = a.in_mine("ask", json!({ "prompt": "still here?" })).await;
    assert!(asked.ok, "{:?}", asked.error);
}

#[tokio::test]
async fn a_reopened_conversation_says_which_model_wrote_each_answer() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    assert!(a.in_mine("ask", json!({ "prompt": "one" })).await.ok);
    assert!(
        a.in_mine("model.set", json!({ "model": "nemotron" }))
            .await
            .ok
    );
    assert!(a.in_mine("ask", json!({ "prompt": "two" })).await.ok);
    let session = a.session.clone();
    let shown = a
        .in_mine("session.show", json!({ "session_id": session }))
        .await;
    let footers: Vec<Value> = shown.result.unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["footer"].clone())
        .collect();
    assert_eq!(
        footers,
        vec![
            json!("llama3.2:1b · local"),
            json!("nvidia/nemotron-3-ultra-550b-a55b:free · remote")
        ]
    );
}

#[tokio::test]
async fn status_reports_the_default_tier_and_last_conversation() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    assert!(a.in_mine("ask", json!({ "prompt": "hi" })).await.ok);
    let status = a.call("status", json!({})).await.result.unwrap();
    assert_eq!(status["tier"], "local");
    assert_eq!(status["last_session"], json!(a.session));
}

fn soon() -> std::time::Duration {
    std::time::Duration::from_secs(2)
}

#[tokio::test]
async fn a_window_at_a_prompt_does_not_block_another() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    let (ask, prompt) = a.stop_at_a_prompt().await;
    for (method, params) in [
        ("model.list", json!({})),
        ("ask", json!({ "prompt": "what is 2 + 2?" })),
    ] {
        let r = tokio::time::timeout(soon(), b.in_mine(method, params))
            .await
            .unwrap_or_else(|_| panic!("{method} waited for A's prompt"));
        assert!(r.ok, "{method}: {:?}", r.error);
    }
    let status = tokio::time::timeout(soon(), b.call("status", json!({})))
        .await
        .expect("status waited for A's prompt");
    assert!(status.ok);
    a.reply(prompt, false).await;
    let answered = tokio::time::timeout(soon(), a.response_to(ask))
        .await
        .expect("A's turn ends after its prompt");
    assert!(answered.ok, "{:?}", answered.error);
}

#[tokio::test]
async fn a_second_question_in_an_answering_conversation_is_refused() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    b.session = a.session.clone();
    let (ask, prompt) = a.stop_at_a_prompt().await;
    let refused = tokio::time::timeout(soon(), b.in_mine("ask", json!({ "prompt": "hello" })))
        .await
        .expect("refused at once");
    assert_eq!(
        refused.error.as_deref(),
        Some("This conversation is answering another question. Wait for it, or open another with /new.")
    );
    a.reply(prompt, false).await;
    assert!(a.response_to(ask).await.ok);
    assert!(b.in_mine("ask", json!({ "prompt": "hello" })).await.ok);
}

#[tokio::test]
async fn an_answering_conversation_cannot_be_deleted() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    let (ask, prompt) = a.stop_at_a_prompt().await;
    let id = a.session.clone();
    let refused = tokio::time::timeout(
        soon(),
        b.call("session.delete", json!({ "session_id": id })),
    )
    .await
    .expect("answered at once");
    assert_eq!(
        refused.error,
        Some(format!(
            "Conversation {id} is answering a question; delete it after."
        ))
    );
    a.reply(prompt, false).await;
    assert!(a.response_to(ask).await.ok);
}

#[tokio::test]
async fn the_marker_is_released_when_a_turn_fails() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let _ = a.in_mine("ask", json!({ "prompt": "fail now" })).await;
    let again = a.in_mine("ask", json!({ "prompt": "hello" })).await;
    assert!(again.ok, "{:?}", again.error);
}

#[tokio::test]
async fn a_mode_change_stays_in_its_conversation() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let mut b = Window::open(&home).await;
    let set = a
        .in_mine("mode.set", json!({ "approval": "assisted" }))
        .await;
    assert!(set.ok, "{:?}", set.error);
    assert_eq!(
        set.result.as_ref().unwrap()["mode_label"],
        "Interactive · Assisted · Sandbox on"
    );
    let a_session = a.session.clone();
    let shown_a = a
        .in_mine("session.show", json!({ "session_id": a_session }))
        .await;
    assert_eq!(shown_a.result.unwrap()["mode"]["approval"], "assisted");
    let b_session = b.session.clone();
    let shown_b = b
        .in_mine("session.show", json!({ "session_id": b_session }))
        .await;
    assert_eq!(shown_b.result.unwrap()["mode"]["approval"], "manual");
    let status = a.call("status", json!({})).await.result.unwrap();
    assert_eq!(status["mode_label"], "Interactive · Manual · Sandbox on");
    let bad = a.in_mine("mode.set", json!({ "approval": "yolo" })).await;
    assert!(!bad.ok);
}

#[tokio::test]
async fn a_new_window_is_told_its_mode() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let new = a.call("session.new", json!({})).await.result.unwrap();
    assert_eq!(new["mode"]["autonomy"], "interactive");
    assert_eq!(new["mode_label"], "Interactive · Manual · Sandbox on");
}

#[tokio::test]
async fn approve_is_a_conversation_request() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let listed = a.in_mine("approve", json!({ "which": "list" })).await;
    assert!(listed.ok, "{:?}", listed.error);
    assert_eq!(
        listed.result.as_ref().unwrap()["text"],
        "Nothing is waiting for /approve."
    );
    assert_eq!(listed.result.unwrap()["remaining"], 0);
    let bad = a.in_mine("approve", json!({ "which": "some" })).await;
    assert!(!bad.ok);
    let no_session = a.call("approve", json!({ "which": "all" })).await;
    assert!(!no_session.ok);
}

#[tokio::test]
async fn go_switches_out_of_plan_and_carries_on() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    assert!(
        a.in_mine("mode.set", json!({ "autonomy": "plan" }))
            .await
            .ok
    );
    // `/go`: the same conversation, now Interactive - so the write asks
    // (Plan would have refused it without a prompt).
    let ask = a
        .send("ask", json!({ "prompt": WRITE, "autonomy": "interactive" }))
        .await;
    let prompt = loop {
        match a.next_message().await {
            Message::ConfirmationRequest(c) => break c.id,
            Message::Response(r) if r.id == ask => panic!("no prompt: {r:?}"),
            _ => {}
        }
    };
    a.reply(prompt, false).await;
    assert!(a.response_to(ask).await.ok);
    let session = a.session.clone();
    let shown = a
        .in_mine("session.show", json!({ "session_id": session }))
        .await;
    assert_eq!(shown.result.unwrap()["mode"]["autonomy"], "interactive");
}

#[tokio::test]
async fn steer_over_ipc() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let idle = a
        .in_mine("steer", json!({ "id": "s1", "text": "use Pictures" }))
        .await;
    assert!(idle.ok, "{:?}", idle.error);
    assert_eq!(idle.result.unwrap()["status"], "not_running");
    let no_session = a.call("steer", json!({ "id": "s1", "text": "x" })).await;
    assert!(!no_session.ok);
    let answered = a.in_mine("ask", json!({ "prompt": "hello" })).await;
    assert_eq!(answered.result.unwrap()["unused_steers"], json!([]));
}

#[tokio::test]
async fn a_conversation_with_an_extra_message_still_gets_a_title() {
    // A steer adds a user message mid-turn; titling must not need exactly 2.
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    MemoryManager::new(home.config.clone())
        .unwrap()
        .add_message(&a.session, "user", "an earlier steer")
        .unwrap();
    assert!(
        a.in_mine("ask", json!({ "prompt": "plan my week" }))
            .await
            .ok
    );
    let listed = a.call("session.list", json!({})).await.result.unwrap();
    let mine = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == json!(a.session))
        .cloned()
        .unwrap();
    assert_ne!(mine["title"], "New Session", "{mine}");
}

#[tokio::test]
async fn steps_reach_the_window() {
    let home = fresh().await;
    let mut a = Window::open(&home).await;
    let (ask, prompt) = a.stop_at_a_prompt().await;
    a.reply(prompt, false).await;
    let mut states = Vec::new();
    loop {
        match a.next_message().await {
            Message::Step(step) => {
                assert_eq!(step.id, ask);
                states.push((step.kind.clone(), step.state.clone()));
            }
            Message::Response(r) if r.id == ask => break,
            _ => {}
        }
    }
    assert!(
        states.contains(&("tool".to_string(), "declined".to_string())),
        "{states:?}"
    );
}
