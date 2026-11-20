//! Turns at once on one GPU (Milestone 6.5b): local model calls queue in
//! the lane, say they are waiting, and can be cancelled while they wait.
//! A gated fake model decides when the GPU is busy; nothing here touches
//! a real model.

mod common;

use common::{gated, Gate};
use jarvis_core::cancel::TurnRegistry;
use jarvis_core::config::{Config, ModelConfig, RemoteConfig};
use jarvis_core::conversation::Conversation;
use jarvis_core::eval::RecordingProvider;
use jarvis_core::orchestrator::{Orchestrator, TurnReport};
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Setup {
    dir: PathBuf,
    jarvis: Arc<Orchestrator>,
    gate: Gate,
    gated: Arc<common::Gated>,
    remote: Arc<RecordingProvider>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup_with(edit: impl FnOnce(&mut Config)) -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-gpu-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(dir.clone());
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
        spill: jarvis_core::config::Spill::Never,
        ..RemoteConfig::default()
    });
    edit(&mut config);
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    // The turns with a tool write in this throw-away folder, unasked.
    permissions
        .set_rule("filesystem.write", PermissionAction::Allow)
        .unwrap();
    let (gated, gate) = gated();
    let remote = Arc::new(RecordingProvider::replying("a remote answer"));
    let jarvis = Arc::new(
        Orchestrator::with_parts(config, gated.clone(), permissions)
            .with_remote(Some(remote.clone())),
    );
    Setup {
        dir,
        jarvis,
        gate,
        gated,
        remote,
    }
}

fn setup() -> Setup {
    setup_with(|_| {})
}

type Turn = tokio::task::JoinHandle<jarvis_core::error::Result<TurnReport>>;

/// Start a turn in its own task, reporting progress to `progress`.
fn ask(
    s: &Setup,
    conv: &Arc<Conversation>,
    input: &str,
    progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel: Option<jarvis_core::cancel::Cancellation>,
) -> Turn {
    let (jarvis, conv, input) = (s.jarvis.clone(), conv.clone(), input.to_string());
    tokio::spawn(async move {
        jarvis
            .run_turn_observed(
                &conv,
                &input,
                None,
                cancel.as_ref(),
                None,
                progress.as_ref(),
            )
            .await
    })
}

async fn line_containing(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    text: &str,
) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = rx.recv().await.expect("progress ended");
            if line.contains(text) {
                return line;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no progress line containing {text:?}"))
}

#[tokio::test]
async fn a_second_local_call_waits_and_says_so() {
    let mut s = setup();
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    assert_eq!(s.gate.reached().await, "llama3.2:1b");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let second = ask(&s, &b, "tell me a riddle", Some(tx), None);
    let line = line_containing(&mut rx, "waiting for the GPU").await;
    assert_eq!(
        line,
        "waiting for the GPU: another window is using llama3.2:1b"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    s.gate.open_once();
    assert!(first.await.unwrap().is_ok());
    s.gate.reached().await;
    s.gate.open_once();
    let report = second.await.unwrap().unwrap();
    let t = &report.timing;
    assert!(t.queue_ms >= 30, "{t:?}");
    assert!(t.jarvis_ms() + t.queue_ms <= t.total_ms, "{t:?}");
}

#[tokio::test]
async fn a_waiting_turn_can_be_cancelled_at_once() {
    let mut s = setup();
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let registry = TurnRegistry::default();
    let id = uuid::Uuid::new_v4();
    let (cancel, _guard) = registry.begin(id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let second = ask(&s, &b, "tell me a riddle", Some(tx), Some(cancel));
    line_containing(&mut rx, "waiting for the GPU").await;
    registry.cancel(id);
    let ended = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("a cancel ends the wait at once, before A finishes");
    assert!(ended.unwrap().is_err());
    assert_eq!(s.jarvis.lane().available(), 0, "A still holds the slot");
    s.gate.open_once();
    first.await.unwrap().unwrap();
    assert_eq!(s.jarvis.lane().available(), 1);
}

#[tokio::test]
async fn a_failed_call_releases_its_slot() {
    let mut s = setup();
    let a = s.jarvis.new_conversation();
    s.gated
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    s.gate.open_once();
    let _ = first.await.unwrap();
    assert_eq!(s.jarvis.lane().available(), 1);
}

#[tokio::test]
async fn settings_are_pinned_for_the_turn() {
    // A turn of two model calls: a tool call, then the answer. A switch
    // made during the first must not reach the second.
    let mut s = setup();
    s.gated.script.lock().unwrap().push_back(common::call(
        "filesystem.write",
        serde_json::json!({ "path": s.dir.join("notes.md"), "content": "hi" }),
    ));
    let a = s.jarvis.new_conversation();
    let first = ask(
        &s,
        &a,
        "create a file called notes.md containing the word hi",
        None,
        None,
    );
    assert_eq!(s.gate.reached().await, "llama3.2:1b");
    s.jarvis.set_model(&a, "qwen").unwrap();
    s.gate.open_once();
    assert_eq!(
        s.gate.reached().await,
        "llama3.2:1b",
        "the turn kept its model"
    );
    s.gate.open_once();
    first.await.unwrap().unwrap();
    let next = ask(&s, &a, "tell me another joke", None, None);
    assert_eq!(s.gate.reached().await, "qwen2.5:3b");
    s.gate.open_once();
    next.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_remote_conversation_does_not_queue() {
    let mut s = setup();
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    s.jarvis.set_remote(&b, true).unwrap();
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let remote = tokio::time::timeout(
        Duration::from_secs(2),
        ask(&s, &b, "tell me a riddle", None, None),
    )
    .await
    .expect("a remote turn does not wait for the GPU");
    assert_eq!(remote.unwrap().unwrap().answer, "a remote answer");
    assert_eq!(s.remote.requests().len(), 1);
    s.gate.open_once();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn slots_two_runs_two_local_calls_together() {
    let mut s = setup_with(|c| c.model.slots = 2);
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    let second = ask(&s, &b, "tell me a riddle", None, None);
    s.gate.reached().await;
    s.gate.reached().await;
    s.gate.open_once();
    s.gate.open_once();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_long_wait_is_logged_as_queued() {
    let mut s = setup_with(|c| c.remote.as_mut().unwrap().spill_after = 1);
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let second = ask(&s, &b, "tell me a riddle", None, None);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    s.gate.open_once();
    first.await.unwrap().unwrap();
    s.gate.reached().await;
    s.gate.open_once();
    second.await.unwrap().unwrap();
    let log = std::fs::read_to_string(s.dir.join("turns.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let signals = |i: usize| lines[i]["signals"].as_array().unwrap().clone();
    assert!(!signals(0).contains(&serde_json::json!("queued")));
    assert!(
        signals(1).contains(&serde_json::json!("queued")),
        "{:?}",
        lines[1]
    );
}

#[tokio::test]
async fn no_model_is_loaded_ahead_while_the_gpu_is_busy() {
    let mut s = setup();
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    s.jarvis.set_model(&b, "qwen").unwrap();
    s.jarvis.warm_up(&b);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        s.gated.warmed.lock().unwrap().is_empty(),
        "loading qwen would evict llama"
    );
    s.gate.open_once();
    first.await.unwrap().unwrap();
    s.jarvis.warm_up(&b);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*s.gated.warmed.lock().unwrap(), vec!["qwen2.5:3b"]);
}
