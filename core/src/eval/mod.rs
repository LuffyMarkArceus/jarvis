//! The eval harness: run cases against a model and grade what happened.
//!
//! Unit tests prove that each piece does what its author meant. They cannot
//! say whether the assistant answers "what's in /tmp" correctly with a given
//! model, or whether a change to the router made answers worse. This module
//! measures that, so model choices and router rules are decided by numbers
//! instead of by a few hand-run `jarvis ask` sessions.
//!
//! Each attempt runs in isolation:
//!
//! * a fresh sandbox directory holds the case's fixture files;
//! * memory, audit log and permission policy live in the sandbox, so neither
//!   the user's history nor their policy changes a result, and a run leaves
//!   nothing behind;
//! * permission prompts are answered by a scripted user that records them;
//! * web cases talk to `search` and `fetch` replayed from a tape, so a score
//!   does not move because the web did.
//!
//! Models are not deterministic, so every case runs several times and is
//! reported as a pass rate.

pub mod case;
pub mod check;

use crate::config::{Config, McpConfig, McpServerConfig};
use crate::error::{JarvisError, Result};
use crate::model::{ChatMessage, ChatTurn};
use crate::orchestrator::{ConfirmationHandler, Orchestrator, TurnReport};
use crate::permissions::PermissionManager;
use crate::provider::{ChatRequest, Provider};
use case::Case;
use check::{Check, Grading, Observed, Snapshot};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// The MCP servers a web case gets, served from its tape.
const WEB_SERVERS: &[&str] = &["search", "fetch"];

pub struct RunOptions {
    /// Provider and model settings come from here; data paths and MCP
    /// servers are replaced per attempt.
    pub base: Config,
    pub provider: Arc<dyn Provider>,
    pub grading: Grading,
    pub repeat: usize,
    pub timeout: Duration,
    /// `evals/tapes`: one directory per web case.
    pub tapes: PathBuf,
    /// `evals/tape_mcp.py`.
    pub tape_script: PathBuf,
    /// The permission policy every attempt runs under. A committed file
    /// rather than the user's own, so a score does not depend on whose
    /// machine produced it. `None` uses the built-in defaults.
    pub policy: Option<PathBuf>,
    /// Record tapes from the real servers configured in `base` instead of
    /// replaying them.
    pub record: bool,
}

/// One run of one case.
#[derive(Debug)]
pub struct Attempt {
    pub case: String,
    pub attempt: usize,
    pub passed: bool,
    pub checks: Vec<Check>,
    pub report: TurnReport,
    pub asked: Vec<String>,
    pub latency: Duration,
    /// Set when the turn did not finish: an error or a timeout.
    pub error: Option<String>,
}

impl Attempt {
    pub fn failed_checks(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| !c.passed)
    }

    /// One JSONL record. The caller adds run-level fields (model, git SHA).
    pub fn to_json(&self) -> Value {
        json!({
            "case": self.case,
            "attempt": self.attempt,
            "passed": self.passed,
            "error": self.error,
            "failed": self.failed_checks().collect::<Vec<_>>(),
            "route": self.report.route,
            "offered": self.report.offered,
            "calls": self.report.calls.iter().map(|c| json!({
                "name": c.name, "args": c.args, "status": c.status,
            })).collect::<Vec<_>>(),
            "asked": self.asked,
            "latency_ms": self.latency.as_millis() as u64,
            "model_calls": self.report.model_calls,
            "prompt_tokens": self.report.prompt_tokens,
            "completion_tokens": self.report.completion_tokens,
            "answer": self.report.answer,
        })
    }
}

/// Run every case `repeat` times, calling `on_attempt` as each finishes.
pub async fn run_suite(
    cases: &[Case],
    options: &RunOptions,
    mut on_attempt: impl FnMut(&Attempt),
) -> Vec<Attempt> {
    let mut attempts = Vec::new();
    for case in cases {
        for n in 1..=options.repeat.max(1) {
            let attempt = run_case(case, n, options).await;
            on_attempt(&attempt);
            attempts.push(attempt);
        }
    }
    attempts
}

/// Run one attempt of `case` in a fresh sandbox.
pub async fn run_case(case: &Case, attempt: usize, options: &RunOptions) -> Attempt {
    let sandbox = Sandbox::new();
    let result = run_in(&sandbox, case, options).await;
    let (resolved, report, asked, before, after, latency, error) = match result {
        Ok(run) => run,
        Err(e) => {
            return Attempt {
                case: case.id.clone(),
                attempt,
                passed: false,
                checks: Vec::new(),
                report: TurnReport::default(),
                asked: Vec::new(),
                latency: Duration::ZERO,
                error: Some(e.to_string()),
            }
        }
    };
    let checks = check::grade(
        &resolved,
        &Observed {
            report: &report,
            asked: &asked,
            before: &before,
            after: &after,
        },
        options.grading,
    );
    Attempt {
        case: case.id.clone(),
        attempt,
        passed: error.is_none() && checks.iter().all(|c| c.passed),
        checks,
        report,
        asked,
        latency,
        error,
    }
}

type Run = (
    Case,
    TurnReport,
    Vec<String>,
    Snapshot,
    Snapshot,
    Duration,
    Option<String>,
);

async fn run_in(sandbox: &Sandbox, case: &Case, options: &RunOptions) -> Result<Run> {
    let case = case.resolve(&sandbox.files.display().to_string());
    if case.web && !options.record {
        for name in WEB_SERVERS {
            let tape = tape_path(&case, name, options);
            if !tape.exists() {
                // Without its tape a web case would be planned with no web
                // tools and fail as a routing error, which it is not.
                return Err(JarvisError::Config(format!(
                    "no tape at {}; record it with: jarvis eval --record --filter {}",
                    tape.display(),
                    case.id
                )));
            }
        }
    }
    sandbox.populate(&case)?;
    let policy = sandbox.root.join("permissions.toml");
    if let Some(source) = &options.policy {
        std::fs::copy(source, &policy)?;
    }

    let config = attempt_config(&case, sandbox, options)?;
    let permissions = PermissionManager::with_paths(policy, sandbox.data.join("audit.db"))?;
    let orchestrator = Orchestrator::with_parts(config, options.provider.clone(), permissions);
    if case.web {
        tokio::time::timeout(Duration::from_secs(30), orchestrator.start_mcp())
            .await
            .map_err(|_| JarvisError::Tool("MCP tape servers did not start".into()))??;
    }

    // Setup turns build the conversation the graded turn happens in.
    for turn in &case.setup {
        let user = ScriptedUser::new(case.approve);
        tokio::time::timeout(
            options.timeout,
            orchestrator.run_turn(turn, Some(&user), None, None),
        )
        .await
        .map_err(|_| JarvisError::Tool(format!("setup turn timed out: {}", turn)))??;
    }

    let before = snapshot(&sandbox.files);
    let user = ScriptedUser::new(case.approve);
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        options.timeout,
        orchestrator.run_turn(&case.input, Some(&user), None, None),
    )
    .await;
    let latency = started.elapsed();
    let after = snapshot(&sandbox.files);
    let asked = user.asked();

    let (report, error) = match outcome {
        Ok(Ok(report)) => (report, None),
        Ok(Err(e)) => (TurnReport::default(), Some(e.to_string())),
        Err(_) => (
            TurnReport::default(),
            Some(format!("timed out after {}s", options.timeout.as_secs())),
        ),
    };
    Ok((case, report, asked, before, after, latency, error))
}

/// The user's provider settings with everything stateful moved into the
/// sandbox.
fn attempt_config(case: &Case, sandbox: &Sandbox, options: &RunOptions) -> Result<Config> {
    let mut config = options.base.clone();
    config.general.data_dir = sandbox.data.clone();
    config.memory.db_path = sandbox.data.join("memory.db");
    config.mcp = None;
    if case.web {
        let mut servers = Vec::new();
        for name in WEB_SERVERS {
            servers.push(tape_server(name, case, options)?);
        }
        config.mcp = Some(McpConfig { servers });
    }
    Ok(config)
}

fn tape_server(name: &str, case: &Case, options: &RunOptions) -> Result<McpServerConfig> {
    let tape = tape_path(case, name, options);
    let mut args = vec![
        options.tape_script.display().to_string(),
        "--tape".to_string(),
        tape.display().to_string(),
    ];
    if options.record {
        let real = options
            .base
            .mcp
            .as_ref()
            .and_then(|m| m.servers.iter().find(|s| s.name == *name))
            .ok_or_else(|| {
                JarvisError::Config(format!(
                    "cannot record: no MCP server named '{}' in the config",
                    name
                ))
            })?;
        args.push("--record".to_string());
        args.push("--".to_string());
        args.push(real.command.clone());
        args.extend(real.args.iter().cloned());
    }
    Ok(McpServerConfig {
        name: name.to_string(),
        command: "python3".to_string(),
        args,
        // Spelled out: the derived `Default` disables a server, and only
        // deserialising from TOML turns `enabled` on.
        enabled: true,
        ..Default::default()
    })
}

fn tape_path(case: &Case, server: &str, options: &RunOptions) -> PathBuf {
    options
        .tapes
        .join(&case.id)
        .join(format!("{}.json", server))
}

/// Answers permission prompts the way the case says, and remembers them.
struct ScriptedUser {
    approve: bool,
    asked: Mutex<Vec<String>>,
}

impl ScriptedUser {
    fn new(approve: bool) -> Self {
        Self {
            approve,
            asked: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ConfirmationHandler for ScriptedUser {
    async fn confirm(&self, tool: &str, _args: &Value) -> bool {
        self.asked.lock().unwrap().push(tool.to_string());
        self.approve
    }
}

/// A throwaway directory per attempt, removed when dropped.
struct Sandbox {
    root: PathBuf,
    files: PathBuf,
    data: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("jarvis-eval-{}", uuid::Uuid::new_v4()));
        Self {
            files: root.join("files"),
            data: root.join("data"),
            root,
        }
    }

    fn populate(&self, case: &Case) -> Result<()> {
        std::fs::create_dir_all(&self.files)?;
        std::fs::create_dir_all(&self.data)?;
        for (path, content) in &case.files {
            let path = self.files.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
        Ok(())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Every file under `dir`, keyed by its path relative to `dir`.
fn snapshot(dir: &Path) -> Snapshot {
    fn walk(dir: &Path, base: &Path, out: &mut Snapshot) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&path, base, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let relative = path.strip_prefix(base).unwrap_or(&path);
                out.insert(relative.to_string_lossy().replace('\\', "/"), bytes);
            }
        }
    }
    let mut out = Snapshot::new();
    walk(dir, dir, &mut out);
    out
}

/// A stand-in model that never calls a tool and always says the same thing.
///
/// CI has no model server. With this provider the suite still checks
/// everything the code decides on its own - routes, deterministic tool
/// calls, permission prompts, the sandbox - under [`Grading::Structural`].
pub struct ScriptedProvider;

pub const SCRIPTED_REPLY: &str = "(scripted reply)";

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn chat_turn(
        &self,
        _request: ChatRequest<'_>,
        _cancel: Option<&crate::cancel::Cancellation>,
    ) -> Result<ChatTurn> {
        Ok(ChatTurn {
            message: ChatMessage::text("assistant", SCRIPTED_REPLY),
            prompt_tokens: None,
            completion_tokens: None,
        })
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&crate::cancel::Cancellation>,
        sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        let _ = sink.send(SCRIPTED_REPLY.to_string());
        self.chat_turn(request, cancel).await
    }
}

/// Pass rate per case, in suite order.
pub fn pass_rates(attempts: &[Attempt]) -> Vec<(String, usize, usize)> {
    let mut rates: Vec<(String, usize, usize)> = Vec::new();
    for attempt in attempts {
        match rates.iter_mut().find(|(id, _, _)| *id == attempt.case) {
            Some(rate) => {
                rate.1 += attempt.passed as usize;
                rate.2 += 1;
            }
            None => rates.push((attempt.case.clone(), attempt.passed as usize, 1)),
        }
    }
    rates
}
