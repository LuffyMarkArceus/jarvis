//! The test of record for Milestone 6.5a: two windows, two real socket
//! clients, one throw-away daemon. Real use (2026-11-18): `/new` in one
//! window moved every window, and a question went into a conversation its
//! window did not show. Never against the owner's daemon: a temp data folder
//! and scripted models.

use jarvis_common::ipc::{Message, Request, Response};
use jarvis_core::config::{Config, ModelConfig, RemoteConfig};
use jarvis_core::daemon::Daemon;
use jarvis_core::eval::RecordingProvider;
use jarvis_core::ipc::server::IpcServer;
use jarvis_core::memory::MemoryManager;
use jarvis_core::orchestrator::Orchestrator;
use jarvis_core::permissions::PermissionManager;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

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
    let orch = Orchestrator::with_parts(
        config.clone(),
        Arc::new(RecordingProvider::replying("an answer")),
        permissions,
    )
    .with_remote(Some(Arc::new(RecordingProvider::replying(
        "a remote answer",
    ))));
    let daemon = Daemon::with_orchestrator(config.clone(), orch).unwrap();
    let socket = dir.join("jarvis.sock");
    let _ = std::fs::remove_file(&socket);
    let server = IpcServer::new(Arc::new(Mutex::new(daemon)), socket.clone());
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
