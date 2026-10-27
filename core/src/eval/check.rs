//! Grading one turn against a case.
//!
//! Every check is deterministic: string containment, exact tool sequences,
//! file contents. A model grading another model's answer is noisy, and the
//! local 1B model cannot grade itself; when a judge is added it will be one
//! more check here, reported separately, not a replacement for these.

use super::case::Case;
use crate::orchestrator::TurnReport;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// One expectation and whether the turn met it.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub check: String,
    pub passed: bool,
    /// What was seen, for a failure message that needs no re-run.
    pub detail: String,
}

/// How much of a case can be graded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Grading {
    /// A real model answered: every expectation applies.
    Full,
    /// A scripted stand-in answered. Its text means nothing, and it never
    /// calls tools, so only what the code decides is graded: the route, and
    /// for routes that do not consult the model, the calls and the sandbox.
    Structural,
}

/// Sandbox contents, relative path to bytes.
pub type Snapshot = BTreeMap<String, Vec<u8>>;

pub struct Observed<'a> {
    pub report: &'a TurnReport,
    /// Tools the scripted user was asked to approve, in order.
    pub asked: &'a [String],
    pub before: &'a Snapshot,
    pub after: &'a Snapshot,
    /// Requests the recording remote model received in the graded turn.
    pub sent: &'a [String],
}

/// Grade `observed` against an already-resolved `case`.
pub fn grade(case: &Case, observed: &Observed, grading: Grading) -> Vec<Check> {
    let expect = &case.expect;
    let report = observed.report;
    let mut checks = Vec::new();
    let mut add = |check: String, passed: bool, detail: String| {
        checks.push(Check {
            check,
            passed,
            detail,
        })
    };

    if let Some(route) = &expect.route {
        add(
            "route".into(),
            &report.route == route,
            format!("expected {}, got {}", route, report.route),
        );
    }

    // A scripted model never picks a tool, so what the model would have
    // called cannot be graded without a real one.
    let calls_graded =
        grading == Grading::Full || expect.route.as_deref().is_some_and(|r| r != "Model");
    let called: Vec<&str> = report.calls.iter().map(|c| c.name.as_str()).collect();

    if calls_graded {
        if let Some(tools) = &expect.tools {
            add(
                "tools".into(),
                called == tools.iter().map(String::as_str).collect::<Vec<_>>(),
                format!("expected {:?}, got {:?}", tools, called),
            );
        }
        for tool in &expect.tools_include {
            add(
                format!("tools_include {}", tool),
                called.contains(&tool.as_str()),
                format!("calls were {:?}", called),
            );
        }
        for (tool, wanted) in &expect.args {
            let actual = report.calls.iter().find(|c| &c.name == tool);
            let (passed, detail) = match actual {
                None => (false, format!("{} was not called", tool)),
                Some(call) => args_match(wanted, &call.args),
            };
            add(format!("args {}", tool), passed, detail);
        }
        if let Some(asked) = &expect.asked {
            add(
                "asked".into(),
                observed.asked == asked.as_slice(),
                format!("expected prompts for {:?}, got {:?}", asked, observed.asked),
            );
        }
    }
    for tool in &expect.tools_exclude {
        add(
            format!("tools_exclude {}", tool),
            !called.contains(&tool.as_str()),
            format!("calls were {:?}", called),
        );
    }

    if grading == Grading::Full {
        let answer = report.answer.to_lowercase();
        for needle in &expect.contains {
            add(
                format!("contains {:?}", needle),
                answer.contains(&needle.to_lowercase()),
                excerpt(&report.answer),
            );
        }
        if !expect.contains_any.is_empty() {
            add(
                format!("contains_any {:?}", expect.contains_any),
                expect
                    .contains_any
                    .iter()
                    .any(|n| answer.contains(&n.to_lowercase())),
                excerpt(&report.answer),
            );
        }
        for needle in &expect.excludes {
            add(
                format!("excludes {:?}", needle),
                !answer.contains(&needle.to_lowercase()),
                excerpt(&report.answer),
            );
        }
    }

    // What reached the remote model is decided by code, so it is graded
    // with or without a real model.
    let sent = observed.sent.join("\n");
    for needle in &expect.sent_contains {
        add(
            format!("sent_contains {:?}", needle),
            sent.contains(needle.as_str()),
            format!("{} request(s) sent", observed.sent.len()),
        );
    }
    for needle in &expect.sent_excludes {
        let at = sent.find(needle.as_str());
        add(
            format!("sent_excludes {:?}", needle),
            at.is_none(),
            match at {
                Some(i) => format!("sent: ...{}...", around(&sent, i, 40)),
                None => format!("{} request(s) sent", observed.sent.len()),
            },
        );
    }

    // Always graded: an unexpected write is wrong whoever chose it. When the
    // writes were the model's to make and no model ran, the right sandbox is
    // an untouched one.
    let mut wanted = observed.before.clone();
    if calls_graded {
        for (path, content) in &expect.files {
            wanted.insert(path.clone(), content.clone().into_bytes());
        }
    }
    let changed = diff_paths(&wanted, observed.after);
    add(
        "files".into(),
        changed.is_empty(),
        if changed.is_empty() {
            "sandbox as expected".into()
        } else {
            format!("unexpected sandbox state for {:?}", changed)
        },
    );

    checks
}

/// Compare only the keys the case names.
///
/// Strings are compared without a trailing `/`, since `/tmp/x` and `/tmp/x/`
/// name the same directory and the router may produce either.
fn args_match(wanted: &toml::Table, actual: &Value) -> (bool, String) {
    for (key, want) in wanted {
        let want = toml_to_json(want);
        let got = actual.get(key).cloned().unwrap_or(Value::Null);
        let equal = match (&want, &got) {
            (Value::String(a), Value::String(b)) => trim_slash(a) == trim_slash(b),
            _ => want == got,
        };
        if !equal {
            return (false, format!("{}: expected {}, got {}", key, want, got));
        }
    }
    (true, format!("args were {}", actual))
}

fn trim_slash(s: &str) -> &str {
    if s.len() > 1 {
        s.trim_end_matches('/')
    } else {
        s
    }
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn diff_paths(wanted: &Snapshot, actual: &Snapshot) -> Vec<String> {
    let mut paths: Vec<String> = wanted
        .keys()
        .chain(actual.keys())
        .filter(|p| wanted.get(*p) != actual.get(*p))
        .cloned()
        .collect();
    paths.dedup();
    paths
}

/// Up to `radius` characters either side of byte `at`, on char boundaries.
fn around(text: &str, at: usize, radius: usize) -> String {
    let start = text[..at]
        .char_indices()
        .rev()
        .nth(radius)
        .map_or(0, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .nth(radius)
        .map_or(text.len(), |(i, _)| at + i);
    text[start..end].replace('\n', " ")
}

/// Enough of the answer to see why a check failed.
fn excerpt(answer: &str) -> String {
    let flat = answer.replace('\n', " ");
    if flat.chars().count() <= 160 {
        format!("answer: {}", flat)
    } else {
        format!("answer: {}...", flat.chars().take(160).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::case::parse;
    use crate::orchestrator::ToolCallRecord;
    use serde_json::json;

    fn case(expect: &str) -> Case {
        let text = format!(
            "[[case]]\nid = \"t\"\ninput = \"x\"\n[case.expect]\n{}",
            expect
        );
        parse(&text, "test").unwrap().remove(0)
    }

    fn report(route: &str, calls: &[(&str, Value)], answer: &str) -> TurnReport {
        TurnReport {
            answer: answer.into(),
            route: route.into(),
            calls: calls
                .iter()
                .map(|(name, args)| ToolCallRecord {
                    name: name.to_string(),
                    args: args.clone(),
                    status: "executed",
                })
                .collect(),
            ..Default::default()
        }
    }

    fn failures(case: &Case, report: &TurnReport, grading: Grading) -> Vec<String> {
        let empty = Snapshot::new();
        let observed = Observed {
            report,
            asked: &[],
            before: &empty,
            after: &empty,
            sent: &[],
        };
        grade(case, &observed, grading)
            .into_iter()
            .filter(|c| !c.passed)
            .map(|c| c.check)
            .collect()
    }

    #[test]
    fn a_matching_turn_passes_every_check() {
        let c = case(
            "route = \"Tool(filesystem.read)\"\ntools = [\"filesystem.read\"]\n\
             args = { \"filesystem.read\" = { path = \"/tmp/a\" } }\ncontains = [\"HELLO\"]",
        );
        let r = report(
            "Tool(filesystem.read)",
            &[("filesystem.read", json!({"path": "/tmp/a/", "max": 5}))],
            "file says hello",
        );
        assert!(failures(&c, &r, Grading::Full).is_empty());
    }

    #[test]
    fn an_empty_tool_list_means_no_tools() {
        let c = case("tools = []");
        let r = report("Model", &[("shell.run", json!({}))], "");
        assert_eq!(failures(&c, &r, Grading::Full), vec!["tools"]);
    }

    #[test]
    fn structural_grading_ignores_text_and_model_chosen_calls() {
        let c = case("route = \"Model\"\ntools = [\"shell.run\"]\ncontains = [\"x\"]");
        let r = report("Model", &[], "mock");
        assert!(failures(&c, &r, Grading::Structural).is_empty());
        assert_eq!(failures(&c, &r, Grading::Full).len(), 2);
    }

    #[test]
    fn a_wrong_argument_is_reported_with_both_values() {
        let c = case("args = { \"filesystem.read\" = { path = \"/a\" } }");
        let r = report("Model", &[("filesystem.read", json!({"path": "/b"}))], "");
        let empty = Snapshot::new();
        let checks = grade(
            &c,
            &Observed {
                report: &r,
                asked: &[],
                before: &empty,
                after: &empty,
                sent: &[],
            },
            Grading::Full,
        );
        let failed = checks.iter().find(|c| !c.passed).expect("a failure");
        assert!(failed.detail.contains("/a") && failed.detail.contains("/b"));
    }

    #[test]
    fn an_unexpected_file_change_fails() {
        let c = case("route = \"Direct\"");
        let r = report("Direct", &[], "");
        let before = Snapshot::from([("a".to_string(), b"1".to_vec())]);
        let after = Snapshot::from([("a".to_string(), b"2".to_vec())]);
        let checks = grade(
            &c,
            &Observed {
                report: &r,
                asked: &[],
                before: &before,
                after: &after,
                sent: &[],
            },
            Grading::Structural,
        );
        assert!(checks.iter().any(|c| c.check == "files" && !c.passed));
    }
}
