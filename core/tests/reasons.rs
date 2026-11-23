//! A no with a reason (Milestone 6.7b): the reason reaches the model, in
//! the declined tool's result.

mod common;

use common::{answer, call, Script};
use jarvis_core::config::Config;
use jarvis_core::orchestrator::{Answer, Choice, ConfirmationHandler, Orchestrator};
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use serde_json::{json, Value};
use std::sync::Arc;

/// Says no, with `reason`.
struct NoBecause(Option<&'static str>);

#[async_trait::async_trait]
impl ConfirmationHandler for NoBecause {
    async fn confirm(&self, _tool: &str, _args: &Value) -> bool {
        false
    }

    async fn answer(&self, _tool: &str, _args: &Value, _choices: &[Choice]) -> Answer {
        Answer {
            approved: false,
            choice: None,
            reason: self.0.map(String::from),
        }
    }
}

async fn model_saw_after_no(reason: Option<&'static str>) -> String {
    let dir = std::env::temp_dir().join(format!("jarvis-reasons-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("project")).unwrap();
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
    let script = Arc::new(Script::new(vec![
        call(
            "filesystem.write",
            json!({ "path": dir.join("project/a.md"), "content": "x" }),
        ),
        answer("ok"),
    ]));
    let jarvis = Orchestrator::with_parts(config, script.clone(), permissions);
    let conv = jarvis.new_conversation();
    let no = NoBecause(reason);
    let _ = jarvis
        .run_turn(
            &conv,
            "check the readme file and install what it says",
            Some(&no),
            None,
            None,
        )
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    let saw = script.sent.lock().unwrap().join("\n");
    saw
}

#[tokio::test]
async fn a_reason_reaches_the_models_next_request() {
    let saw = model_saw_after_no(Some("use notes/ instead")).await;
    assert!(
        saw.contains("Tool filesystem.write was declined by the user: use notes/ instead"),
        "{saw}"
    );
}

#[tokio::test]
async fn a_plain_no_ends_the_turn_as_before() {
    // Without a reason the turn stops with its report; the model is not
    // asked again.
    let saw = model_saw_after_no(None).await;
    assert!(!saw.contains("was declined by the user"), "{saw}");
}
