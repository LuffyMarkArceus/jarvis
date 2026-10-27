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
