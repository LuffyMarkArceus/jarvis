//! A `~/` path from the model is the owner's home, for the tool, the
//! permission check and undo alike. Its own test binary: it sets HOME,
//! which every test in the same process would see.

mod common;

use common::{answer, call, Script};
use jarvis_core::config::Config;
use jarvis_core::orchestrator::Orchestrator;
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use jarvis_core::undo::UndoStore;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn a_tilde_path_is_the_file_written_and_the_one_undo_records() {
    let home = std::env::temp_dir().join(format!("jarvis-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(home.join("data")).unwrap();
    // Safe here: this binary runs no other test that reads HOME.
    unsafe { std::env::set_var("HOME", &home) };

    let mut config = Config::default();
    config.general.data_dir = home.join("data");
    config.memory.db_path = home.join("data/memory.db");
    config.memory.recall_enabled = false;
    let permissions =
        PermissionManager::with_paths(home.join("data/p.toml"), home.join("data/a.db")).unwrap();
    permissions
        .set_rule("filesystem.write", PermissionAction::Allow)
        .unwrap();
    let script = vec![
        call(
            "filesystem.write",
            json!({ "path": "~/notes.md", "content": "hello" }),
        ),
        answer("done"),
    ];
    let jarvis =
        Orchestrator::with_parts(config.clone(), Arc::new(Script::new(script)), permissions);
    let report = jarvis
        .run_turn(
            "check the readme file and install what it says",
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        report.calls[0].args["path"],
        home.join("notes.md").display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(home.join("notes.md")).unwrap(),
        "hello"
    );
    assert!(
        !std::path::Path::new("~").exists(),
        "no folder named ~ in the working directory"
    );
    let changes = UndoStore::for_config(&config).unwrap().list().unwrap();
    assert_eq!(
        changes[0].path,
        home.join("notes.md").canonicalize().unwrap()
    );
    let _ = std::fs::remove_dir_all(home);
}
