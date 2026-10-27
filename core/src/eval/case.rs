//! The eval case format.
//!
//! Cases are TOML so they can be read and reviewed without reading Rust. A
//! file holds any number of `[[case]]` tables:
//!
//! ```toml
//! [[case]]
//! id = "fs.list.sandbox"
//! input = "list contents of {sandbox}"
//! tags = ["filesystem"]
//! files = { "notes.txt" = "buy milk" }
//!
//! [case.expect]
//! route = "Tool(filesystem.list_dir)"
//! tools = ["filesystem.list_dir"]
//! contains = ["notes.txt"]
//! ```
//!
//! `{sandbox}` stands for the case's throwaway directory, where `files` are
//! created. It is substituted in the input, the setup turns and every
//! expectation.
//!
//! Every table rejects unknown keys. A misspelt expectation (`contain =`)
//! would otherwise be ignored, and the case would pass while checking less
//! than it appears to - the silent default the learning log warns about.

use crate::error::{JarvisError, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Stable identifier, `area.what`. Results are compared across runs by
    /// it, so renaming a case starts its history over.
    pub id: String,
    pub input: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Why the case exists, for whoever reads a failure.
    #[serde(default)]
    pub note: Option<String>,
    /// Files to create in the sandbox, relative path to content.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Earlier turns in the same session, run before `input` and not graded.
    #[serde(default)]
    pub setup: Vec<String>,
    /// How the scripted user answers a permission prompt. Declining is the
    /// default because a case that approves really runs the tool.
    #[serde(default)]
    pub approve: bool,
    /// Serve the `search` and `fetch` MCP servers from this case's tape.
    #[serde(default)]
    pub web: bool,
    /// Send to a recording "remote" model instead of the real one, so the
    /// case can check what would have left the machine.
    #[serde(default)]
    pub remote: bool,
    pub expect: Expect,
}

/// What a passing turn looks like. Every field is optional; an absent field
/// is not checked.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// `Direct`, `Model` or `Tool(name)`.
    pub route: Option<String>,
    /// The exact, ordered tool calls attempted. `[]` means no tool at all.
    pub tools: Option<Vec<String>>,
    /// Tools that must be among the calls, in any order.
    #[serde(default)]
    pub tools_include: Vec<String>,
    /// Tools that must not be attempted.
    #[serde(default)]
    pub tools_exclude: Vec<String>,
    /// Arguments of the first call to a tool. Only the keys given are
    /// compared, so `{ path = "{sandbox}" }` ignores other arguments.
    #[serde(default)]
    pub args: BTreeMap<String, toml::Table>,
    /// The exact, ordered tools the user was asked to approve.
    pub asked: Option<Vec<String>>,
    /// Substrings the answer must contain, case-insensitively.
    #[serde(default)]
    pub contains: Vec<String>,
    /// At least one of these must appear, for answers with several fair
    /// spellings ("4" or "four").
    #[serde(default)]
    pub contains_any: Vec<String>,
    /// Substrings the answer must not contain.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Expected content of sandbox files after the turn. Any other change to
    /// the sandbox fails the case.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Text that must reach the remote model (remote cases only).
    #[serde(default)]
    pub sent_contains: Vec<String>,
    /// Text that must not reach the remote model (remote cases only).
    #[serde(default)]
    pub sent_excludes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    #[serde(default)]
    case: Vec<Case>,
}

impl Case {
    /// The case with `{sandbox}` replaced throughout.
    pub fn resolve(&self, sandbox: &str) -> Case {
        let sub = |s: &str| s.replace("{sandbox}", sandbox);
        let subs = |v: &[String]| v.iter().map(|s| sub(s)).collect::<Vec<_>>();
        let mut case = self.clone();
        case.input = sub(&case.input);
        case.setup = subs(&case.setup);
        let e = &mut case.expect;
        e.contains = subs(&e.contains);
        e.contains_any = subs(&e.contains_any);
        e.excludes = subs(&e.excludes);
        e.sent_contains = subs(&e.sent_contains);
        e.sent_excludes = subs(&e.sent_excludes);
        for table in e.args.values_mut() {
            for (_, value) in table.iter_mut() {
                substitute_value(value, sandbox);
            }
        }
        case
    }

    /// Mistakes a TOML parser cannot see.
    fn validate(&self) -> std::result::Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("empty id".into());
        }
        if self.input.trim().is_empty() {
            return Err(format!("{}: empty input", self.id));
        }
        if let Some(route) = &self.expect.route {
            let ok = route == "Direct"
                || route == "Model"
                || (route.starts_with("Tool(") && route.ends_with(')') && route.len() > 6);
            if !ok {
                return Err(format!(
                    "{}: route '{}' is not Direct, Model or Tool(name)",
                    self.id, route
                ));
            }
        }
        let checks_sent =
            !self.expect.sent_contains.is_empty() || !self.expect.sent_excludes.is_empty();
        if checks_sent && !self.remote {
            // Without a recording model nothing is captured, and an empty
            // capture would pass every sent_excludes.
            return Err(format!(
                "{}: sent_contains/sent_excludes need remote = true",
                self.id
            ));
        }
        for path in self.files.keys().chain(self.expect.files.keys()) {
            if Path::new(path).is_absolute() || path.split('/').any(|part| part == "..") {
                return Err(format!(
                    "{}: file '{}' must stay inside the sandbox",
                    self.id, path
                ));
            }
        }
        Ok(())
    }
}

fn substitute_value(value: &mut toml::Value, sandbox: &str) {
    match value {
        toml::Value::String(s) => *s = s.replace("{sandbox}", sandbox),
        toml::Value::Array(items) => items.iter_mut().for_each(|v| substitute_value(v, sandbox)),
        toml::Value::Table(table) => table
            .iter_mut()
            .for_each(|(_, v)| substitute_value(v, sandbox)),
        _ => {}
    }
}

/// Parse one case file.
pub fn parse(text: &str, origin: &str) -> Result<Vec<Case>> {
    let file: CaseFile =
        toml::from_str(text).map_err(|e| JarvisError::Config(format!("{}: {}", origin, e)))?;
    for case in &file.case {
        case.validate()
            .map_err(|e| JarvisError::Config(format!("{}: {}", origin, e)))?;
    }
    Ok(file.case)
}

/// Every case under `dir`, from `*.toml` files in name order.
///
/// Fails on the first unreadable file or duplicate id rather than running
/// the cases it could read: a partial suite reports a pass rate for a
/// different suite.
pub fn load_dir(dir: &Path) -> Result<Vec<Case>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| JarvisError::Config(format!("cannot read {}: {}", dir.display(), e)))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();

    let mut cases = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        let text = std::fs::read_to_string(&path).map_err(JarvisError::Io)?;
        for case in parse(&text, &path.display().to_string())? {
            if !seen.insert(case.id.clone()) {
                return Err(JarvisError::Config(format!(
                    "{}: duplicate case id '{}'",
                    path.display(),
                    case.id
                )));
            }
            cases.push(case);
        }
    }
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = r#"
        [[case]]
        id = "fs.list"
        input = "list {sandbox}"
        files = { "a.txt" = "x" }
        [case.expect]
        route = "Tool(filesystem.list_dir)"
        args = { "filesystem.list_dir" = { path = "{sandbox}" } }
        contains = ["a.txt"]
    "#;

    #[test]
    fn a_case_parses_with_defaults() {
        let cases = parse(ONE, "test").expect("parses");
        assert_eq!(cases.len(), 1);
        assert!(!cases[0].approve);
        assert!(cases[0].expect.tools.is_none());
    }

    #[test]
    fn checking_what_was_sent_needs_a_remote_case() {
        let text = "[[case]]\nid = \"e\"\ninput = \"x\"\n[case.expect]\nsent_excludes = [\"k\"]";
        let error = parse(text, "test").expect_err("rejected").to_string();
        assert!(error.contains("remote = true"), "{}", error);
        let remote = text.replace("input = \"x\"", "input = \"x\"\nremote = true");
        assert!(parse(&remote, "test").is_ok());
    }

    #[test]
    fn a_misspelt_expectation_is_an_error_not_ignored() {
        let text = ONE.replace("contains =", "contain =");
        let error = parse(&text, "test").expect_err("rejected").to_string();
        assert!(error.contains("contain"), "{error}");
    }

    #[test]
    fn a_malformed_route_is_rejected() {
        let text = ONE.replace("Tool(filesystem.list_dir)", "list_dir");
        assert!(parse(&text, "test").is_err());
    }

    #[test]
    fn a_fixture_cannot_escape_the_sandbox() {
        let text = ONE.replace("\"a.txt\"", "\"../a.txt\"");
        assert!(parse(&text, "test").is_err());
    }

    #[test]
    fn the_sandbox_is_substituted_everywhere() {
        let case = parse(ONE, "test").unwrap().remove(0).resolve("/tmp/box");
        assert_eq!(case.input, "list /tmp/box");
        assert_eq!(
            case.expect.args["filesystem.list_dir"]["path"].as_str(),
            Some("/tmp/box")
        );
    }
}
