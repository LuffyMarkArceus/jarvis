//! How long a tool-using turn may run, and what stops it.
//!
//! The round limit was 4, tuned on a 1B model whose long turns were loops.
//! A capable model's long turns are research, so the limit is now higher and
//! per tier, and loops are stopped by noticing that a call repeats.

use jarvis_core::cancel::Cancellation;
use jarvis_core::config::Config;
use jarvis_core::error::Result;
use jarvis_core::model::{ChatMessage, ChatTurn, ModelFunction, ModelToolCall};
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator, TurnReport};
use jarvis_core::permissions::PermissionManager;
use jarvis_core::provider::{ChatRequest, Provider};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

/// A model that plays a script: one reply per request, the last repeated.
struct Script {
    replies: Vec<ChatTurn>,
    next: Mutex<usize>,
}

impl Script {
    fn new(replies: Vec<ChatTurn>) -> Self {
        Script {
            replies,
            next: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Script {
    fn name(&self) -> &'static str {
        "script"
    }

    async fn chat_turn(
        &self,
        _request: ChatRequest<'_>,
        _cancel: Option<&Cancellation>,
    ) -> Result<ChatTurn> {
        let mut next = self.next.lock().unwrap();
        let turn = self.replies[(*next).min(self.replies.len() - 1)].clone();
        *next += 1;
        Ok(turn)
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
        _sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        self.chat_turn(request, cancel).await
    }
}

fn call(name: &str, args: Value) -> ChatTurn {
    ChatTurn {
        message: ChatMessage::assistant_with_tool_calls(
            "",
            vec![ModelToolCall {
                id: None,
                function: ModelFunction {
                    index: None,
                    name: name.into(),
                    arguments: args,
                },
            }],
        ),
        prompt_tokens: None,
        completion_tokens: None,
    }
}

fn answer(text: &str) -> ChatTurn {
    ChatTurn {
        message: ChatMessage::text("assistant", text),
        prompt_tokens: None,
        completion_tokens: None,
    }
}

/// Says yes to everything and counts the prompts.
struct Yes(Mutex<usize>);

#[async_trait::async_trait]
impl ConfirmationHandler for Yes {
    async fn confirm(&self, _tool: &str, _args: &Value) -> bool {
        *self.0.lock().unwrap() += 1;
        true
    }
}

struct Setup {
    dir: PathBuf,
    jarvis: Orchestrator,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sandbox() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jarvis-rounds-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(dir: PathBuf, replies: Vec<ChatTurn>, max_rounds: Option<usize>) -> Setup {
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.model.max_rounds = max_rounds;
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let jarvis = Orchestrator::with_parts(config, Arc::new(Script::new(replies)), permissions);
    Setup { dir, jarvis }
}

fn write(dir: &std::path::Path, n: usize) -> ChatTurn {
    call(
        "filesystem.write",
        json!({ "path": dir.join(format!("notes{}.md", n)), "content": "hello" }),
    )
}

impl Setup {
    async fn ask(&self, user: &Yes) -> TurnReport {
        let input = format!(
            "create a file called notes.md in {} containing the word hello",
            self.dir.display()
        );
        let report = self
            .jarvis
            .run_turn(&input, Some(user), None, None)
            .await
            .unwrap();
        assert_eq!(report.route, "Model");
        report
    }
}

fn writes(report: &TurnReport) -> usize {
    report
        .calls
        .iter()
        .filter(|c| c.name == "filesystem.write" && c.status == "executed")
        .count()
}

#[tokio::test]
async fn a_repeated_call_is_not_run_again_and_ends_the_turn() {
    let dir = sandbox();
    // The 1B's loop: the same write, round after round.
    let s = setup(dir.clone(), vec![write(&dir, 1)], None);
    let user = Yes(Mutex::new(0));
    let report = s.ask(&user).await;

    assert_eq!(writes(&report), 1, "{:?}", report.calls);
    assert_eq!(*user.0.lock().unwrap(), 1, "asked once, for the one write");
    // The second round was only a repeat: there is no third.
    assert_eq!(report.model_calls, 2);
}

#[tokio::test]
async fn a_capable_model_gets_more_than_four_rounds() {
    let dir = sandbox();
    let mut script: Vec<ChatTurn> = (1..=5).map(|n| write(&dir, n)).collect();
    script.push(answer("Wrote five files."));
    let s = setup(dir, script, None);
    let report = s.ask(&Yes(Mutex::new(0))).await;

    assert_eq!(writes(&report), 5, "{:?}", report.calls);
    assert_eq!(report.model_calls, 6);
    assert!(
        report.answer.contains("Wrote five files"),
        "{}",
        report.answer
    );
}

#[tokio::test]
async fn max_rounds_is_set_per_tier() {
    let dir = sandbox();
    let script: Vec<ChatTurn> = (1..=5).map(|n| write(&dir, n)).collect();
    let s = setup(dir, script, Some(2));
    let report = s.ask(&Yes(Mutex::new(0))).await;

    assert_eq!(writes(&report), 2, "{:?}", report.calls);
    assert_eq!(report.model_calls, 2);
}
