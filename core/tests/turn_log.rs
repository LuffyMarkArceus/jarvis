//! Measuring turns (Milestone 6.4): where each turn's time went, one log line
//! per turn with its friction signals, and the owner's /bad and /good.

mod common;

use common::{answer, call};
use jarvis_core::cancel::Cancellation;
use jarvis_core::config::Config;
use jarvis_core::error::Result;
use jarvis_core::model::{ChatTurn, ProviderTiming};
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator};
use jarvis_core::permissions::{PermissionAction, PermissionManager};
use jarvis_core::provider::{ChatRequest, Provider};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

/// Plays a script, and reports the timing Ollama would.
struct Timed {
    replies: Vec<ChatTurn>,
    next: Mutex<usize>,
}

#[async_trait::async_trait]
impl Provider for Timed {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn chat_turn(&self, _r: ChatRequest<'_>, _c: Option<&Cancellation>) -> Result<ChatTurn> {
        let mut next = self.next.lock().unwrap();
        let mut turn = self.replies[(*next).min(self.replies.len() - 1)].clone();
        *next += 1;
        turn.timing = ProviderTiming {
            load_ms: Some(40),
            prompt_ms: Some(30),
            gen_ms: Some(20),
        };
        Ok(turn)
    }

    async fn chat_turn_streaming(
        &self,
        r: ChatRequest<'_>,
        c: Option<&Cancellation>,
        _s: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        self.chat_turn(r, c).await
    }
}

/// Takes `delay` to answer, then answers `yes`.
struct Slow {
    delay: std::time::Duration,
    yes: bool,
}

#[async_trait::async_trait]
impl ConfirmationHandler for Slow {
    async fn confirm(&self, _tool: &str, _args: &Value) -> bool {
        tokio::time::sleep(self.delay).await;
        self.yes
    }
}

struct Setup {
    dir: PathBuf,
    jarvis: Orchestrator,
    conv: std::sync::Arc<jarvis_core::conversation::Conversation>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(replies: Vec<ChatTurn>) -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-turnlog-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(dir.clone());
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    permissions
        .set_rule("filesystem.write", PermissionAction::Ask)
        .unwrap();
    let provider = Arc::new(Timed {
        replies,
        next: Mutex::new(0),
    });
    let jarvis = Orchestrator::with_parts(config, provider, permissions);
    let conv = jarvis.new_conversation();
    Setup { dir, conv, jarvis }
}

impl Setup {
    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("turns.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn write_turn(&self) -> Vec<ChatTurn> {
        vec![
            call(
                "filesystem.write",
                json!({ "path": self.dir.join("notes.md"), "content": "hi" }),
            ),
            answer("done"),
        ]
    }
}

const WRITE: &str = "create a file called notes.md containing the word hi";

#[tokio::test]
async fn the_providers_breakdown_reaches_the_report() {
    let s = setup(vec![answer("hello there")]);
    let report = s
        .jarvis
        .run_turn(&s.conv, "tell me a joke", None, None, None)
        .await
        .unwrap();
    let model = &report.timing.model[0];
    assert_eq!(model.load_ms, Some(40));
    assert_eq!(model.prompt_ms, Some(30));
    assert_eq!(model.gen_ms, Some(20));
    assert!(
        report.timing.total_ms >= model.wall_ms,
        "{:?}",
        report.timing
    );
}

#[tokio::test]
async fn time_spent_on_a_prompt_is_the_owners_not_jarvis() {
    let s = setup(vec![]);
    let jarvis = rebuild(&s, s.write_turn());
    let s = Setup {
        conv: jarvis.new_conversation(),
        jarvis,
        dir: s.dir.clone(),
    };
    let owner = Slow {
        delay: std::time::Duration::from_millis(120),
        yes: true,
    };
    let report = s
        .jarvis
        .run_turn(&s.conv, WRITE, Some(&owner), None, None)
        .await
        .unwrap();
    assert!(report.timing.you_ms >= 120, "{:?}", report.timing);
    assert_eq!(report.timing.tools[0].name, "filesystem.write");
    assert!(
        report.timing.jarvis_ms() < report.timing.total_ms,
        "{:?}",
        report.timing
    );
}

/// The same setup with another script (setup() owns the folder).
fn rebuild(s: &Setup, replies: Vec<ChatTurn>) -> Orchestrator {
    let mut config = Config::default();
    config.general.data_dir = s.dir.clone();
    config.memory.db_path = s.dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(s.dir.clone());
    let permissions =
        PermissionManager::with_paths(s.dir.join("permissions.toml"), s.dir.join("audit.db"))
            .unwrap();
    Orchestrator::with_parts(
        config,
        Arc::new(Timed {
            replies,
            next: Mutex::new(0),
        }),
        permissions,
    )
}

#[tokio::test]
async fn every_turn_is_logged_with_its_friction() {
    let s = setup(vec![]);
    let jarvis = rebuild(&s, s.write_turn());
    let conv = jarvis.new_conversation();
    let no = Slow {
        delay: std::time::Duration::ZERO,
        yes: false,
    };
    jarvis
        .run_turn(&conv, WRITE, Some(&no), None, None)
        .await
        .unwrap();
    // Asked again in other words, straight away: a sign the first failed.
    jarvis
        .run_turn(
            &conv,
            "please create notes.md with the word hi in it",
            Some(&no),
            None,
            None,
        )
        .await
        .unwrap();

    let lines = s.lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let first = &lines[0];
    assert_eq!(first["type"], "turn");
    assert_eq!(first["route"], "Model");
    assert!(first["input"].as_str().unwrap().contains("notes.md"));
    assert_eq!(first["calls"][0]["status"], "declined");
    assert!(first["timing"]["total_ms"].is_number());
    let signals = |l: &Value| -> Vec<String> {
        l["signals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(signals(first), vec!["declined"]);
    assert!(
        signals(&lines[1]).contains(&"rephrased".to_string()),
        "{:?}",
        lines[1]
    );
}

#[tokio::test]
async fn a_cancelled_turn_is_logged_too() {
    let s = setup(vec![answer("never seen")]);
    // Cancelled before it starts: run_turn returns Err, and the log says so.
    let registry = jarvis_core::cancel::TurnRegistry::default();
    let id = uuid::Uuid::new_v4();
    let (cancel, _g) = registry.begin(id);
    registry.cancel(id);
    let result = s
        .jarvis
        .run_turn(&s.conv, "tell me a joke", None, Some(&cancel), None)
        .await;
    assert!(result.is_err());
    let lines = s.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0]["signals"]
        .as_array()
        .unwrap()
        .contains(&json!("cancelled")));
}

#[tokio::test]
async fn bad_and_good_point_at_the_last_turn() {
    let s = setup(vec![answer("the capital of Australia is Sydney")]);
    s.jarvis
        .run_turn(
            &s.conv,
            "what is the capital of australia?",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    s.jarvis.feedback(&s.conv, "bad", "it is Canberra").unwrap();
    let lines = s.lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["type"], "feedback");
    assert_eq!(lines[1]["kind"], "bad");
    assert_eq!(lines[1]["note"], "it is Canberra");
    assert_eq!(lines[1]["turn"], lines[0]["id"]);
}

#[tokio::test]
async fn feedback_before_any_turn_says_so() {
    let s = setup(vec![]);
    let err = s
        .jarvis
        .feedback(&s.conv, "bad", "")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no turn"), "{}", err);
}

#[tokio::test]
async fn a_long_turn_reports_what_it_is_doing() {
    // Real use: a 50-second remote turn of five model rounds looked hung -
    // only a spinner, nothing to tell working from stuck.
    let s = setup(vec![]);
    let jarvis = rebuild(&s, s.write_turn());
    let conv = jarvis.new_conversation();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let yes = Slow {
        delay: std::time::Duration::ZERO,
        yes: true,
    };
    jarvis
        .run_turn_observed(&conv, WRITE, Some(&yes), None, None, Some(&tx))
        .await
        .unwrap();
    drop(tx);
    let mut steps = Vec::new();
    while let Some(step) = rx.recv().await {
        steps.push(step);
    }
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert!(steps[0].starts_with("asking llama3.2:1b"), "{steps:?}");
    assert!(steps[0].contains("step 1"), "{steps:?}");
    assert_eq!(steps[1], "writing notes.md");
    assert!(steps[2].contains("step 2"), "{steps:?}");
}

#[tokio::test]
async fn an_answer_never_shows_our_untrusted_marker() {
    // Real use: a turn with no tools came back as a "find" result inside a
    // copy of our marker - paths the model made up.
    let s = setup(vec![answer(
        "<untrusted source=\"$ find /mnt/DATA -iname '*kusuriya*'\">\n/mnt/DATA/Anime/ep01.mkv\n</untrusted>",
    )]);
    let report = s
        .jarvis
        .run_turn(&s.conv, "tell me a joke", None, None, None)
        .await
        .unwrap();
    assert!(!report.answer.contains("<untrusted"), "{}", report.answer);
    assert!(
        report.answer.contains("nothing was searched or run"),
        "{}",
        report.answer
    );
}
