//! `jarvis eval`: run the eval suite and report pass rates.
//!
//! Results are appended to `evals/results/<run>.jsonl`, one line per
//! attempt, tagged with the model and git SHA so two runs can be compared
//! after the fact.

use anyhow::{bail, Context};
use jarvis_core::config::Config;
use jarvis_core::eval::{self, case, check::Grading, Attempt, RunOptions, ScriptedProvider};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct EvalArgs {
    pub dir: PathBuf,
    pub filter: Option<String>,
    pub repeat: usize,
    pub model: Option<String>,
    pub offline: bool,
    pub record: bool,
    pub timeout: u64,
}

pub async fn run(args: EvalArgs) -> anyhow::Result<()> {
    let mut cases = case::load_dir(&args.dir.join("cases"))?;
    if let Some(filter) = &args.filter {
        cases.retain(|c| c.id.contains(filter.as_str()) || c.tags.iter().any(|t| t == filter));
    }
    if args.record {
        cases.retain(|c| c.web);
    }
    if cases.is_empty() {
        bail!("no cases selected");
    }

    let mut base = Config::load();
    if let Some(model) = &args.model {
        base.model.default_model = model.clone();
    }
    let (provider, grading): (Arc<dyn jarvis_core::provider::Provider>, _) = if args.offline {
        (Arc::new(ScriptedProvider), Grading::Structural)
    } else {
        (jarvis_core::provider::from_config(&base)?, Grading::Full)
    };
    let model = if args.offline {
        "scripted".to_string()
    } else {
        base.model.default_model.clone()
    };

    let sha = git_sha();
    let run_id = format!(
        "{}-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        model.replace(['/', ':'], "_"),
        sha
    );
    let results_dir = args.dir.join("results");
    std::fs::create_dir_all(&results_dir)?;
    let results_path = results_dir.join(format!("{}.jsonl", run_id));
    let mut results = std::fs::File::create(&results_path)
        .with_context(|| format!("cannot create {}", results_path.display()))?;

    let options = RunOptions {
        provider: provider.clone(),
        grading,
        repeat: if args.record { 1 } else { args.repeat },
        timeout: Duration::from_secs(args.timeout),
        tapes: args.dir.join("tapes"),
        tape_script: std::fs::canonicalize(args.dir.join("tape_mcp.py"))
            .context("evals/tape_mcp.py not found")?,
        record: args.record,
        policy: Some(args.dir.join("permissions.toml")).filter(|p| p.exists()),
        base,
    };

    println!(
        "Eval {}: {} case(s) x {} | {} {} | grading {:?}{}",
        run_id,
        cases.len(),
        options.repeat,
        provider.name(),
        model,
        grading,
        if args.record {
            " | RECORDING tapes"
        } else {
            ""
        }
    );

    let meta = json!({
        "run": run_id, "sha": sha, "provider": provider.name(), "model": model,
        "grading": format!("{:?}", grading),
    });
    let mut write_error = None;
    let attempts = eval::run_suite(&cases, &options, |attempt| {
        print_attempt(attempt);
        let mut record = attempt.to_json();
        merge(&mut record, &meta);
        if let Err(e) = writeln!(results, "{}", record) {
            write_error.get_or_insert(e);
        }
    })
    .await;
    if let Some(e) = write_error {
        eprintln!("warning: some results were not written: {}", e);
    }

    print_summary(&cases, &attempts);
    println!("Results: {}", results_path.display());
    Ok(())
}

fn print_attempt(attempt: &Attempt) {
    let mark = if attempt.passed { "PASS" } else { "FAIL" };
    println!(
        "{} {} #{} ({} ms, route {})",
        mark,
        attempt.case,
        attempt.attempt,
        attempt.latency.as_millis(),
        if attempt.report.route.is_empty() {
            "-"
        } else {
            &attempt.report.route
        }
    );
    if let Some(error) = &attempt.error {
        println!("     error: {}", error);
    }
    for check in attempt.failed_checks() {
        println!("     {}: {}", check.check, check.detail);
    }
}

fn print_summary(cases: &[case::Case], attempts: &[Attempt]) {
    let rates = eval::pass_rates(attempts);
    println!("\nPer case:");
    for (id, passed, total) in &rates {
        println!("  {}/{}  {}", passed, total, id);
    }

    // The mean per-case rate weighs every case equally however many times
    // it ran; "fully passing" is the stricter number to track over time.
    let mean = rates
        .iter()
        .map(|(_, p, t)| *p as f64 / *t as f64)
        .sum::<f64>()
        / rates.len() as f64;
    let full = rates.iter().filter(|(_, p, t)| p == t).count();
    println!(
        "\nScore: {:.1}% mean pass rate | {}/{} cases pass every attempt",
        mean * 100.0,
        full,
        rates.len()
    );

    let mut tags: Vec<&str> = cases
        .iter()
        .flat_map(|c| c.tags.iter().map(String::as_str))
        .collect();
    tags.sort();
    tags.dedup();
    for tag in tags {
        let ids: Vec<&str> = cases
            .iter()
            .filter(|c| c.tags.iter().any(|t| t == tag))
            .map(|c| c.id.as_str())
            .collect();
        let (passed, total) = attempts
            .iter()
            .filter(|a| ids.contains(&a.case.as_str()))
            .fold((0, 0), |(p, t), a| (p + a.passed as usize, t + 1));
        println!("  {:<12} {}/{}", tag, passed, total);
    }

    let mut latencies: Vec<u128> = attempts
        .iter()
        .filter(|a| a.report.model_calls > 0)
        .map(|a| a.latency.as_millis())
        .collect();
    latencies.sort();
    if !latencies.is_empty() {
        let at = |q: f64| latencies[((latencies.len() - 1) as f64 * q).round() as usize];
        println!(
            "Latency of turns that used the model: p50 {} ms, p95 {} ms",
            at(0.5),
            at(0.95)
        );
    }
}

fn merge(record: &mut Value, meta: &Value) {
    if let (Some(record), Some(meta)) = (record.as_object_mut(), meta.as_object()) {
        for (k, v) in meta {
            record.insert(k.clone(), v.clone());
        }
    }
}

/// Short SHA, marked dirty when the tree has uncommitted changes: a result
/// from uncommitted code cannot be reproduced from the SHA alone.
fn git_sha() -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let Some(sha) = run(&["rev-parse", "--short", "HEAD"]) else {
        return "nogit".to_string();
    };
    let dirty =
        run(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    if dirty {
        format!("{}-dirty", sha)
    } else {
        sha
    }
}
