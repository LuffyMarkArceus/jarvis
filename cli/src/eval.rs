//! `jarvis eval`: run the eval suite and report pass rates.
//!
//! Results are appended to `evals/results/<run>.jsonl`, one line per
//! attempt, tagged with the model and git SHA so two runs can be compared
//! after the fact.

use anyhow::{bail, Context};
use jarvis_core::config::Config;
use jarvis_core::eval::diff::{self, CaseChange, Run};
use jarvis_core::eval::{self, case, check::Grading, Attempt, RunOptions, ScriptedProvider};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct EvalArgs {
    pub dir: PathBuf,
    pub filter: Option<String>,
    pub repeat: usize,
    pub model: Option<String>,
    pub offline: bool,
    /// Evaluate the `[remote]` model instead of `[model]`.
    pub remote: bool,
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
    if args.remote {
        let remote = base
            .remote
            .clone()
            .context("--remote needs a [remote] section in config.toml")?;
        base.model = remote.model;
    }
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

/// `jarvis eval diff`: print the cases that changed between two runs.
pub fn diff(results: &Path, old: Option<&str>, new: Option<&str>) -> anyhow::Result<()> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(results)
        .with_context(|| format!("no results in {}", results.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    // Run ids start with a UTC timestamp, so name order is time order.
    files.sort();
    let load = |path: &Path| -> anyhow::Result<Run> {
        let text = std::fs::read_to_string(path)?;
        Ok(Run::parse(&text, &path.display().to_string())?)
    };
    // A path, or the newest run whose id contains the text.
    let find = |name: &str| -> anyhow::Result<PathBuf> {
        let path = PathBuf::from(name);
        if path.is_file() {
            return Ok(path);
        }
        files
            .iter()
            .rev()
            .find(|p| {
                p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().contains(name))
            })
            .cloned()
            .with_context(|| format!("no run in {} matches {:?}", results.display(), name))
    };

    let new_path = match new {
        Some(name) => find(name)?,
        None => files.last().cloned().context("no runs to compare")?,
    };
    let new_run = load(&new_path)?;
    let old_run = match old {
        Some(name) => load(&find(name)?)?,
        // The previous run of the same model and grading - comparing against
        // another model would mostly measure the model - that shares the most
        // cases with the new one, newest first among equals. A run filtered
        // to five cases is a poor baseline for a full one, and when a suite
        // gains cases no earlier run covers them all.
        None => {
            let earlier: Vec<Run> = files
                .iter()
                .rev()
                .filter(|p| **p < new_path)
                .filter_map(|p| load(p).ok())
                .filter(|r| r.model == new_run.model && r.grading == new_run.grading)
                .collect();
            let shared = |r: &Run| {
                new_run
                    .cases
                    .keys()
                    .filter(|k| r.cases.contains_key(*k))
                    .count()
            };
            // `earlier` is newest first, and max_by_key keeps the last
            // maximum, so search it reversed to keep the newest.
            let pick = (0..earlier.len())
                .rev()
                .max_by_key(|i| shared(&earlier[*i]))
                .unwrap_or(0);
            earlier
                .into_iter()
                .nth(pick)
                .with_context(|| format!("no earlier {} run to compare with", new_run.model))?
        }
    };

    println!("old: {}\nnew: {}", old_run.id, new_run.id);
    if old_run.model != new_run.model {
        println!("note: different models, so changes include the model, not only the code");
    }
    let d = diff::compare(&old_run, &new_run);
    let print = |title: &str, changes: &[CaseChange], with_failures: bool| {
        if changes.is_empty() {
            return;
        }
        println!("\n{}:", title);
        for c in changes {
            println!(
                "  {}/{} -> {}/{}  {}{}",
                c.old.0,
                c.old.1,
                c.new.0,
                c.new.1,
                c.id,
                if c.may_be_noise() {
                    "  (1 attempt, may be noise)"
                } else {
                    ""
                }
            );
            if with_failures {
                for f in &c.failures {
                    println!("       {}", f);
                }
            }
        }
    };
    print("Worse", &d.regressed, true);
    print("Better", &d.improved, false);
    let list = |ids: &[String]| {
        if ids.len() <= 5 {
            ids.join(", ")
        } else {
            format!("{} cases", ids.len())
        }
    };
    if !d.only_new.is_empty() {
        println!("\nOnly in new: {}", list(&d.only_new));
    }
    if !d.only_old.is_empty() {
        println!("Only in old: {}", list(&d.only_old));
    }
    let latency = |ms: Option<u64>| {
        ms.map(|ms| format!("{:.1} s", ms as f64 / 1000.0))
            .unwrap_or_else(|| "-".into())
    };
    println!(
        "\n{} unchanged | mean pass rate on {} shared cases {:.1}% -> {:.1}% | p50 latency {} -> {}",
        d.unchanged,
        d.unchanged + d.regressed.len() + d.improved.len(),
        d.old_mean * 100.0,
        d.new_mean * 100.0,
        latency(d.old_p50_ms),
        latency(d.new_p50_ms)
    );
    if !d.regressed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
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
