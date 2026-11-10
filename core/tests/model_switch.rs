//! Switching models mid-conversation (`/model`): a pick from the configured
//! lists, by exact name or a unique part of one; the name decides the tier.

use jarvis_core::config::{Config, ModelConfig, RemoteConfig};
use jarvis_core::eval::RecordingProvider;
use jarvis_core::orchestrator::Orchestrator;
use jarvis_core::permissions::PermissionManager;
use std::path::PathBuf;
use std::sync::Arc;

struct Setup {
    dir: PathBuf,
    jarvis: Orchestrator,
    local: Arc<RecordingProvider>,
    remote: Arc<RecordingProvider>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup() -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-models-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.model.default_model = "llama3.2:1b".into();
    config.model.models = vec!["qwen2.5:3b".into(), "llama3.2:1b".into()];
    config.remote = Some(RemoteConfig {
        model: ModelConfig {
            provider: "openai".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            default_model: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
            models: vec![
                "cohere/north-mini-code:free".into(),
                "recursive/flint-v1.0:free".into(),
            ],
            data_policy: Some("may_train".into()),
            ..ModelConfig::default()
        },
        default: false,
    });
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let local = Arc::new(RecordingProvider::replying("local"));
    let remote = Arc::new(RecordingProvider::replying("remote"));
    let jarvis = Orchestrator::with_parts(config, local.clone(), permissions)
        .with_remote(Some(remote.clone()));
    Setup {
        dir,
        jarvis,
        local,
        remote,
    }
}

impl Setup {
    async fn chat(&self) {
        self.jarvis
            .run_turn("tell me a joke", None, None, None)
            .await
            .unwrap();
    }
}

fn names(s: &Setup) -> Vec<(String, bool, bool)> {
    s.jarvis
        .models()
        .into_iter()
        .map(|m| (m.name, m.remote, m.current))
        .collect()
}

#[test]
fn the_list_is_each_tiers_default_then_its_others_without_repeats() {
    let s = setup();
    assert_eq!(
        names(&s),
        vec![
            ("llama3.2:1b".into(), false, true),
            ("qwen2.5:3b".into(), false, false),
            ("nvidia/nemotron-3-ultra-550b-a55b:free".into(), true, false),
            ("cohere/north-mini-code:free".into(), true, false),
            ("recursive/flint-v1.0:free".into(), true, false),
        ]
    );
}

#[tokio::test]
async fn a_local_model_is_used_for_the_next_turn() {
    let s = setup();
    let chosen = s.jarvis.set_model("qwen").unwrap();
    assert_eq!(chosen.name, "qwen2.5:3b");
    assert!(!chosen.remote);
    s.chat().await;
    assert_eq!(*s.local.models.lock().unwrap(), vec!["qwen2.5:3b"]);
    assert_eq!(s.jarvis.model_info().1, "qwen2.5:3b");
}

#[tokio::test]
async fn a_remote_model_moves_the_conversation_to_the_remote_tier() {
    let s = setup();
    s.jarvis.set_model("north").unwrap();
    assert!(s.jarvis.uses_remote());
    s.chat().await;
    assert_eq!(
        *s.remote.models.lock().unwrap(),
        vec!["cohere/north-mini-code:free"]
    );
    assert!(s.local.models.lock().unwrap().is_empty());
    // Back to local: the local tier's choice is still its own.
    s.jarvis.set_model("llama").unwrap();
    assert!(!s.jarvis.uses_remote());
    s.chat().await;
    assert_eq!(*s.local.models.lock().unwrap(), vec!["llama3.2:1b"]);
}

#[test]
fn switching_tiers_keeps_each_tiers_choice() {
    let s = setup();
    s.jarvis.set_model("flint").unwrap();
    s.jarvis.set_remote(false).unwrap();
    assert_eq!(s.jarvis.model_info().1, "llama3.2:1b");
    s.jarvis.set_remote(true).unwrap();
    assert_eq!(s.jarvis.model_info().1, "recursive/flint-v1.0:free");
}

#[test]
fn an_exact_name_wins_and_an_ambiguous_or_unknown_one_is_refused() {
    let s = setup();
    // "llama3.2:1b" is exact; ":free" matches three.
    assert_eq!(
        s.jarvis.set_model("llama3.2:1b").unwrap().name,
        "llama3.2:1b"
    );
    let err = s.jarvis.set_model(":free").unwrap_err().to_string();
    assert!(err.contains("north-mini-code"), "{}", err);
    let err = s.jarvis.set_model("gpt-9").unwrap_err().to_string();
    assert!(err.contains("models ="), "says how to add one: {}", err);
    // A refused switch changes nothing.
    assert_eq!(s.jarvis.model_info().1, "llama3.2:1b");
    // Case does not matter.
    assert_eq!(s.jarvis.set_model("QWEN").unwrap().name, "qwen2.5:3b");
}

#[test]
fn the_current_mark_follows_the_choice() {
    let s = setup();
    s.jarvis.set_model("nemotron").unwrap();
    let current: Vec<String> = s
        .jarvis
        .models()
        .into_iter()
        .filter(|m| m.current)
        .map(|m| m.name)
        .collect();
    assert_eq!(current, vec!["nvidia/nemotron-3-ultra-550b-a55b:free"]);
}

#[test]
fn without_a_remote_tier_only_local_models_are_listed() {
    let s = setup();
    let mut config = Config::default();
    config.general.data_dir = s.dir.join("other");
    config.memory.db_path = s.dir.join("other/memory.db");
    config.model.models = vec!["qwen2.5:3b".into()];
    let permissions = PermissionManager::with_paths(
        s.dir.join("other/permissions.toml"),
        s.dir.join("other/audit.db"),
    )
    .unwrap();
    let jarvis = Orchestrator::with_parts(
        config,
        Arc::new(RecordingProvider::replying("x")),
        permissions,
    );
    assert!(jarvis.models().iter().all(|m| !m.remote));
    assert_eq!(jarvis.models().len(), 2);
}
