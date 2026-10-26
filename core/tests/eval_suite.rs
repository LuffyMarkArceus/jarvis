//! The eval suite, graded without a model.
//!
//! CI has no model server, so a scripted provider stands in and only what
//! the code decides is graded: routes, deterministic tool calls, permission
//! prompts and the sandbox. That still catches the regressions a router or
//! planner change can cause, and it proves every case file parses. Grading
//! the answers themselves needs `jarvis eval` against a real model.

use jarvis_core::config::Config;
use jarvis_core::eval::{self, case, check::Grading, RunOptions, ScriptedProvider};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn every_case_holds_without_a_model() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../evals");
    let cases = case::load_dir(&root.join("cases")).expect("case files parse");
    assert!(cases.len() >= 20, "suite shrank to {} cases", cases.len());

    let options = RunOptions {
        base: Config::default(),
        provider: Arc::new(ScriptedProvider),
        grading: Grading::Structural,
        repeat: 1,
        timeout: Duration::from_secs(30),
        tapes: root.join("tapes"),
        tape_script: root
            .join("tape_mcp.py")
            .canonicalize()
            .expect("tape script"),
        record: false,
        policy: Some(root.join("permissions.toml")),
    };
    let attempts = eval::run_suite(&cases, &options, |_| {}).await;

    let failures: Vec<String> = attempts
        .iter()
        .filter(|a| !a.passed)
        .map(|a| {
            let mut lines = vec![format!("  {} (route {})", a.case, a.report.route)];
            lines.extend(a.error.iter().map(|e| format!("    error: {}", e)));
            lines.extend(
                a.failed_checks()
                    .map(|c| format!("    {}: {}", c.check, c.detail)),
            );
            lines.join("\n")
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} case(s) failed structurally:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
