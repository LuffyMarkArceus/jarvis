//! Permissions and audit system

use crate::config::Config;
use crate::error::{JarvisError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

impl PermissionAction {
    /// A human-readable description of what the action means.
    pub fn label(&self) -> &'static str {
        match self {
            PermissionAction::Allow => "run without asking",
            PermissionAction::Ask => "ask for confirmation",
            PermissionAction::Deny => "never run",
        }
    }
}

impl std::str::FromStr for PermissionAction {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "allow" => Ok(PermissionAction::Allow),
            "ask" => Ok(PermissionAction::Ask),
            "deny" => Ok(PermissionAction::Deny),
            other => Err(format!(
                "unknown action '{}' - expected allow, ask or deny",
                other
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    pub action: PermissionAction,
    /// Per-command exceptions to `action`, for tools that name a command.
    ///
    /// When a pattern matches, the most restrictive matching pattern wins
    /// (deny beats ask beats allow), so adding a narrow `allow` can never
    /// punch a hole through a broader `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patterns: Option<Vec<CommandPattern>>,
}

/// One pattern/action pair attached to a tool rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandPattern {
    /// Glob over the command string. `*` matches any run of characters,
    /// `?` matches exactly one. Matching is case-sensitive and anchored:
    /// the pattern must describe the whole command.
    pub pattern: String,
    pub action: PermissionAction,
}

/// What the policy says about one tool call, and what decided it.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionVerdict {
    pub action: PermissionAction,
    /// The pattern that decided the outcome, when one did.
    ///
    /// The tool-level action has no pattern; a pattern-driven decision does.
    /// Recording it is what makes an audit entry explainable later.
    pub pattern: Option<String>,
}

impl PermissionVerdict {
    fn from_rule(action: PermissionAction) -> Self {
        Self {
            action,
            pattern: None,
        }
    }
}

/// The argument that holds the command a pattern should match, if the tool
/// has one.
///
/// Only tools whose arguments describe a single command can support command
/// patterns. Everything else is decided by the tool-level rule alone.
pub fn command_field(tool: &str) -> Option<&'static str> {
    match tool {
        "shell.run" => Some("command"),
        _ => None,
    }
}

/// Whether `tool` supports per-command patterns.
pub fn supports_command_patterns(tool: &str) -> bool {
    command_field(tool).is_some()
}

/// Characters that let one command string become several, or change where
/// its output goes.
///
/// A glob cannot describe these safely: `git *` matches
/// `git status; rm -rf ~`, which is not what the person who wrote that rule
/// meant. Allow and ask patterns therefore only apply to commands without
/// them. Deny patterns are exempt because a deny match can only ever be
/// safer than not matching.
const SHELL_METACHARACTERS: &[char] = &[';', '&', '|', '`', '$', '>', '<', '\n', '\r'];

/// True when `command` is a single simple command with no shell syntax.
fn is_simple_command(command: &str) -> bool {
    !command.chars().any(|c| SHELL_METACHARACTERS.contains(&c))
}

/// Rank actions by restrictiveness so a match set can be reduced.
fn restrictiveness(action: &PermissionAction) -> u8 {
    match action {
        PermissionAction::Deny => 3,
        PermissionAction::Ask => 2,
        PermissionAction::Allow => 1,
    }
}

/// Match `pattern` against `text`, supporting `*` and `?`.
///
/// Deliberately tiny: no character classes, no escaping, no regex. A policy
/// file should be readable by a human deciding whether to trust it.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_text) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_text = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last `*` swallow one more character.
            pi = s + 1;
            star_text += 1;
            ti = star_text;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl PermissionsConfig {
    /// Decide a tool call against this policy.
    ///
    /// A tool with no rule defaults to `ask`: an unrecognised tool is not
    /// something to run unprompted.
    pub fn evaluate(&self, tool: &str, args: &Value) -> PermissionVerdict {
        let Some(rule) = self.rules.iter().find(|r| r.tool == tool) else {
            return PermissionVerdict::from_rule(PermissionAction::Ask);
        };

        if let (Some(field), Some(patterns)) = (command_field(tool), rule.patterns.as_ref()) {
            let command = args
                .get(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if let Some(verdict) = match_patterns(patterns, command) {
                return verdict;
            }
        }

        PermissionVerdict::from_rule(rule.action.clone())
    }
}

/// Reduce every matching pattern to the most restrictive one.
///
/// "Most restrictive wins" rather than "first match wins" is the difference
/// between a policy that composes and one that quietly depends on line
/// order. Adding `deny "rm *"` must protect you even if an earlier
/// `allow "*"` would also match.
fn match_patterns(patterns: &[CommandPattern], command: &str) -> Option<PermissionVerdict> {
    if command.is_empty() {
        return None;
    }
    let simple = is_simple_command(command);

    let mut best: Option<PermissionVerdict> = None;
    for p in patterns {
        // An allow or ask match can relax a stricter rule, so it is only
        // applied when it provably describes the whole command. A deny match
        // is always safe to apply.
        if p.action != PermissionAction::Deny && !simple {
            continue;
        }
        if !glob_match(&p.pattern, command) {
            continue;
        }
        let better = match &best {
            None => true,
            Some(current) => restrictiveness(&p.action) > restrictiveness(&current.action),
        };
        if better {
            best = Some(PermissionVerdict {
                action: p.action.clone(),
                pattern: Some(p.pattern.clone()),
            });
        }
    }
    best
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default = "default_rules")]
    pub rules: Vec<PermissionRule>,
}

fn default_rules() -> Vec<PermissionRule> {
    vec![
        PermissionRule {
            tool: "filesystem.list_dir".to_string(),
            action: PermissionAction::Allow,
            patterns: None,
        },
        PermissionRule {
            tool: "filesystem.read".to_string(),
            action: PermissionAction::Allow,
            patterns: None,
        },
        PermissionRule {
            tool: "system.info".to_string(),
            action: PermissionAction::Allow,
            patterns: None,
        },
        PermissionRule {
            tool: "shell.run".to_string(),
            action: PermissionAction::Ask,
            patterns: None,
        },
        PermissionRule {
            tool: "filesystem.write".to_string(),
            action: PermissionAction::Ask,
            patterns: None,
        },
        PermissionRule {
            tool: "filesystem.edit".to_string(),
            action: PermissionAction::Ask,
            patterns: None,
        },
    ]
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            rules: default_rules(),
        }
    }
}

/// A policy that refuses every tool Jarvis ships with.
///
/// Used when the policy file exists but cannot be read or parsed. Tools the
/// policy has never heard of still fall through to `ask`, which a headless
/// run treats as no.
fn deny_all() -> PermissionsConfig {
    PermissionsConfig {
        rules: crate::tools::known_tool_names()
            .into_iter()
            .map(|tool| PermissionRule {
                tool: tool.to_string(),
                action: PermissionAction::Deny,
                patterns: None,
            })
            .collect(),
    }
}

#[derive(Debug, Clone)]
pub struct PermissionManager {
    config_path: PathBuf,
    audit_conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub session_id: Option<String>,
    pub tool: String,
    pub args: Value,
    pub decision: String,
    pub result: Option<String>,
    pub timestamp: String,
}

impl PermissionManager {
    pub fn new(config: &Config) -> Result<Self> {
        std::fs::create_dir_all(&config.general.data_dir).map_err(JarvisError::Io)?;

        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("jarvis");
        std::fs::create_dir_all(&config_dir).map_err(JarvisError::Io)?;

        let config_path = config_dir.join("permissions.toml");
        if !config_path.exists() {
            let default_cfg = PermissionsConfig::default();
            let toml_str = toml::to_string_pretty(&default_cfg).unwrap_or_default();
            let _ = std::fs::write(&config_path, toml_str);
        }

        Self::with_paths(config_path, config.general.data_dir.join("audit.db"))
    }

    /// Build a manager over explicit paths.
    ///
    /// `new` is the production entry point; this exists so the policy logic
    /// can be tested against a throwaway file instead of the user's real
    /// configuration.
    pub fn with_paths(config_path: PathBuf, audit_db_path: PathBuf) -> Result<Self> {
        for path in [&config_path, &audit_db_path] {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(JarvisError::Io)?;
            }
        }

        let conn =
            Connection::open(&audit_db_path).map_err(|e| JarvisError::Memory(e.to_string()))?;
        let mgr = Self {
            config_path,
            audit_conn: Arc::new(Mutex::new(conn)),
        };
        mgr.init_audit_schema()?;
        Ok(mgr)
    }

    /// Read the policy from disk.
    ///
    /// The file is re-read on every call rather than cached. It is a few
    /// hundred bytes and tool decisions are rare, so the cost is
    /// negligible - and it means a policy change takes effect immediately,
    /// without restarting the daemon. A cached policy would silently keep
    /// applying stale rules after an operator changed them.
    ///
    /// A file that cannot be read or parsed denies every tool. Falling back
    /// to the built-in defaults would be worse than failing: the defaults
    /// are more permissive than most hand-written policies (they allow
    /// reading, for a start), so a typo would quietly widen access at
    /// exactly the moment the operator was trying to narrow it.
    fn load(&self) -> PermissionsConfig {
        match std::fs::read_to_string(&self.config_path) {
            Ok(content) => match toml::from_str::<PermissionsConfig>(&content) {
                Ok(config) => config,
                Err(e) => {
                    tracing::error!(
                        "{} is not valid permissions TOML ({}). Denying every tool until it is fixed.",
                        self.config_path.display(),
                        e
                    );
                    deny_all()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PermissionsConfig::default(),
            Err(e) => {
                tracing::error!(
                    "Cannot read {} ({}). Denying every tool until it is fixed.",
                    self.config_path.display(),
                    e
                );
                deny_all()
            }
        }
    }

    fn init_audit_schema(&self) -> Result<()> {
        let conn = self.audit_conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tool_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                decision TEXT NOT NULL,
                result TEXT,
                timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_audit_tool ON tool_audit(tool);
            CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON tool_audit(timestamp);
            ",
        )
        .map_err(|e| JarvisError::Memory(e.to_string()))?;
        Ok(())
    }

    pub fn check_permission(&self, tool: &str, args: &Value) -> PermissionVerdict {
        self.load().evaluate(tool, args)
    }

    /// The rules currently in force.
    pub fn rules(&self) -> Vec<PermissionRule> {
        self.load().rules
    }

    /// Set the action for a tool, adding a rule if the tool is not listed.
    pub fn set_rule(&self, tool: &str, action: PermissionAction) -> Result<()> {
        let mut config = self.load();
        match config.rules.iter_mut().find(|r| r.tool == tool) {
            Some(rule) => rule.action = action,
            None => config.rules.push(PermissionRule {
                tool: tool.to_string(),
                action,
                patterns: None,
            }),
        }
        self.save(&config)
    }

    /// Drop a rule, restoring the default behaviour for that tool.
    ///
    /// Returns whether a rule was actually removed.
    pub fn remove_rule(&self, tool: &str) -> Result<bool> {
        let mut config = self.load();
        let before = config.rules.len();
        config.rules.retain(|r| r.tool != tool);
        if config.rules.len() == before {
            return Ok(false);
        }
        self.save(&config)?;
        Ok(true)
    }

    /// Add or replace a per-command pattern on a tool rule.
    ///
    /// The tool-level action is left alone: patterns are exceptions layered
    /// on top of it, not a replacement for it.
    pub fn set_pattern(&self, tool: &str, pattern: &str, action: PermissionAction) -> Result<()> {
        if !supports_command_patterns(tool) {
            return Err(JarvisError::Config(format!(
                "{} has no command to match against; command patterns apply to: shell.run",
                tool
            )));
        }
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(JarvisError::Config("pattern is empty".to_string()));
        }

        let mut config = self.load();
        let rule = match config.rules.iter_mut().find(|r| r.tool == tool) {
            Some(rule) => rule,
            None => {
                // A tool with no rule is "ask" by default; make that explicit
                // so the patterns have a base to layer on.
                config.rules.push(PermissionRule {
                    tool: tool.to_string(),
                    action: PermissionAction::Ask,
                    patterns: None,
                });
                config.rules.last_mut().expect("just pushed a rule")
            }
        };

        let patterns = rule.patterns.get_or_insert_with(Vec::new);
        match patterns.iter_mut().find(|p| p.pattern == pattern) {
            Some(existing) => existing.action = action,
            None => patterns.push(CommandPattern {
                pattern: pattern.to_string(),
                action,
            }),
        }
        self.save(&config)
    }

    /// Remove a command pattern. Returns whether one was actually removed.
    pub fn remove_pattern(&self, tool: &str, pattern: &str) -> Result<bool> {
        let pattern = pattern.trim();
        let mut config = self.load();
        let Some(rule) = config.rules.iter_mut().find(|r| r.tool == tool) else {
            return Ok(false);
        };
        let Some(patterns) = rule.patterns.as_mut() else {
            return Ok(false);
        };
        let before = patterns.len();
        patterns.retain(|p| p.pattern != pattern);
        if patterns.len() == before {
            return Ok(false);
        }
        if patterns.is_empty() {
            // An empty table in the file reads like a rule that does
            // something; drop it so the file stays honest.
            rule.patterns = None;
        }
        self.save(&config)?;
        Ok(true)
    }

    fn save(&self, config: &PermissionsConfig) -> Result<()> {
        let toml_str = toml::to_string_pretty(config)
            .map_err(|e| JarvisError::Config(format!("Could not serialise permissions: {}", e)))?;
        std::fs::write(&self.config_path, toml_str).map_err(JarvisError::Io)
    }

    pub fn log_audit(
        &self,
        session_id: Option<&str>,
        tool: &str,
        args: &Value,
        decision: &str,
        result: Option<&str>,
    ) -> Result<()> {
        let conn = self.audit_conn.lock().unwrap();
        let args_str = serde_json::to_string(args).unwrap_or_default();
        conn.execute(
            "INSERT INTO tool_audit (session_id, tool, args, decision, result) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, tool, args_str, decision, result],
        )
        .map_err(|e| JarvisError::Memory(e.to_string()))?;
        Ok(())
    }

    pub fn get_audit_log(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.audit_conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, tool, args, decision, result, timestamp FROM tool_audit ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| JarvisError::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let args_str: String = row.get(3)?;
                let args: Value = serde_json::from_str(&args_str).unwrap_or(Value::Null);
                Ok(AuditEntry {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    tool: row.get(2)?,
                    args,
                    decision: row.get(4)?,
                    result: row.get(5)?,
                    timestamp: row.get(6)?,
                })
            })
            .map_err(|e| JarvisError::Memory(e.to_string()))?;
        let mut entries = Vec::new();
        for e in rows {
            entries.push(e.map_err(|e| JarvisError::Memory(e.to_string()))?);
        }
        Ok(entries)
    }

    pub fn get_config_path(&self) -> &PathBuf {
        &self.config_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pattern(pattern: &str, action: PermissionAction) -> CommandPattern {
        CommandPattern {
            pattern: pattern.to_string(),
            action,
        }
    }

    fn shell_rule(action: PermissionAction, patterns: Vec<CommandPattern>) -> PermissionsConfig {
        PermissionsConfig {
            rules: vec![PermissionRule {
                tool: "shell.run".to_string(),
                action,
                patterns: Some(patterns),
            }],
        }
    }

    fn action_for(config: &PermissionsConfig, command: &str) -> PermissionAction {
        config
            .evaluate("shell.run", &json!({ "command": command }))
            .action
    }

    // ---------------------------------------------------------------- basics

    #[test]
    fn unknown_tool_defaults_to_ask() {
        let config = PermissionsConfig { rules: vec![] };
        let verdict = config.evaluate("mystery.tool", &json!({}));
        assert_eq!(verdict.action, PermissionAction::Ask);
        assert!(verdict.pattern.is_none());
    }

    #[test]
    fn tool_rule_applies_when_no_pattern_matches() {
        let config = shell_rule(
            PermissionAction::Deny,
            vec![pattern("git *", PermissionAction::Allow)],
        );
        let verdict = config.evaluate("shell.run", &json!({"command": "make build"}));
        assert_eq!(verdict.action, PermissionAction::Deny);
        assert!(verdict.pattern.is_none());
    }

    #[test]
    fn allow_pattern_permits_a_matching_command() {
        let config = shell_rule(
            PermissionAction::Deny,
            vec![pattern("git *", PermissionAction::Allow)],
        );
        let verdict = config.evaluate("shell.run", &json!({"command": "git status"}));
        assert_eq!(verdict.action, PermissionAction::Allow);
        assert_eq!(verdict.pattern.as_deref(), Some("git *"));
    }

    // ------------------------------------------------------------- precedence

    #[test]
    fn deny_pattern_wins_over_allow_pattern() {
        // Both match "git push". Writing the allow first must not let it win.
        let config = shell_rule(
            PermissionAction::Allow,
            vec![
                pattern("git *", PermissionAction::Allow),
                pattern("git push*", PermissionAction::Deny),
            ],
        );
        assert_eq!(
            action_for(&config, "git push origin main"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn ask_pattern_wins_over_allow_pattern() {
        let config = shell_rule(
            PermissionAction::Deny,
            vec![
                pattern("git *", PermissionAction::Allow),
                pattern("git push*", PermissionAction::Ask),
            ],
        );
        assert_eq!(
            action_for(&config, "git push origin main"),
            PermissionAction::Ask
        );
    }

    #[test]
    fn most_specific_still_loses_to_a_broader_deny() {
        let config = shell_rule(
            PermissionAction::Allow,
            vec![
                pattern("git status", PermissionAction::Allow),
                pattern("git *", PermissionAction::Deny),
            ],
        );
        assert_eq!(action_for(&config, "git status"), PermissionAction::Deny);
    }

    // ------------------------------------------------------- shell safety

    #[test]
    fn deny_pattern_still_matches_a_compound_command() {
        // Denying is always safe, so it applies even when the command cannot
        // be described by a glob.
        let config = shell_rule(
            PermissionAction::Allow,
            vec![pattern("rm *", PermissionAction::Deny)],
        );
        assert_eq!(
            action_for(&config, "rm -rf /tmp/x; echo done"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn allow_pattern_does_not_match_a_compound_command() {
        // `git *` must not be read as permission to run a second command.
        let config = shell_rule(
            PermissionAction::Deny,
            vec![pattern("git *", PermissionAction::Allow)],
        );
        assert_eq!(
            action_for(&config, "git status; rm -rf /tmp/x"),
            PermissionAction::Deny
        );
        assert_eq!(
            action_for(&config, "git status && rm -rf /tmp/x"),
            PermissionAction::Deny
        );
        assert_eq!(
            action_for(&config, "git status | tee /tmp/log"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn allow_pattern_does_not_apply_to_command_substitution() {
        let config = shell_rule(
            PermissionAction::Deny,
            vec![pattern("echo *", PermissionAction::Allow)],
        );
        assert_eq!(
            action_for(&config, "echo $(cat /etc/shadow)"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn patterns_are_ignored_for_tools_without_a_command() {
        let config = PermissionsConfig {
            rules: vec![PermissionRule {
                tool: "filesystem.read".to_string(),
                action: PermissionAction::Deny,
                patterns: Some(vec![pattern("*", PermissionAction::Allow)]),
            }],
        };
        let verdict = config.evaluate("filesystem.read", &json!({"path": "/etc/shadow"}));
        assert_eq!(verdict.action, PermissionAction::Deny);
    }

    #[test]
    fn empty_command_falls_back_to_the_tool_rule() {
        let config = shell_rule(
            PermissionAction::Deny,
            vec![pattern("*", PermissionAction::Allow)],
        );
        assert_eq!(action_for(&config, ""), PermissionAction::Deny);
        assert_eq!(action_for(&config, "   "), PermissionAction::Deny);
    }

    // ---------------------------------------------------------------- globbing

    #[test]
    fn glob_matches_anchored_patterns() {
        assert!(glob_match("git *", "git status"));
        assert!(glob_match("git status", "git status"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("ls", "ls"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("a*b*", "ab"));
    }

    #[test]
    fn glob_does_not_match_when_it_should_not() {
        assert!(!glob_match("git *", "gitx"));
        assert!(!glob_match("git status", "git status -s"));
        assert!(!glob_match("ls", "ls -la"));
        assert!(!glob_match("git *", "sudo git status"));
        assert!(!glob_match("a*b", "abx"));
    }

    #[test]
    fn glob_supports_single_character_wildcard() {
        assert!(glob_match("ls ?", "ls -"));
        assert!(!glob_match("ls ?", "ls -la"));
    }

    // ------------------------------------------------------------ file round trip

    fn temp_manager() -> PermissionManager {
        let dir = std::env::temp_dir().join(format!("jarvis-perm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mgr = PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db"))
            .expect("manager");
        mgr.set_rule("shell.run", PermissionAction::Deny)
            .expect("shell rule");
        mgr
    }

    #[test]
    fn patterns_persist_to_the_file_and_take_effect() {
        let mgr = temp_manager();
        mgr.set_pattern("shell.run", "git *", PermissionAction::Allow)
            .expect("allow pattern");
        mgr.set_pattern("shell.run", "rm *", PermissionAction::Deny)
            .expect("deny pattern");

        // rules() re-reads the file, so this asserts the write actually landed.
        let rules = mgr.rules();
        let shell = rules
            .iter()
            .find(|r| r.tool == "shell.run")
            .expect("shell rule");
        let patterns = shell.patterns.as_ref().expect("patterns");
        assert_eq!(patterns.len(), 2);

        let allowed = mgr.check_permission("shell.run", &json!({"command": "git log"}));
        assert_eq!(allowed.action, PermissionAction::Allow);
        assert_eq!(allowed.pattern.as_deref(), Some("git *"));

        let denied = mgr.check_permission("shell.run", &json!({"command": "rm -rf /tmp/x"}));
        assert_eq!(denied.action, PermissionAction::Deny);

        // The tool-level rule still catches everything else.
        let unmatched = mgr.check_permission("shell.run", &json!({"command": "make"}));
        assert_eq!(unmatched.action, PermissionAction::Deny);
        assert!(unmatched.pattern.is_none());
    }

    #[test]
    fn setting_the_same_pattern_twice_updates_it() {
        let mgr = temp_manager();
        mgr.set_pattern("shell.run", "git *", PermissionAction::Allow)
            .expect("first");
        mgr.set_pattern("shell.run", "git *", PermissionAction::Ask)
            .expect("second");

        let rules = mgr.rules();
        let shell = rules.iter().find(|r| r.tool == "shell.run").expect("rule");
        let patterns = shell.patterns.as_ref().expect("patterns");
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].action, PermissionAction::Ask);
    }

    #[test]
    fn removing_the_last_pattern_drops_the_list() {
        let mgr = temp_manager();
        mgr.set_pattern("shell.run", "git *", PermissionAction::Allow)
            .expect("set");
        assert!(mgr.remove_pattern("shell.run", "git *").expect("remove"));
        assert!(!mgr.remove_pattern("shell.run", "git *").expect("again"));

        let rules = mgr.rules();
        let shell = rules.iter().find(|r| r.tool == "shell.run").expect("rule");
        assert!(shell.patterns.is_none());
    }

    #[test]
    fn patterns_are_rejected_for_tools_without_a_command() {
        let mgr = temp_manager();
        assert!(mgr
            .set_pattern("filesystem.read", "/etc/*", PermissionAction::Allow)
            .is_err());
    }

    #[test]
    fn empty_pattern_is_rejected() {
        let mgr = temp_manager();
        assert!(mgr
            .set_pattern("shell.run", "  ", PermissionAction::Allow)
            .is_err());
    }

    #[test]
    fn adding_a_pattern_to_an_unlisted_tool_creates_a_safe_base_rule() {
        let mgr = temp_manager();
        mgr.remove_rule("shell.run").expect("remove rule");
        assert!(mgr.rules().iter().all(|r| r.tool != "shell.run"));

        mgr.set_pattern("shell.run", "git *", PermissionAction::Allow)
            .expect("set pattern");

        let rules = mgr.rules();
        let shell = rules
            .iter()
            .find(|r| r.tool == "shell.run")
            .expect("rule was created");
        // The base must be Ask, not Allow: adding one permissive pattern must
        // not hand blanket permission to everything else.
        assert_eq!(shell.action, PermissionAction::Ask);
        assert_eq!(shell.patterns.as_ref().map(|p| p.len()), Some(1));

        let unmatched = mgr.check_permission("shell.run", &json!({"command": "make"}));
        assert_eq!(unmatched.action, PermissionAction::Ask);
    }
}

#[cfg(test)]
mod fail_closed_tests {
    use super::*;
    use serde_json::json;

    fn manager_with(content: &str) -> PermissionManager {
        let dir = std::env::temp_dir().join(format!("jarvis-perm-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("permissions.toml");
        std::fs::write(&path, content).expect("write policy");
        PermissionManager::with_paths(path, dir.join("audit.db")).expect("manager")
    }

    #[test]
    fn malformed_policy_denies_everything() {
        let mgr = manager_with("this is not toml [[[");

        for tool in ["shell.run", "filesystem.read", "filesystem.write"] {
            let verdict = mgr.check_permission(tool, &json!({"command": "ls", "path": "/tmp"}));
            assert_eq!(
                verdict.action,
                PermissionAction::Deny,
                "{} should be denied when the policy cannot be parsed",
                tool
            );
        }
    }

    #[test]
    fn a_missing_policy_file_uses_the_documented_defaults() {
        let dir = std::env::temp_dir().join(format!("jarvis-perm-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mgr = PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db"))
            .expect("manager");

        assert_eq!(
            mgr.check_permission("filesystem.read", &json!({"path": "/tmp/x"}))
                .action,
            PermissionAction::Allow
        );
        assert_eq!(
            mgr.check_permission("shell.run", &json!({"command": "ls"}))
                .action,
            PermissionAction::Ask
        );
    }

    #[test]
    fn old_string_patterns_do_not_silently_reinterpret_as_allow() {
        // The `patterns` field once held plain strings. That shape is now
        // invalid, and an invalid policy must fail closed rather than be
        // ignored.
        let mgr = manager_with(
            r#"
[[rules]]
tool = "shell.run"
action = "deny"
patterns = ["rm *"]
"#,
        );
        assert_eq!(
            mgr.check_permission("shell.run", &json!({"command": "ls"}))
                .action,
            PermissionAction::Deny
        );
    }
}
