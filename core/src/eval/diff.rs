//! Comparing two eval runs.
//!
//! A run's absolute score mostly measures the model: llama3.2:1b tops out
//! near 85% on this suite for reasons no code change will fix. What a code
//! change can do is move individual cases, so during development the useful
//! question is "which cases changed since the last run of the same model".

use crate::error::{JarvisError, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// One results file, reduced to what a comparison needs.
#[derive(Debug, Default)]
pub struct Run {
    pub id: String,
    pub model: String,
    pub sha: String,
    pub grading: String,
    pub cases: BTreeMap<String, CaseResult>,
}

#[derive(Debug, Default, Clone)]
pub struct CaseResult {
    pub passed: usize,
    pub total: usize,
    /// `check: detail` lines from the first failing attempt.
    pub failures: Vec<String>,
    /// Milliseconds, for attempts that called the model.
    pub latencies: Vec<u64>,
}

impl CaseResult {
    fn rate(&self) -> f64 {
        self.passed as f64 / self.total.max(1) as f64
    }
}

impl Run {
    /// Parse a `.jsonl` results file. Run metadata comes from the first line.
    pub fn parse(text: &str, origin: &str) -> Result<Run> {
        let mut run = Run::default();
        for (n, line) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let record: Value = serde_json::from_str(line)
                .map_err(|e| JarvisError::Config(format!("{}:{}: {}", origin, n + 1, e)))?;
            let field = |k: &str| {
                record
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            if run.id.is_empty() {
                run.id = field("run");
                run.model = field("model");
                run.sha = field("sha");
                run.grading = field("grading");
            }
            // The model never answered: availability, not quality.
            if record.get("unavailable").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let case = run.cases.entry(field("case")).or_default();
            case.total += 1;
            if record.get("passed").and_then(Value::as_bool) == Some(true) {
                case.passed += 1;
            } else if case.failures.is_empty() {
                case.failures = failures(&record);
            }
            if record
                .get("model_calls")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
            {
                if let Some(ms) = record.get("latency_ms").and_then(Value::as_u64) {
                    case.latencies.push(ms);
                }
            }
        }
        if run.cases.is_empty() {
            return Err(JarvisError::Config(format!("{}: no results", origin)));
        }
        Ok(run)
    }
}

fn p50(mut ms: Vec<u64>) -> Option<u64> {
    ms.sort_unstable();
    ms.get(ms.len().saturating_sub(1) / 2).copied()
}

fn failures(record: &Value) -> Vec<String> {
    let mut lines: Vec<String> = record
        .get("failed")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let get = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
            format!("{}: {}", get("check"), get("detail"))
        })
        .collect();
    if let Some(error) = record.get("error").and_then(Value::as_str) {
        lines.insert(0, format!("error: {}", error));
    }
    lines
}

/// A case whose pass rate moved.
#[derive(Debug)]
pub struct CaseChange {
    pub id: String,
    pub old: (usize, usize),
    pub new: (usize, usize),
    /// Failures in the new run, so a regression can be read without opening
    /// the results file.
    pub failures: Vec<String>,
}

impl CaseChange {
    /// Moved by a single attempt out of the same number. With a small model
    /// and three attempts that happens by chance, so it is reported as such
    /// rather than hidden: a real regression can also start as one attempt.
    pub fn may_be_noise(&self) -> bool {
        self.old.1 == self.new.1 && self.old.0.abs_diff(self.new.0) == 1
    }
}

#[derive(Debug, Default)]
pub struct Diff {
    pub regressed: Vec<CaseChange>,
    pub improved: Vec<CaseChange>,
    pub unchanged: usize,
    pub only_old: Vec<String>,
    pub only_new: Vec<String>,
    /// Mean pass rates over the cases both runs contain. A run filtered to a
    /// few cases has a different mean for that reason alone.
    pub old_mean: f64,
    pub new_mean: f64,
    /// Median latency over the same shared cases, for model turns.
    pub old_p50_ms: Option<u64>,
    pub new_p50_ms: Option<u64>,
}

/// Compare the cases both runs contain. Rates rather than counts are
/// compared, so runs with different `--repeat` can still be read side by side.
pub fn compare(old: &Run, new: &Run) -> Diff {
    let mut diff = Diff::default();
    let (mut old_ms, mut new_ms) = (Vec::new(), Vec::new());
    for (id, before) in &old.cases {
        let Some(after) = new.cases.get(id) else {
            diff.only_old.push(id.clone());
            continue;
        };
        old_ms.extend(&before.latencies);
        new_ms.extend(&after.latencies);
        diff.old_mean += before.rate();
        diff.new_mean += after.rate();
        let change = CaseChange {
            id: id.clone(),
            old: (before.passed, before.total),
            new: (after.passed, after.total),
            failures: after.failures.clone(),
        };
        if after.rate() < before.rate() {
            diff.regressed.push(change);
        } else if after.rate() > before.rate() {
            diff.improved.push(change);
        } else {
            diff.unchanged += 1;
        }
    }
    let shared = (old.cases.len() - diff.only_old.len()).max(1) as f64;
    diff.old_mean /= shared;
    diff.new_mean /= shared;
    diff.old_p50_ms = p50(old_ms);
    diff.new_p50_ms = p50(new_ms);
    diff.only_new = new
        .cases
        .keys()
        .filter(|id| !old.cases.contains_key(*id))
        .cloned()
        .collect();
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(case: &str, passed: bool) -> String {
        let failed = if passed {
            "[]"
        } else {
            r#"[{"check":"route","passed":false,"detail":"expected Direct, got Model"}]"#
        };
        format!(
            r#"{{"run":"r","model":"m","sha":"s","grading":"Full","case":"{}","passed":{},"failed":{},"error":null,"model_calls":1,"latency_ms":100}}"#,
            case, passed, failed
        )
    }

    fn run(results: &[(&str, &[bool])]) -> Run {
        let text: Vec<String> = results
            .iter()
            .flat_map(|(case, attempts)| attempts.iter().map(move |p| line(case, *p)))
            .collect();
        Run::parse(&text.join("\n"), "test").unwrap()
    }

    #[test]
    fn a_results_file_is_reduced_to_pass_counts_per_case() {
        let r = run(&[("a", &[true, false, true]), ("b", &[true])]);
        assert_eq!((r.cases["a"].passed, r.cases["a"].total), (2, 3));
        assert_eq!(
            r.cases["a"].failures,
            vec!["route: expected Direct, got Model"]
        );
        assert_eq!(r.model, "m");
        assert_eq!(r.cases["a"].latencies.len(), 3);
    }

    #[test]
    fn only_cases_that_moved_are_listed() {
        let old = run(&[
            ("same", &[true; 3]),
            ("worse", &[true; 3]),
            ("better", &[false; 3]),
        ]);
        let new = run(&[
            ("same", &[true; 3]),
            ("worse", &[false; 3]),
            ("better", &[true; 3]),
        ]);
        let d = compare(&old, &new);
        assert_eq!(d.unchanged, 1);
        assert_eq!(d.regressed.len(), 1);
        assert_eq!(d.regressed[0].id, "worse");
        assert_eq!(d.regressed[0].failures.len(), 1);
        assert_eq!(d.improved[0].id, "better");
        assert!(!d.regressed[0].may_be_noise());
    }

    #[test]
    fn a_single_attempt_move_is_flagged_as_possible_noise() {
        let old = run(&[("a", &[true, true, true])]);
        let new = run(&[("a", &[true, true, false])]);
        assert!(compare(&old, &new).regressed[0].may_be_noise());
    }

    #[test]
    fn rates_are_compared_across_different_repeat_counts() {
        let old = run(&[("a", &[true, false])]);
        let new = run(&[("a", &[true, true, false, false])]);
        let d = compare(&old, &new);
        assert_eq!(d.unchanged, 1);
    }

    #[test]
    fn cases_in_one_run_only_are_reported_not_compared() {
        let old = run(&[("gone", &[true]), ("kept", &[true])]);
        let new = run(&[("kept", &[true]), ("added", &[false])]);
        let d = compare(&old, &new);
        assert_eq!(d.only_old, vec!["gone"]);
        assert_eq!(d.only_new, vec!["added"]);
        assert!(d.regressed.is_empty());
        // "added" failed, but it is not in both runs, so it does not count.
        assert_eq!((d.old_mean, d.new_mean), (1.0, 1.0));
        assert_eq!(d.new_p50_ms, Some(100));
    }

    #[test]
    fn an_empty_file_is_an_error() {
        assert!(Run::parse("\n", "empty").is_err());
    }
}
