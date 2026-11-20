//! Conversations keep their own model and tier, across restarts; each answer
//! records the model that wrote it; a new conversation starts with the model
//! used last (Milestone 6.5a, task 2).

use jarvis_core::config::{Config, ModelConfig, RemoteConfig};
use jarvis_core::conversation::Conversations;
use jarvis_core::eval::RecordingProvider;
use jarvis_core::memory::MemoryManager;
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator};
use jarvis_core::permissions::PermissionManager;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct Setup {
    dir: PathBuf,
    config: Config,
    jarvis: Orchestrator,
    remote: Arc<RecordingProvider>,
}

impl Drop for Setup {
    fn drop(&mut self) {
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
    config.egress.file_allow = vec![dir.join("allowed").display().to_string()];
    config.remote = Some(RemoteConfig {
        model: ModelConfig {
            provider: "openai".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            default_model: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
            models: vec!["cohere/north-mini-code:free".into()],
            data_policy: Some("may_train".into()),
            ..ModelConfig::default()
        },
        default: false,
        ..RemoteConfig::default()
    });
    config
}

fn setup() -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-convs-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("allowed")).unwrap();
    let config = config_in(&dir);
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let local = Arc::new(RecordingProvider::replying("local answer"));
    let remote = Arc::new(RecordingProvider::replying("remote answer"));
    let jarvis = Orchestrator::with_parts(config.clone(), local, permissions)
        .with_remote(Some(remote.clone()));
    Setup {
        dir,
        config,
        jarvis,
        remote,
    }
}

fn registry(s: &Setup) -> Conversations {
    Conversations::new(MemoryManager::new(s.config.clone()).unwrap())
}

/// Says `yes` to everything and keeps the tools it was asked about.
struct Owner(bool, Mutex<Vec<String>>);

#[async_trait::async_trait]
impl ConfirmationHandler for Owner {
    async fn confirm(&self, tool: &str, _args: &Value) -> bool {
        self.1.lock().unwrap().push(tool.to_string());
        self.0
    }
}

#[test]
fn migrations_run_twice_and_old_messages_have_no_model() {
    let dir = std::env::temp_dir().join(format!("jarvis-convs-old-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("memory.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT,
                 created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                 role TEXT NOT NULL, content TEXT NOT NULL, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
             INSERT INTO sessions (id, title) VALUES ('s', 'old');
             INSERT INTO messages (session_id, role, content) VALUES ('s', 'assistant', 'old answer');",
        )
        .unwrap();
    }
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = db;
    for _ in 0..2 {
        let memory = MemoryManager::new(config.clone()).unwrap();
        let messages = memory.get_recent_messages("s", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model, None);
        assert_eq!(memory.session_settings("s").unwrap(), Some((None, false)));
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_conversations_model_and_tier_survive_a_restart() {
    let s = setup();
    let convs = registry(&s);
    let conv = convs.create(&s.jarvis);
    s.jarvis.set_model(&conv, "north").unwrap();
    convs.saved(&conv);

    let again = registry(&s).get(&conv.id).unwrap();
    assert!(again.remote());
    assert_eq!(s.jarvis.model_info(&again).1, "cohere/north-mini-code:free");
}

#[test]
fn a_new_conversation_starts_with_the_model_used_last() {
    let s = setup();
    let convs = registry(&s);
    let first = convs.create(&s.jarvis);
    s.jarvis.set_model(&first, "qwen").unwrap();
    convs.saved(&first);
    let second = convs.create(&s.jarvis);
    assert_eq!(s.jarvis.model_info(&second).1, "qwen2.5:3b");
    assert!(!second.remote());
    // A conversation already open keeps its own.
    let untouched = convs.create(&s.jarvis);
    s.jarvis.set_model(&first, "llama").unwrap();
    convs.saved(&first);
    assert_eq!(s.jarvis.model_info(&untouched).1, "qwen2.5:3b");
}

#[tokio::test]
async fn model_hopping_records_each_answers_model() {
    let s = setup();
    let convs = registry(&s);
    let conv = convs.create(&s.jarvis);
    for model in ["llama", "qwen", "llama", "north"] {
        s.jarvis.set_model(&conv, model).unwrap();
        s.jarvis
            .run_turn(&conv, &format!("tell me a joke, {model}"), None, None, None)
            .await
            .unwrap();
    }
    let memory = MemoryManager::new(s.config.clone()).unwrap();
    let answers: Vec<Option<String>> = memory
        .get_recent_messages(&conv.id, 20)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.model)
        .collect();
    assert_eq!(
        answers,
        vec![
            Some("llama3.2:1b".to_string()),
            Some("qwen2.5:3b".to_string()),
            Some("llama3.2:1b".to_string()),
            Some("cohere/north-mini-code:free".to_string()),
        ]
    );
    // The remote model saw the whole conversation, answers included.
    let sent = s.remote.requests().pop().unwrap();
    assert_eq!(sent.matches("local answer").count(), 3, "{sent}");
}

#[tokio::test]
async fn switching_to_remote_still_asks_before_sending_local_files() {
    let s = setup();
    let convs = registry(&s);
    let conv = convs.create(&s.jarvis);
    let secret = s.dir.join("outside.txt");
    std::fs::write(&secret, "my private notes").unwrap();
    s.jarvis
        .run_turn(
            &conv,
            &format!("read {}", secret.display()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    s.jarvis.set_model(&conv, "north").unwrap();
    let owner = Owner(false, Mutex::new(Vec::new()));
    s.jarvis
        .run_turn(&conv, "what was in that file?", Some(&owner), None, None)
        .await
        .unwrap();
    assert!(
        owner.1.lock().unwrap().iter().any(|t| t == "egress.send"),
        "{:?}",
        owner.1.lock().unwrap()
    );
    assert!(s.remote.requests().is_empty(), "nothing was sent");
}

#[test]
fn an_unknown_conversation_is_an_error() {
    let s = setup();
    let err = registry(&s)
        .get("nope")
        .err()
        .expect("an error")
        .to_string();
    assert!(err.contains("nope"), "{err}");
}
