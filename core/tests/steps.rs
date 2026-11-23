//! Step events (Milestone 6.7b): each step of a turn is reported as it
//! starts and as it ends, so a window can show the work and fold it.

mod common;

use common::{answer, call, Script};
use jarvis_common::ipc::StepEvent;
use jarvis_core::config::Config;
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator};
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

struct No;

#[async_trait::async_trait]
impl ConfirmationHandler for No {
    async fn confirm(&self, _tool: &str, _args: &Value) -> bool {
        false
    }
}

async fn steps_of(
    turns: impl FnOnce(&std::path::Path) -> Vec<jarvis_core::model::ChatTurn>,
) -> (Vec<StepEvent>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("jarvis-steps-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::write(dir.join("project/a.txt"), "a").unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(dir.join("project"));
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    permissions
        .set_rule("filesystem.write", PermissionAction::Ask)
        .unwrap();
    let jarvis = Orchestrator::with_parts(config, Arc::new(Script::new(turns(&dir))), permissions);
    let conv = jarvis.new_conversation();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    jarvis
        .run_turn_observed(
            &conv,
            "check the readme file and install what it says",
            Some(&No),
            None,
            None,
            None,
            Some(&tx),
        )
        .await
        .unwrap();
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    (events, dir)
}

#[tokio::test]
async fn a_tool_turn_emits_running_and_done_pairs() {
    let (events, dir) = steps_of(|d| {
        vec![
            call("filesystem.list_dir", json!({ "path": d.join("project") })),
            answer("one file"),
        ]
    })
    .await;
    let seen: Vec<(u32, &str, &str)> = events.iter().map(|e| (e.step, e.kind, e.state)).collect();
    assert_eq!(
        seen,
        vec![
            (1, "model", "running"),
            (1, "model", "done"),
            (2, "tool", "running"),
            (2, "tool", "done"),
            (3, "model", "running"),
            (3, "model", "done"),
        ],
        "{events:?}"
    );
    assert_eq!(events[2].name, "filesystem.list_dir");
    assert!(events[3].detail.is_some(), "{:?}", events[3]);
    assert!(events[0].summary.starts_with("asking "), "{:?}", events[0]);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_declined_call_ends_declined() {
    let (events, dir) = steps_of(|d| {
        vec![
            call(
                "filesystem.write",
                json!({ "path": d.join("project/b.txt"), "content": "b" }),
            ),
            answer("not written"),
        ]
    })
    .await;
    let write_end = events
        .iter()
        .find(|e| e.kind == "tool" && e.state != "running")
        .expect("the write's end");
    assert_eq!(write_end.state, "declined", "{events:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_step_detail_never_shows_the_untrusted_tag() {
    // Review M9: the detail is the output's first line, not Jarvis's label.
    let (events, dir) = steps_of(|d| {
        vec![
            call(
                "filesystem.read",
                json!({ "path": d.join("project/a.txt") }),
            ),
            answer("it says a"),
        ]
    })
    .await;
    let read_end = events
        .iter()
        .find(|e| e.kind == "tool" && e.state != "running")
        .expect("the read's end");
    let detail = read_end.detail.clone().unwrap_or_default();
    assert!(!detail.contains("<untrusted"), "{detail:?}");
    assert!(detail.contains('a'), "{detail:?}");
    let _ = std::fs::remove_dir_all(dir);
}
