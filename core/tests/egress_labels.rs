//! Labels survive a real turn: a file Jarvis reads is stored as file
//! content, and comes back as file content from history and from recall in
//! another session. Without this, a file shown today would leave next week
//! as "memory" (docs/design/model-tiers.md).

use jarvis_core::config::Config;
use jarvis_core::egress::Origin;
use jarvis_core::eval::ScriptedProvider;
use jarvis_core::memory::MemoryManager;
use jarvis_core::orchestrator::Orchestrator;
use jarvis_core::permissions::PermissionManager;
use std::sync::Arc;

#[tokio::test]
async fn a_file_read_is_stored_and_recalled_as_file_content() {
    let dir = std::env::temp_dir().join(format!("jarvis-egress-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let notes = dir.join("notes.txt");
    std::fs::write(&notes, "buy oat milk").unwrap();

    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    // The default policy allows reads; the user's own policy is not touched.
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let jarvis = Orchestrator::with_parts(config.clone(), Arc::new(ScriptedProvider), permissions);

    let report = jarvis
        .run_turn(&format!("read {}", notes.display()), None, None, None)
        .await
        .unwrap();
    assert_eq!(report.route, "Tool(filesystem.read)");

    let memory = MemoryManager::new(config).unwrap();
    let history = memory.get_recent_messages(jarvis.session_id(), 10).unwrap();
    let [question, answer] = history.as_slice() else {
        panic!("expected two messages, got {:?}", history);
    };
    assert_eq!(
        question.label.as_ref().unwrap().origins,
        [Origin::User].into()
    );
    let label = answer.label.as_ref().expect("the answer is labelled");
    assert!(label.origins.contains(&Origin::File), "{:?}", label);
    assert!(label.paths.contains(notes.to_str().unwrap()), "{:?}", label);

    let recalled = memory.recall("oat milk", 5).unwrap();
    let answer_recall = recalled
        .iter()
        .find(|r| r.role == "assistant")
        .expect("recalled");
    assert!(answer_recall
        .label
        .as_ref()
        .unwrap()
        .origins
        .contains(&Origin::File));

    let _ = std::fs::remove_dir_all(dir);
}
