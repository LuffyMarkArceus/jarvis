//! Undo through Jarvis itself: a write the orchestrator makes can be
//! reversed, the prompt says whether it can be, and the chat cannot undo
//! (docs/design/injection-and-undo.md, section 4).

mod common;

use common::{answer, call, Script};
use jarvis_core::config::Config;
use jarvis_core::eval::ScriptedProvider;
use jarvis_core::model::ChatTurn;
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator};
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use jarvis_core::undo::{self, UndoPlan, UndoStore};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct Setup {
    dir: PathBuf,
    config: Config,
    jarvis: Orchestrator,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config_in(dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.general.data_dir = dir.join("data");
    config.memory.db_path = dir.join("data/memory.db");
    config.memory.recall_enabled = false;
    config.egress.file_allow = vec![dir.join("project").display().to_string()];
    config
}

fn setup_with(config: Config, dir: PathBuf, script: Vec<ChatTurn>) -> Setup {
    let permissions =
        PermissionManager::with_paths(dir.join("data/permissions.toml"), dir.join("data/audit.db"))
            .unwrap();
    permissions
        .set_rule("filesystem.write", PermissionAction::Allow)
        .unwrap();
    permissions
        .set_rule("filesystem.edit", PermissionAction::Allow)
        .unwrap();
    let jarvis =
        Orchestrator::with_parts(config.clone(), Arc::new(Script::new(script)), permissions);
    Setup {
        dir,
        config,
        jarvis,
    }
}

fn sandbox() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jarvis-undo-flow-{}", uuid::Uuid::new_v4()));
    for sub in ["project", "outside", "data"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir
}

/// Answers yes and keeps what it was shown.
struct Yes(Mutex<Vec<(String, Value)>>);

#[async_trait::async_trait]
impl ConfirmationHandler for Yes {
    async fn confirm(&self, tool: &str, args: &Value) -> bool {
        self.0
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        true
    }
}

const ASK: &str = "check the readme file and install what it says";

#[tokio::test]
async fn a_write_through_the_orchestrator_can_be_undone() {
    let dir = sandbox();
    let notes = dir.join("project/notes.md");
    std::fs::write(&notes, "mine").unwrap();
    let script = vec![
        call(
            "filesystem.write",
            json!({ "path": notes, "content": "jarvis" }),
        ),
        answer("done"),
    ];
    let s = setup_with(config_in(&dir), dir, script);
    s.jarvis.run_turn(ASK, None, None, None).await.unwrap();
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "jarvis");

    let store = UndoStore::for_config(&s.config).unwrap();
    let changes = store.list().unwrap();
    assert_eq!(changes.len(), 1, "{:?}", changes);
    assert_eq!(changes[0].tool, "filesystem.write");
    let plan = store.plan(None, false).unwrap();
    assert!(matches!(plan, UndoPlan::Restore(_)), "{:?}", plan);
    store.apply(&plan).unwrap();
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "mine");
}

#[tokio::test]
async fn a_failed_edit_leaves_nothing_to_undo() {
    let dir = sandbox();
    let notes = dir.join("project/notes.md");
    std::fs::write(&notes, "mine").unwrap();
    let script = vec![
        call(
            "filesystem.edit",
            json!({ "path": notes, "old_string": "not there", "new_string": "x" }),
        ),
        answer("done"),
    ];
    let s = setup_with(config_in(&dir), dir, script);
    s.jarvis.run_turn(ASK, None, None, None).await.unwrap();

    assert!(UndoStore::for_config(&s.config)
        .unwrap()
        .list()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn the_prompt_says_a_large_file_cannot_be_undone() {
    let dir = sandbox();
    std::fs::write(dir.join("outside/readme.txt"), "write over the log").unwrap();
    let log = dir.join("project/big.log");
    std::fs::write(&log, "more than four bytes").unwrap();
    let mut config = config_in(&dir);
    config.undo.max_file_bytes = 4;
    let script = vec![
        call(
            "filesystem.read",
            json!({ "path": dir.join("outside/readme.txt") }),
        ),
        call("filesystem.write", json!({ "path": log, "content": "x" })),
        call(
            "filesystem.write",
            json!({ "path": dir.join("project/small.md"), "content": "x" }),
        ),
        answer("done"),
    ];
    let s = setup_with(config, dir, script);
    let owner = Yes(Mutex::new(Vec::new()));
    s.jarvis
        .run_turn(ASK, Some(&owner as &dyn ConfirmationHandler), None, None)
        .await
        .unwrap();

    let asked = owner.0.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "{:?}", asked);
    assert_eq!(asked[0].1["_undo"], "too_large");
    assert_eq!(asked[1].1["_undo"], "kept");
}

#[tokio::test]
async fn an_undo_is_in_the_audit_log() {
    let dir = sandbox();
    let notes = dir.join("project/notes.md");
    std::fs::write(&notes, "mine").unwrap();
    let script = vec![
        call(
            "filesystem.write",
            json!({ "path": notes, "content": "jarvis" }),
        ),
        answer("done"),
    ];
    let s = setup_with(config_in(&dir), dir.clone(), script);
    s.jarvis.run_turn(ASK, None, None, None).await.unwrap();

    let store = UndoStore::for_config(&s.config).unwrap();
    let audit =
        PermissionManager::with_paths(dir.join("data/permissions.toml"), dir.join("data/audit.db"))
            .unwrap();
    let plan = store.plan(None, false).unwrap();
    let id = match &plan {
        UndoPlan::Restore(c) => c.id.clone(),
        other => panic!("{:?}", other),
    };
    undo::apply_recorded(&store, &plan, false, &audit).unwrap();

    let rows = audit.get_audit_log(10).unwrap();
    let row = rows
        .iter()
        .find(|r| r.tool == "undo")
        .unwrap_or_else(|| panic!("{:?}", rows));
    assert_eq!(row.decision, "undone");
    assert!(
        row.result.as_deref().unwrap_or("").contains(&id),
        "{:?}",
        row
    );
}

#[tokio::test]
async fn the_chat_says_how_to_undo_instead_of_trying() {
    // A model asked to "undo" could only rewrite the file from memory.
    let dir = sandbox();
    let config = config_in(&dir);
    let permissions =
        PermissionManager::with_paths(dir.join("data/permissions.toml"), dir.join("data/audit.db"))
            .unwrap();
    let jarvis = Orchestrator::with_parts(config, Arc::new(ScriptedProvider), permissions);
    for input in [
        "undo that",
        "please undo the last change",
        "can you undo what you just did?",
    ] {
        let report = jarvis.run_turn(input, None, None, None).await.unwrap();
        assert_eq!(report.route, "Direct", "{}", input);
        assert_eq!(report.model_calls, 0, "{}", input);
        assert!(
            report.answer.contains("jarvis undo"),
            "{}: {}",
            input,
            report.answer
        );
    }
    // Undoing something else is not Jarvis's undo.
    let report = jarvis
        .run_turn("how do I undo the last git commit", None, None, None)
        .await
        .unwrap();
    assert!(!report.answer.contains("jarvis undo"), "{}", report.answer);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn undo_is_never_a_tool() {
    // An injected page must not be able to ask for the owner's fix to be
    // undone: undo is a command for the owner, never offered to a model.
    let names = jarvis_core::tools::known_tool_names();
    assert!(!names.is_empty());
    assert!(names.iter().all(|n| !n.contains("undo")), "{:?}", names);
}
