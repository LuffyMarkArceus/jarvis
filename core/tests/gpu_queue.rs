//! Turns at once on one GPU (Milestone 6.5b): local model calls queue in
//! the lane, say they are waiting, and can be cancelled while they wait.
//! A gated fake model decides when the GPU is busy; nothing here touches
//! a real model.

mod common;

use common::{gated, Gate};
use jarvis_core::cancel::TurnRegistry;
use jarvis_core::config::Spill;
use jarvis_core::config::{Config, ModelConfig, RemoteConfig};
use jarvis_core::conversation::Conversation;
use jarvis_core::eval::RecordingProvider;
use jarvis_core::orchestrator::{footer, ConfirmationHandler, Orchestrator, TurnReport};
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Setup {
    dir: PathBuf,
    config: Config,
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
    let kept = config.clone();
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
        config: kept,
        jarvis,
        gate,
        gated,
        remote,
    }
}

impl Setup {
    fn jarvis_config(&self) -> Config {
        self.config.clone()
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
    ask_with(s, conv, input, progress, cancel, None)
}

fn ask_with(
    s: &Setup,
    conv: &Arc<Conversation>,
    input: &str,
    progress: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel: Option<jarvis_core::cancel::Cancellation>,
    owner: Option<Arc<Owner>>,
) -> Turn {
    let (jarvis, conv, input) = (s.jarvis.clone(), conv.clone(), input.to_string());
    tokio::spawn(async move {
        let confirm = owner.as_deref().map(|o| o as &dyn ConfirmationHandler);
        jarvis
            .run_turn_observed(
                &conv,
                &input,
                confirm,
                cancel.as_ref(),
                None,
                progress.as_ref(),
                None,
            )
            .await
    })
}

/// Answers every prompt `yes` or `no`, and keeps what it was asked.
struct Owner {
    yes: bool,
    /// The one prompt answered the other way.
    except: Option<&'static str>,
    asked: std::sync::Mutex<Vec<String>>,
}

impl Owner {
    fn saying(yes: bool) -> Arc<Owner> {
        Arc::new(Owner {
            yes,
            except: None,
            asked: Default::default(),
        })
    }

    /// Yes to everything but `tool`.
    fn refusing(tool: &'static str) -> Arc<Owner> {
        Arc::new(Owner {
            yes: true,
            except: Some(tool),
            asked: Default::default(),
        })
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ConfirmationHandler for Owner {
    async fn confirm(&self, tool: &str, _args: &serde_json::Value) -> bool {
        self.asked.lock().unwrap().push(tool.to_string());
        self.yes != (self.except == Some(tool))
    }
}

fn spilling(spill: Spill) -> impl FnOnce(&mut Config) {
    move |c| {
        let remote = c.remote.as_mut().unwrap();
        remote.spill = spill;
        remote.spill_after = 1;
    }
}

/// Run a turn that may or may not reach the model, opening the gate if it
/// does.
async fn finish(s: &mut Setup, turn: Turn) -> TurnReport {
    tokio::pin!(turn);
    tokio::select! {
        done = &mut turn => return done.unwrap().unwrap(),
        _ = s.gate.reached() => s.gate.open_once(),
    }
    turn.await.unwrap().unwrap()
}

fn log_signals(s: &Setup) -> Vec<Vec<String>> {
    std::fs::read_to_string(s.dir.join("turns.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|l| l["type"] == "turn")
        .map(|l| {
            l["signals"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        })
        .collect()
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

#[tokio::test]
async fn a_busy_gpu_spills_to_remote_after_the_wait() {
    let mut s = setup_with(spilling(Spill::Auto));
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let second = ask(&s, &b, "tell me a riddle", Some(tx), None);
    let line = line_containing(&mut rx, "GPU busy").await;
    assert_eq!(
        line,
        "GPU busy: answering with nemotron-3-ultra-550b-a55b (remote)"
    );
    let report = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the spilled turn does not wait for A")
        .unwrap()
        .unwrap();
    // Live check, 6.5b: the wait before a spill was not counted as waiting.
    assert!(report.timing.queue_ms >= 1000, "{:?}", report.timing);
    assert_eq!(report.answer, "a remote answer");
    assert!(
        footer(&report).ends_with(" (GPU busy)"),
        "{}",
        footer(&report)
    );
    assert!(!b.remote(), "the conversation stays local");
    let stored = jarvis_core::memory::MemoryManager::new(s.jarvis_config())
        .unwrap()
        .get_recent_messages(&b.id, 10)
        .unwrap();
    assert_eq!(
        stored.last().unwrap().model.as_deref(),
        Some("nvidia/nemotron-3-ultra-550b-a55b:free")
    );
    s.gate.open_once();
    first.await.unwrap().unwrap();
    assert!(log_signals(&s)
        .iter()
        .any(|l| l.contains(&"spilled".to_string())));
    let next = ask(&s, &b, "tell me another riddle", None, None);
    assert_eq!(
        s.gate.reached().await,
        "llama3.2:1b",
        "the next turn tries local"
    );
    s.gate.open_once();
    next.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_spill_still_asks_before_sending_a_local_file() {
    let mut s = setup_with(spilling(Spill::Auto));
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let secret = s.dir.join("outside.txt");
    std::fs::write(&secret, "my private notes").unwrap();
    let read = ask(&s, &b, &format!("read {}", secret.display()), None, None);
    finish(&mut s, read).await;
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let no = Owner::saying(false);
    let second = ask_with(
        &s,
        &b,
        "what was in that file?",
        None,
        None,
        Some(no.clone()),
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        no.asked().contains(&"egress.send".to_string()),
        "{:?}",
        no.asked()
    );
    assert!(s.remote.requests().is_empty(), "nothing was sent");
    s.gate.open_once();
    first.await.unwrap().unwrap();
    assert_eq!(s.gate.reached().await, "llama3.2:1b");
    s.gate.open_once();
    let report = second.await.unwrap().unwrap();
    assert!(!footer(&report).contains("GPU busy"), "{}", footer(&report));
    let signals = log_signals(&s);
    assert!(
        signals
            .last()
            .unwrap()
            .contains(&"spill_withheld".to_string()),
        "{signals:?}"
    );
    assert!(!signals.last().unwrap().contains(&"spilled".to_string()));
}

#[tokio::test]
async fn a_spill_leaves_out_recall_the_policy_would_block() {
    let mut s = setup_with(|c| {
        spilling(Spill::Auto)(c);
        c.memory.recall_enabled = true;
    });
    jarvis_core::memory::MemoryManager::new(s.jarvis_config())
        .unwrap()
        .add_message(
            "old",
            "assistant",
            "the zebra plans are secret, per notes.txt",
        )
        .unwrap();
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let owner = Owner::saying(true);
    let second = ask_with(
        &s,
        &b,
        "what are the zebra plans?",
        None,
        None,
        Some(owner.clone()),
    );
    let report = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("spilled")
        .unwrap()
        .unwrap();
    assert_eq!(report.answer, "a remote answer");
    let sent = s.remote.requests().join("\n");
    assert!(
        !sent.contains("per notes.txt"),
        "recall the policy blocks was sent: {sent}"
    );
    assert!(owner.asked().is_empty(), "{:?}", owner.asked());
    s.gate.open_once();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn spill_never_waits_for_the_gpu() {
    let mut s = setup_with(spilling(Spill::Never));
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let second = ask(&s, &b, "tell me a riddle", None, None);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(s.remote.requests().is_empty());
    s.gate.open_once();
    first.await.unwrap().unwrap();
    s.gate.reached().await;
    s.gate.open_once();
    assert_eq!(
        second.await.unwrap().unwrap().answer,
        "answered by llama3.2:1b"
    );
}

#[tokio::test]
async fn spill_ask_with_a_no_keeps_its_place() {
    let mut s = setup_with(spilling(Spill::Ask));
    let (a, b, c) = (
        s.jarvis.new_conversation(),
        s.jarvis.new_conversation(),
        s.jarvis.new_conversation(),
    );
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let no = Owner::saying(false);
    let second = ask_with(&s, &b, "tell me a riddle", None, None, Some(no.clone()));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(no.asked(), vec!["egress.spill"]);
    let third = ask(&s, &c, "tell me a story", None, None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    s.gate.open_once();
    first.await.unwrap().unwrap();
    s.gate.reached().await;
    s.gate.open_once();
    let b_done = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("B ran before C");
    assert!(b_done.unwrap().is_ok());
    s.gate.reached().await;
    s.gate.open_once();
    third.await.unwrap().unwrap();
    assert!(s.remote.requests().is_empty());
}

#[tokio::test]
async fn a_spill_to_a_failing_remote_reports_the_error() {
    let mut s = setup_with(spilling(Spill::Auto));
    let failing = Arc::new(Failing);
    let jarvis = Orchestrator::with_parts(
        s.jarvis_config(),
        s.gated.clone(),
        PermissionManager::with_paths(s.dir.join("permissions.toml"), s.dir.join("audit.db"))
            .unwrap(),
    )
    .with_remote(Some(failing));
    s.jarvis = Arc::new(jarvis);
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let second = ask(&s, &b, "tell me a riddle", None, None);
    let report = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("a failing remote does not hang the turn")
        .unwrap()
        .unwrap();
    assert!(report.model_error.is_some(), "{report:?}");
    s.gate.open_once();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_local_remote_tier_is_not_a_spill() {
    let mut s = setup_with(|c| {
        spilling(Spill::Auto)(c);
        c.remote.as_mut().unwrap().model.base_url = "http://127.0.0.1:11434".into();
    });
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let second = ask(&s, &b, "tell me a riddle", None, None);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        s.remote.requests().is_empty(),
        "a local [remote] is no spill"
    );
    s.gate.open_once();
    first.await.unwrap().unwrap();
    s.gate.reached().await;
    s.gate.open_once();
    second.await.unwrap().unwrap();
}

/// A remote model that is down.
struct Failing;

#[async_trait::async_trait]
impl jarvis_core::provider::Provider for Failing {
    fn name(&self) -> &'static str {
        "failing"
    }

    async fn chat_turn(
        &self,
        _r: jarvis_core::provider::ChatRequest<'_>,
        _c: Option<&jarvis_core::cancel::Cancellation>,
    ) -> jarvis_core::error::Result<jarvis_core::model::ChatTurn> {
        Err(jarvis_core::error::JarvisError::Model(
            "503 upstream".into(),
        ))
    }

    async fn chat_turn_streaming(
        &self,
        r: jarvis_core::provider::ChatRequest<'_>,
        c: Option<&jarvis_core::cancel::Cancellation>,
        _s: &tokio::sync::mpsc::UnboundedSender<String>,
    ) -> jarvis_core::error::Result<jarvis_core::model::ChatTurn> {
        self.chat_turn(r, c).await
    }
}

#[tokio::test]
async fn every_step_of_a_spilled_turn_gets_the_remote_prompt() {
    // Review of 6.5b: only the first spilled call used the prompt with
    // recall filtered for remote; step 2 sent the local one and asked.
    let mut s = setup_with(|c| {
        spilling(Spill::Auto)(c);
        c.memory.recall_enabled = true;
    });
    jarvis_core::memory::MemoryManager::new(s.jarvis_config())
        .unwrap()
        .add_message(
            "old",
            "assistant",
            "the zebra plans are secret, per notes.txt",
        )
        .unwrap();
    let remote = Arc::new(common::Script::new(vec![
        common::call(
            "filesystem.write",
            serde_json::json!({ "path": s.dir.join("notes.md"), "content": "zebra" }),
        ),
        common::answer("written"),
    ]));
    s.jarvis = Arc::new(
        Orchestrator::with_parts(
            s.jarvis_config(),
            s.gated.clone(),
            PermissionManager::with_paths(s.dir.join("permissions.toml"), s.dir.join("audit.db"))
                .unwrap(),
        )
        .with_remote(Some(remote.clone())),
    );
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let owner = Owner::saying(true);
    let second = ask_with(
        &s,
        &b,
        "create a file called notes.md with the zebra plans",
        None,
        None,
        Some(owner.clone()),
    );
    let report = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("spilled")
        .unwrap()
        .unwrap();
    let sent = remote.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "both steps went remote: {report:?}");
    assert!(
        sent.iter().all(|r| !r.contains("per notes.txt")),
        "a spilled step carried recall the policy blocks"
    );
    // Prompts about this turn's own write are fine; one about memory, or
    // the "answer locally?" offer a spill must never make, is not.
    assert!(
        !owner.asked().contains(&"egress.local".to_string()),
        "{:?}",
        owner.asked()
    );
    s.gate.open_once();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_refused_later_step_of_a_spill_waits_for_the_gpu() {
    // Step 2 of a spill may not go (the owner says no): back to the GPU
    // queue, not the "answer locally?" offer, and logged spill_withheld.
    let mut s = setup_with(|c| {
        spilling(Spill::Auto)(c);
        c.memory.recall_enabled = true;
    });
    jarvis_core::memory::MemoryManager::new(s.jarvis_config())
        .unwrap()
        .add_message(
            "old",
            "assistant",
            "the zebra plans are secret, per notes.txt",
        )
        .unwrap();
    let remote = Arc::new(common::Script::new(vec![
        common::call(
            "filesystem.write",
            serde_json::json!({ "path": s.dir.join("notes.md"), "content": "zebra" }),
        ),
        common::answer("written"),
    ]));
    s.jarvis = Arc::new(
        Orchestrator::with_parts(
            s.jarvis_config(),
            s.gated.clone(),
            PermissionManager::with_paths(s.dir.join("permissions.toml"), s.dir.join("audit.db"))
                .unwrap(),
        )
        .with_remote(Some(remote.clone())),
    );
    let (a, b) = (s.jarvis.new_conversation(), s.jarvis.new_conversation());
    let first = ask(&s, &a, "tell me a joke", None, None);
    s.gate.reached().await;
    let owner = Owner::refusing("egress.send");
    let second = ask_with(
        &s,
        &b,
        "create a file called notes.md with the zebra plans",
        None,
        None,
        Some(owner.clone()),
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(remote.sent.lock().unwrap().len(), 1, "step 2 was not sent");
    assert!(!second.is_finished(), "it waits for the GPU");
    s.gate.open_once();
    first.await.unwrap().unwrap();
    assert_eq!(s.gate.reached().await, "llama3.2:1b");
    s.gate.open_once();
    second.await.unwrap().unwrap();
    assert!(
        !owner.asked().contains(&"egress.local".to_string()),
        "{:?}",
        owner.asked()
    );
    let signals = log_signals(&s);
    assert!(
        signals
            .last()
            .unwrap()
            .contains(&"spill_withheld".to_string()),
        "{signals:?}"
    );
}
