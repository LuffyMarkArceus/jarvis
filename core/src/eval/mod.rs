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
pub mod diff;

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
    /// The model never answered (rate limited, unreachable). Such an attempt
    /// measures the provider's availability, not the model, so it is
    /// reported apart and left out of pass rates.
    pub unavailable: bool,
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
            "unavailable": self.unavailable,
            "model_error": self.report.model_error,
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
    let run = match result {
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
                unavailable: false,
            }
        }
    };
    let checks = check::grade(
        &run.case,
        &Observed {
            report: &run.report,
            asked: &run.asked,
            before: &run.before,
            after: &run.after,
            sent: &run.sent,
        },
        options.grading,
    );
    let unavailable = run
        .report
        .model_error
        .as_deref()
        .is_some_and(is_unavailable);
    Attempt {
        unavailable,
        case: case.id.clone(),
        attempt,
        passed: !unavailable && run.error.is_none() && checks.iter().all(|c| c.passed),
        checks,
        report: run.report,
        asked: run.asked,
        latency: run.latency,
        error: run.error,
    }
}

/// Everything one attempt observed, before grading.
struct Run {
    /// The case with `{sandbox}` resolved.
    case: Case,
    report: TurnReport,
    asked: Vec<String>,
    before: Snapshot,
    after: Snapshot,
    latency: Duration,
    error: Option<String>,
    /// What the recording remote model received in the graded turn.
    sent: Vec<String>,
}

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

    let mut config = attempt_config(&case, sandbox, options)?;
    // A remote case talks to a recording model on a non-local address, so
    // the egress check applies and what it lets through can be graded.
    let recorder = case
        .remote
        .then(|| Arc::new(RecordingProvider::replying(SCRIPTED_REPLY)));
    let provider: Arc<dyn Provider> = match &recorder {
        Some(r) => {
            config.model.base_url = "https://remote.invalid/v1".into();
            config.model.locality = Some("remote".into());
            r.clone()
        }
        None => options.provider.clone(),
    };
    let permissions = PermissionManager::with_paths(policy, sandbox.data.join("audit.db"))?;
    let orchestrator = Orchestrator::with_parts(config, provider, permissions);
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
    let sent_before = recorder.as_ref().map_or(0, |r| r.requests().len());
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
    let sent = recorder
        .map(|r| r.requests().split_off(sent_before))
        .unwrap_or_default();
    Ok(Run {
        case,
        report,
        asked,
        before,
        after,
        latency,
        error,
        sent,
    })
}

/// The user's provider settings with everything stateful moved into the
/// sandbox.
fn attempt_config(case: &Case, sandbox: &Sandbox, options: &RunOptions) -> Result<Config> {
    let mut config = options.base.clone();
    config.general.data_dir = sandbox.data.clone();
    config.memory.db_path = sandbox.data.join("memory.db");
    config.mcp = None;
    // Fixture files are not private. With a remote model, asking before
    // sending them would grade the privacy rules instead of the model; the
    // egress cases (`remote = true`) test those rules and keep them.
    if !case.remote {
        config.egress.file_allow = vec![sandbox.files.display().to_string()];
    }
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

/// A stand-in model that keeps every request it is sent, as the provider
/// would see it: system prompt and messages, after any redaction. Used to
/// check what would have left the machine.
#[derive(Default)]
pub struct RecordingProvider {
    /// Each request, flattened to text.
    pub sent: std::sync::Mutex<Vec<String>>,
    /// What it answers.
    pub reply: String,
}

impl RecordingProvider {
    pub fn replying(reply: &str) -> Self {
        RecordingProvider {
            sent: Default::default(),
            reply: reply.to_string(),
        }
    }

    pub fn requests(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Provider for RecordingProvider {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn chat_turn(
        &self,
        request: ChatRequest<'_>,
        _cancel: Option<&crate::cancel::Cancellation>,
    ) -> Result<ChatTurn> {
        let mut text = request.system.unwrap_or_default().to_string();
        for m in request.messages {
            text.push_str(&format!("\n[{}] {}", m.role, m.content));
        }
        self.sent.lock().unwrap().push(text);
        Ok(ChatTurn {
            message: ChatMessage::text("assistant", &self.reply),
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
        })
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&crate::cancel::Cancellation>,
        sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        let turn = self.chat_turn(request, cancel).await?;
        let _ = sink.send(turn.message.content.clone());
        Ok(turn)
    }
}

/// Pass rate per case, in suite order.
/// Does this model error mean the provider did not answer, rather than that
/// our request was wrong? Only the first is availability. A 400 is a bug on
/// our side and must count as a failure: the first version of this check
/// excluded every error and hid one (tool names a strict server rejected).
pub fn is_unavailable(error: &str) -> bool {
    let busy_status = error
        .split("HTTP ")
        .skip(1)
        .filter_map(|rest| rest.get(..3)?.parse::<u16>().ok())
        .any(|code| code == 429 || (500..600).contains(&code));
    busy_status
        || [
            "error sending request",
            "timed out",
            "connection",
            "provider returned no choices",
        ]
        .iter()
        .any(|marker| error.contains(marker))
}

/// Attempts where the model never answered are not counted, so a case can
/// have a total of 0.
pub fn pass_rates(attempts: &[Attempt]) -> Vec<(String, usize, usize)> {
    let mut rates: Vec<(String, usize, usize)> = Vec::new();
    for attempt in attempts {
        if attempt.unavailable {
            if !rates.iter().any(|(id, _, _)| *id == attempt.case) {
                rates.push((attempt.case.clone(), 0, 0));
            }
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(case: &str, passed: bool, unavailable: bool) -> Attempt {
        Attempt {
            case: case.into(),
            attempt: 1,
            passed,
            checks: Vec::new(),
            report: TurnReport::default(),
            asked: Vec::new(),
            latency: Duration::ZERO,
            error: None,
            unavailable,
        }
    }

    #[test]
    fn only_a_provider_that_did_not_answer_is_unavailable() {
        assert!(is_unavailable(
            "Model error: HTTP 429 Too Many Requests: busy"
        ));
        assert!(is_unavailable(
            "Model error: HTTP 529 <unknown status code>: overloaded"
        ));
        assert!(is_unavailable("Model error: error sending request for url"));
        // Our request was wrong: a failure, not bad luck.
        assert!(!is_unavailable(
            "Model error: HTTP 400 Bad Request: Function at index 0 has an invalid name"
        ));
        assert!(!is_unavailable("Model error: HTTP 401 Unauthorized"));
    }

    #[test]
    fn an_attempt_the_model_never_answered_is_not_scored() {
        let attempts = [
            attempt("a", true, false),
            attempt("a", false, true),
            attempt("b", false, true),
        ];
        assert_eq!(
            pass_rates(&attempts),
            vec![("a".to_string(), 1, 1), ("b".to_string(), 0, 0)]
        );
    }
}
