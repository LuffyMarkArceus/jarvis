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

// ------------------------------------------------------------ the gateway

use jarvis_core::eval::RecordingProvider;
use jarvis_core::orchestrator::{ConfirmationHandler, TurnReport};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

/// Answers every prompt the same way and remembers what it was asked.
struct User {
    answer: bool,
    asked: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl ConfirmationHandler for User {
    async fn confirm(&self, tool: &str, args: &Value) -> bool {
        self.asked
            .lock()
            .unwrap()
            .push((tool.to_string(), args.clone()));
        self.answer
    }
}

fn user(answer: bool) -> User {
    User {
        answer,
        asked: Mutex::new(Vec::new()),
    }
}

struct Setup {
    dir: PathBuf,
    jarvis: Orchestrator,
    model: Arc<RecordingProvider>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// An orchestrator whose model is "remote" (or local) and records requests.
fn setup(locality: &str, reply: &str, file_allow: &[&str]) -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-gate-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("allowed")).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.model.base_url = "https://models.example/v1".into();
    config.model.locality = Some(locality.into());
    config.egress.file_allow = file_allow
        .iter()
        .map(|d| dir.join(d).display().to_string())
        .collect();
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let model = Arc::new(RecordingProvider::replying(reply));
    let jarvis = Orchestrator::with_parts(config, model.clone(), permissions);
    Setup { dir, jarvis, model }
}

impl Setup {
    fn file(&self, rel: &str, content: &str) -> String {
        let path = self.dir.join(rel);
        std::fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    async fn ask(&self, input: &str, user: Option<&User>) -> TurnReport {
        self.jarvis
            .run_turn(
                input,
                user.map(|u| u as &dyn ConfirmationHandler),
                None,
                None,
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn pasted_secrets_and_personal_data_leave_as_placeholders() {
    let s = setup("remote", "I will write to <EMAIL_1> using <SECRET_1>.", &[]);
    let report = s
        .ask(
            "email shrey@example.com, my key is OPENAI_API_KEY=sk-proj-AbCdEf0123456789AbCdEf",
            None,
        )
        .await;
    let sent = s.model.requests().join("\n");
    assert!(!sent.contains("shrey@example.com"), "{}", sent);
    assert!(!sent.contains("sk-proj-AbCdEf"), "{}", sent);
    assert!(
        sent.contains("<EMAIL_1>") && sent.contains("<SECRET_1>"),
        "{}",
        sent
    );
    // Personal data comes back; the secret does not.
    assert_eq!(
        report.answer,
        "I will write to shrey@example.com using <SECRET_1>."
    );
    assert!(report.remote);
    assert_eq!(report.redacted, 2);
}

#[tokio::test]
async fn a_file_from_history_is_asked_about_and_declining_sends_nothing() {
    let s = setup("remote", "ok", &[]);
    let notes = s.file("notes.txt", "dentist on friday");
    s.ask(&format!("read {}", notes), None).await; // deterministic, no model
    assert!(s.model.requests().is_empty());

    let no = user(false);
    let report = s.ask("what was in that note?", Some(&no)).await;
    let asked = no.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, "egress.send");
    assert!(
        asked[0].1.to_string().contains("notes.txt"),
        "{}",
        asked[0].1
    );
    assert!(s.model.requests().is_empty(), "declined, yet sent");
    assert!(
        report.answer.starts_with("Nothing was sent"),
        "{}",
        report.answer
    );
    assert!(!report.remote);
}

#[tokio::test]
async fn approving_sends_the_file_once_redacted() {
    let s = setup("remote", "it says dentist", &[]);
    let notes = s.file("notes.txt", "dentist on friday, call 555-123-4567");
    s.ask(&format!("read {}", notes), None).await;
    let yes = user(true);
    let report = s.ask("what was in that note?", Some(&yes)).await;
    let sent = s.model.requests().join("\n");
    assert!(sent.contains("dentist on friday"), "{}", sent);
    assert!(!sent.contains("555-123-4567"), "{}", sent);
    assert_eq!(report.answer, "it says dentist");
}

#[tokio::test]
async fn a_file_in_an_allowed_folder_goes_without_asking() {
    let s = setup("remote", "ok", &["allowed"]);
    let readme = s.file("allowed/README.md", "# Jarvis");
    s.ask(&format!("read {}", readme), None).await;
    let no = user(false);
    s.ask("summarise that", Some(&no)).await;
    assert!(no.asked.lock().unwrap().is_empty());
    assert!(s.model.requests().join("\n").contains("# Jarvis"));
}

#[tokio::test]
async fn an_env_file_is_never_sent_and_there_is_nothing_to_approve() {
    let s = setup("remote", "ok", &["allowed"]);
    let env = s.file("allowed/.env", "DB_HOST=db.internal");
    s.ask(&format!("read {}", env), None).await;
    let yes = user(true);
    let report = s.ask("what is in it?", Some(&yes)).await;
    assert!(
        yes.asked.lock().unwrap().is_empty(),
        "never is not a question"
    );
    assert!(s.model.requests().is_empty());
    assert!(report.answer.contains("never sent"), "{}", report.answer);
}

#[tokio::test]
async fn with_nobody_to_ask_nothing_is_sent() {
    let s = setup("remote", "ok", &[]);
    let notes = s.file("notes.txt", "private");
    s.ask(&format!("read {}", notes), None).await;
    let report = s.ask("what was in that note?", None).await;
    assert!(s.model.requests().is_empty());
    assert!(report.answer.starts_with("Nothing was sent"));
}

#[tokio::test]
async fn a_local_model_gets_everything_unchanged() {
    let s = setup("local", "ok", &[]);
    let notes = s.file("notes.txt", "call 555-123-4567");
    s.ask(&format!("read {}", notes), None).await;
    let no = user(false);
    let report = s.ask("email a@x.io about that note", Some(&no)).await;
    let sent = s.model.requests().join("\n");
    assert!(
        sent.contains("555-123-4567") && sent.contains("a@x.io"),
        "{}",
        sent
    );
    assert!(no.asked.lock().unwrap().is_empty());
    assert!(!report.remote);
    assert_eq!(report.redacted, 0);
}

// ------------------------------------------------------------ two tiers

use jarvis_core::config::{ModelConfig, RemoteConfig};
use jarvis_core::orchestrator::{footer, Choice};

/// Answers each kind of prompt as told and records every prompt.
struct Owner {
    send: Option<char>,
    answer_locally: bool,
    asked: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ConfirmationHandler for Owner {
    async fn confirm(&self, tool: &str, _args: &Value) -> bool {
        self.asked.lock().unwrap().push(tool.to_string());
        tool == "egress.local" && self.answer_locally
    }

    async fn choose(&self, tool: &str, _args: &Value, choices: &[Choice]) -> Option<char> {
        let offered: String = choices.iter().map(|c| c.key).collect();
        self.asked
            .lock()
            .unwrap()
            .push(format!("{} [{}]", tool, offered));
        self.send
    }
}

fn owner(send: Option<char>, answer_locally: bool) -> Owner {
    Owner {
        send,
        answer_locally,
        asked: Mutex::new(Vec::new()),
    }
}

struct Tiers {
    dir: PathBuf,
    jarvis: Orchestrator,
    local: Arc<RecordingProvider>,
    remote: Arc<RecordingProvider>,
}

impl Drop for Tiers {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `[model]` is local llama, `[remote]` a cloud model new sessions start on.
fn tiers() -> Tiers {
    let dir = std::env::temp_dir().join(format!("jarvis-tiers-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.remote = Some(RemoteConfig {
        model: ModelConfig {
            provider: "openai".into(),
            base_url: "https://models.example/v1".into(),
            default_model: "cloud-model".into(),
            ..ModelConfig::default()
        },
        default: true,
    });
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let local = Arc::new(RecordingProvider::replying("local answer"));
    let remote = Arc::new(RecordingProvider::replying("remote answer"));
    let jarvis = Orchestrator::with_parts(config, local.clone(), permissions)
        .with_remote(Some(remote.clone()));
    Tiers {
        dir,
        jarvis,
        local,
        remote,
    }
}

impl Tiers {
    async fn read_note(&self, name: &str, text: &str) {
        let path = self.dir.join("notes").join(name);
        std::fs::write(&path, text).unwrap();
        self.jarvis
            .run_turn(&format!("read {}", path.display()), None, None, None)
            .await
            .unwrap();
    }

    async fn ask(&self, input: &str, who: &Owner) -> TurnReport {
        self.jarvis
            .run_turn(input, Some(who), None, None)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_remote_session_sends_to_the_remote_model_and_says_so() {
    let t = tiers();
    let report = t.ask("hello", &owner(None, false)).await;
    assert_eq!(t.remote.requests().len(), 1);
    assert!(t.local.requests().is_empty());
    assert_eq!(report.answer, "remote answer");
    assert!(
        footer(&report).starts_with("cloud-model · remote: recording"),
        "{}",
        footer(&report)
    );
}

#[tokio::test]
async fn declining_then_choosing_local_answers_locally_with_the_file() {
    let t = tiers();
    t.read_note("todo.txt", "dentist on friday").await;
    let who = owner(None, true);
    let report = t.ask("what was in that note?", &who).await;
    assert!(
        t.remote.requests().is_empty(),
        "declined, yet sent remotely"
    );
    // Local gets the request whole: nothing leaves, so nothing is redacted.
    assert!(t.local.requests().join("\n").contains("dentist on friday"));
    assert_eq!(report.answer, "local answer");
    assert!(!report.remote);
    assert_eq!(footer(&report), "llama3.2:1b · local · 110 tokens");
    assert_eq!(
        *who.asked.lock().unwrap(),
        ["egress.send [ya]", "egress.local"]
    );
}

#[tokio::test]
async fn declining_both_sends_nothing_anywhere() {
    let t = tiers();
    t.read_note("todo.txt", "dentist").await;
    let report = t.ask("what was in that note?", &owner(None, false)).await;
    assert!(t.remote.requests().is_empty() && t.local.requests().is_empty());
    assert!(
        report.answer.starts_with("Nothing was sent"),
        "{}",
        report.answer
    );
}

#[tokio::test]
async fn always_allow_saves_the_folder_and_stops_asking() {
    let t = tiers();
    t.read_note("todo.txt", "dentist").await;
    t.ask("what was in that note?", &owner(Some('a'), false))
        .await;
    assert_eq!(t.remote.requests().len(), 1);
    let saved = jarvis_core::egress::allowed_folders(&t.jarvis.allow_file());
    assert_eq!(saved, vec![t.dir.join("notes").display().to_string()]);

    // Another file from the same folder, another turn: no prompt.
    t.read_note("shopping.txt", "oat milk").await;
    let quiet = owner(None, false);
    t.ask("and that one?", &quiet).await;
    assert!(
        quiet.asked.lock().unwrap().is_empty(),
        "{:?}",
        quiet.asked.lock().unwrap()
    );
    assert!(t.remote.requests().last().unwrap().contains("oat milk"));
}

#[tokio::test]
async fn a_never_file_offers_only_the_local_model() {
    let t = tiers();
    t.read_note(".env", "DB_PASSWORD=hunter22").await;
    let who = owner(Some('y'), true);
    let report = t.ask("what is the password?", &who).await;
    assert_eq!(
        *who.asked.lock().unwrap(),
        ["egress.local"],
        "nothing to approve for sending"
    );
    assert!(t.remote.requests().is_empty());
    assert_eq!(report.answer, "local answer");
}

#[tokio::test]
async fn a_session_switches_tiers_and_cannot_switch_to_a_missing_one() {
    let t = tiers();
    t.jarvis.set_remote(false).unwrap();
    t.ask("hello", &owner(None, false)).await;
    assert_eq!(t.local.requests().len(), 1);
    assert!(t.remote.requests().is_empty());

    let s = setup("local", "ok", &[]);
    assert!(s.jarvis.set_remote(true).is_err());
}
