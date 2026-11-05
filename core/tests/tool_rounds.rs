//! How long a tool-using turn may run, and what stops it.
//!
//! The round limit was 4, tuned on a 1B model whose long turns were loops.
//! A capable model's long turns are research, so the limit is now higher and
//! per tier, and loops are stopped by noticing that a call repeats.

mod common;

use common::{answer, call, Script};
use jarvis_core::config::Config;
use jarvis_core::model::ChatTurn;
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator, TurnReport};
use jarvis_core::permissions::PermissionManager;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    // The second round was only a repeat, so the loop ends; the closing
    // request is answered with the same call, which is not run either.
    assert_eq!(report.model_calls, 3);
    assert!(
        report.answer.starts_with("Here is what actually happened"),
        "{}",
        report.answer
    );
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
    // Two rounds, then the closing request, which answered with a third
    // write: not run, and the report is the answer.
    assert_eq!(report.model_calls, 3);
    assert!(
        report.answer.starts_with("Here is what actually happened"),
        "{}",
        report.answer
    );
}

#[tokio::test]
async fn out_of_rounds_the_model_is_asked_once_to_answer_from_what_it_found() {
    let dir = sandbox();
    let script = vec![
        write(&dir, 1),
        write(&dir, 2),
        answer("Wrote notes1.md and notes2.md."),
    ];
    let s = setup(dir, script, Some(2));
    let report = s.ask(&Yes(Mutex::new(0))).await;

    assert_eq!(writes(&report), 2, "{:?}", report.calls);
    assert_eq!(report.model_calls, 3);
    assert!(
        report.answer.contains("Wrote notes1.md and notes2.md"),
        "{}",
        report.answer
    );
}

#[tokio::test]
async fn with_nothing_found_there_is_no_closing_request() {
    let dir = sandbox();
    // A tool this request was not offered: each call fails without running.
    let script = (1..=3)
        .map(|n| call("shell.run", json!({ "command": format!("echo {}", n) })))
        .collect();
    let s = setup(dir, script, Some(2));
    let report = s.ask(&Yes(Mutex::new(0))).await;

    assert!(
        report.calls.iter().all(|c| c.status == "not_offered"),
        "{:?}",
        report.calls
    );
    assert_eq!(report.model_calls, 2);
}

// A relative path or a command runs in `[tools] work_dir` when it is set. An
// eval attempt once wrote `notes.md` into the repository it was started from.

#[tokio::test]
async fn a_relative_path_lands_in_the_work_dir() {
    let dir = sandbox();
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(work.clone());
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let name = format!("relative-{}.md", uuid::Uuid::new_v4());
    let script = vec![
        call("filesystem.write", json!({ "path": name, "content": "hello" })),
        answer("done"),
    ];
    let jarvis = Orchestrator::with_parts(config, Arc::new(Script::new(script)), permissions);
    let s = Setup { dir, jarvis };
    let report = s.ask(&Yes(Mutex::new(0))).await;

    assert_eq!(writes(&report), 1, "{:?}", report.calls);
    assert!(work.join(&name).exists(), "written in the work dir");
    assert!(!std::path::Path::new(&name).exists(), "not where the process started");
    assert_eq!(report.calls[0].args["path"], work.join(&name).display().to_string());
}

#[tokio::test]
async fn a_command_runs_in_the_work_dir() {
    let dir = sandbox();
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.work_dir = Some(work.clone());
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let jarvis = Orchestrator::with_parts(config, Arc::new(Script::new(vec![answer("x")])), permissions);
    let s = Setup { dir, jarvis };
    let report = s
        .jarvis
        .run_turn("run pwd", Some(&Yes(Mutex::new(0))), None, None)
        .await
        .unwrap();
    assert_eq!(report.route, "Tool(shell.run)");
    assert!(report.answer.contains(work.to_str().unwrap()), "{}", report.answer);
}
