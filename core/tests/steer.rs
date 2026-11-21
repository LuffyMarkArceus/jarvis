//! Steer (Milestone 6.7a): a message typed while a turn runs joins it at
//! the model's next step. A gated model decides when each step happens.

mod common;

use common::{answer, call, gated, Gate};
use jarvis_core::config::Config;
use jarvis_core::conversation::{Conversation, Steer};
use jarvis_core::memory::MemoryManager;
use jarvis_core::orchestrator::{Orchestrator, TurnReport};
use jarvis_core::permissions::PermissionManager;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

struct Setup {
    dir: PathBuf,
    config: Config,
    jarvis: Arc<Orchestrator>,
    gate: Gate,
    gated: Arc<common::Gated>,
    conv: Arc<Conversation>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(edit: impl FnOnce(&mut Config)) -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-steer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(dir.join("project"));
    edit(&mut config);
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let (gated, gate) = gated();
    let jarvis = Arc::new(Orchestrator::with_parts(
        config.clone(),
        gated.clone(),
        permissions,
    ));
    let conv = jarvis.new_conversation();
    Setup {
        dir,
        config,
        jarvis,
        gate,
        gated,
        conv,
    }
}

type Turn = tokio::task::JoinHandle<jarvis_core::error::Result<TurnReport>>;

/// A request routed to the model with tools (file and shell).
const TOOL_TURN: &str = "check the readme file and install what it says";

fn ask(s: &Setup, input: &str, cancel: Option<jarvis_core::cancel::Cancellation>) -> Turn {
    let (jarvis, conv, input) = (s.jarvis.clone(), s.conv.clone(), input.to_string());
    tokio::spawn(async move {
        jarvis
            .run_turn(&conv, &input, None, cancel.as_ref(), None)
            .await
    })
}

fn steer(id: &str, text: &str) -> Steer {
    Steer {
        id: id.into(),
        text: text.into(),
    }
}

fn list_dir(s: &Setup) {
    s.gated.script.lock().unwrap().push_back(call(
        "filesystem.list_dir",
        json!({ "path": s.dir.join("project") }),
    ));
}

#[tokio::test]
async fn a_steer_reaches_the_next_model_call_and_memory() {
    let mut s = setup(|_| {});
    list_dir(&s);
    s.gated.script.lock().unwrap().push_back(answer("done"));
    let turn = ask(&s, TOOL_TURN, None);
    s.gate.reached().await;
    assert!(s.conv.steer(steer("s1", "use the Pictures folder")));
    s.gate.open_once();
    s.gate.reached().await;
    s.gate.open_once();
    turn.await.unwrap().unwrap();
    let requests = s.gated.requests.lock().unwrap().clone();
    assert!(!requests[0].contains("Pictures"));
    assert!(
        requests[1].contains("[user] use the Pictures folder"),
        "{}",
        requests[1]
    );
    let stored = MemoryManager::new(s.config.clone())
        .unwrap()
        .get_recent_messages(&s.conv.id, 20)
        .unwrap();
    assert!(stored
        .iter()
        .any(|m| m.role == "user" && m.content == "use the Pictures folder"));
    assert_eq!(s.conv.take_used_steers(), vec!["s1".to_string()]);
    assert!(s.conv.take_steers().is_empty());
}

#[tokio::test]
async fn a_steer_to_a_direct_answer_is_not_running() {
    let mut s = setup(|_| {});
    let turn = ask(&s, "tell me a joke", None);
    s.gate.reached().await;
    assert!(
        !s.conv.steer(steer("s1", "make it shorter")),
        "a one-call answer"
    );
    s.gate.open_once();
    turn.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_steer_at_the_final_answer_gets_one_more_round() {
    // Review Focus 5.
    let mut s = setup(|_| {});
    s.gated
        .script
        .lock()
        .unwrap()
        .push_back(answer("first answer"));
    let turn = ask(&s, TOOL_TURN, None);
    s.gate.reached().await;
    assert!(s.conv.steer(steer("s1", "in French please")));
    s.gate.open_once();
    s.gate.reached().await; // a second call, for the steer
    s.gate.open_once();
    let report = turn.await.unwrap().unwrap();
    let requests = s.gated.requests.lock().unwrap().clone();
    assert!(
        requests[1].contains("[assistant] first answer"),
        "{}",
        requests[1]
    );
    assert!(
        requests[1].contains("[user] in French please"),
        "{}",
        requests[1]
    );
    assert_eq!(report.answer, "answered by llama3.2:1b");
}

#[tokio::test]
async fn a_steer_with_no_rounds_left_comes_back_unused() {
    let mut s = setup(|c| c.model.max_rounds = Some(1));
    s.gated
        .script
        .lock()
        .unwrap()
        .push_back(answer("only answer"));
    let turn = ask(&s, TOOL_TURN, None);
    s.gate.reached().await;
    assert!(s.conv.steer(steer("s1", "too late")));
    s.gate.open_once();
    let report = turn.await.unwrap().unwrap();
    assert_eq!(report.answer, "only answer");
    let unused = s.conv.take_steers();
    assert_eq!(unused.len(), 1);
    assert_eq!(unused[0].id, "s1");
    assert!(s.conv.take_used_steers().is_empty());
}

#[tokio::test]
async fn a_cancelled_turn_reports_the_steers_it_used() {
    let mut s = setup(|_| {});
    list_dir(&s);
    let registry = jarvis_core::cancel::TurnRegistry::default();
    let id = uuid::Uuid::new_v4();
    let (cancel, _guard) = registry.begin(id);
    let turn = ask(&s, TOOL_TURN, Some(cancel));
    s.gate.reached().await;
    assert!(s.conv.steer(steer("s1", "use Pictures")));
    s.gate.open_once();
    s.gate.reached().await; // the steer is in this call
    registry.cancel(id);
    let _ = turn.await.unwrap();
    assert_eq!(s.conv.take_used_steers(), vec!["s1".to_string()]);
}

#[tokio::test]
async fn a_conversation_is_not_steerable_after_its_turn() {
    let mut s = setup(|_| {});
    s.gated.script.lock().unwrap().push_back(answer("done"));
    let turn = ask(&s, TOOL_TURN, None);
    s.gate.reached().await;
    s.gate.open_once();
    turn.await.unwrap().unwrap();
    assert!(!s.conv.steer(steer("late", "hello?")));
}
