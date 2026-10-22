//! Deterministic intent routing.
//!
//! # Why this exists
//!
//! Small local models cannot reliably decide *whether* a request needs a
//! tool, let alone which one. Measured against `llama3.2:1b` on this
//! machine, asking the model to classify a request gives the right answer
//! roughly a third of the time: "what is 2+2?" is classified as
//! `SYSTEM_INFO` as often as `CHAT`, and the model reaches for a tool even
//! for pure chit-chat. Tool *selection* therefore happens here, in code,
//! where it is exact, fast, testable and auditable.
//!
//! The model is still used for everything the router cannot decide:
//! `Route::Model` hands it the tool catalogue and lets it try. That path is
//! strictly a fallback - the common cases never reach it.
//!
//! # Precedence
//!
//! 1. Explicit slash commands (`/list /tmp`, `/read <file>`, `/run <cmd>`)
//! 2. System-information requests
//! 3. `run` / `execute` / `eval` followed by a command
//! 4. A path mentioned in the request -> list it or read it
//! 5. `open` / `launch` / `start` followed by an application
//! 6. "list the files" with no path -> the current directory
//! 7. Anything that smells like a machine action -> `Route::Model`
//! 8. Everything else -> `Route::Direct` (pure conversation)

use serde_json::{json, Value};
use std::path::Path;

/// What the orchestrator should do with a user request.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Pure conversation. Answer with the model, no tools involved.
    Direct,
    /// A specific tool call was identified with high confidence.
    Tool { name: String, args: Value },
    /// Ambiguous, but the request looks like it needs a tool. Let the model
    /// choose from the catalogue.
    Model,
}

/// Determiners that may precede an application name ("open **the** firefox").
const APP_ARTICLES: &[&str] = &[
    "the", "a", "an", "my", "your", "our", "their", "its", "his", "her", "some",
];

/// Words that never form part of an application name.
const APP_STOPWORDS: &[&str] = &[
    "browser",
    "app",
    "application",
    "window",
    "tab",
    "file",
    "files",
    "folder",
    "folders",
    "directory",
    "for",
    "me",
    "please",
    "and",
    "in",
    "on",
    "at",
    "of",
    "to",
    "from",
    "with",
    "by",
    "as",
    "can",
    "you",
    "could",
    "would",
    "should",
    "i",
    "we",
    "it",
    "they",
    "them",
    "using",
    "via",
    "kindly",
    "now",
    "then",
    "up",
    "down",
    "again",
    "all",
    "not",
    "no",
    "so",
    "if",
    "when",
    "while",
    "because",
    "but",
    "or",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "do",
    "does",
    "did",
    "have",
    "has",
    "had",
    "will",
    "still",
    "also",
    "too",
    "very",
    "just",
    "really",
    "actually",
    "maybe",
    "there",
    "here",
];

/// Tokens that indicate the user wants to list a directory.
const LISTING_WORDS: &[&str] = &[
    "list",
    "ls",
    "directory",
    "directories",
    "folder",
    "folders",
    "dir",
    "dirs",
    "subdirectory",
    "subdirectories",
];

/// Tokens that indicate the user wants the contents of a file.
const READING_WORDS: &[&str] = &[
    "read", "cat", "contents", "content", "inside", "print", "head",
];

/// Phrases that identify a request for machine specs.
const SYSTEM_INFO_PHRASES: &[&str] = &[
    "operating system",
    "system info",
    "system specs",
    "system information",
    "system architecture",
    "machine architecture",
    "cpu architecture",
    "processor architecture",
    "hostname",
    "host name",
    "computer name",
    "kernel version",
    "what os am i",
    "which os am i",
    "am i running linux",
    "am i running windows",
];

/// Tokens that suggest the user wants something done on the machine.
const ACTION_WORDS: &[&str] = &[
    "check",
    "find",
    "search",
    "grep",
    "install",
    "download",
    "fetch",
    "create",
    "delete",
    "remove",
    "copy",
    "move",
    "rename",
    "count",
    "monitor",
    "watch",
    "restart",
    "kill",
    "docker",
    "systemctl",
    "git",
    "ping",
    "curl",
    "ssh",
    "sudo",
    "apt",
    "pacman",
    "pip",
    "npm",
    "cargo",
];

/// Route a user request.
pub fn route(input: &str) -> Route {
    let raw = input.trim();
    if raw.is_empty() {
        return Route::Direct;
    }

    // 1. Explicit slash commands.
    if let Some(rest) = raw.strip_prefix('/') {
        if let Some(r) = route_command(rest) {
            return r;
        }
    }

    let lower = raw.to_lowercase();

    // 2. An explicit command.
    if let Some(cmd) = command_after(raw, &["run", "execute", "eval"]) {
        return Route::Tool {
            name: "shell.run".to_string(),
            args: json!({ "command": cmd }),
        };
    }

    // 3. A path was mentioned. This is checked before the system-info rules
    //    because a path such as /etc/hostname contains the word "hostname".
    if let Some(path) = first_path(raw, &lower) {
        return Route::Tool {
            name: path_tool(&path, &lower).to_string(),
            args: json!({ "path": path }),
        };
    }

    // 4. System information.
    if is_system_info(&strip_paths(&lower)) {
        return Route::Tool {
            name: "system.info".to_string(),
            args: json!({}),
        };
    }

    // 5. An application was named.
    if let Some(app) = app_after(raw) {
        return Route::Tool {
            name: "shell.run".to_string(),
            args: json!({ "command": app, "background": true }),
        };
    }

    // 6. "list the files" with no path.
    if mentions_listing(&lower) && mentions_files(&lower) {
        return Route::Tool {
            name: "filesystem.list_dir".to_string(),
            args: json!({ "path": "." }),
        };
    }

    // 7. Looks like a machine action the router cannot pin down.
    if has_action_word(&lower) {
        return Route::Model;
    }

    // 8. Conversation.
    Route::Direct
}

/// Handle `/list`, `/read`, `/run`, `/info`, `/ls`, `/cat`.
fn route_command(rest: &str) -> Option<Route> {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let verb = parts.next()?.trim().to_lowercase();
    let arg = parts.next().unwrap_or("").trim().to_string();

    let tool = match verb.as_str() {
        "list" | "ls" => "filesystem.list_dir",
        "read" | "cat" => "filesystem.read",
        "run" | "sh" | "shell" => "shell.run",
        "info" | "system" => "system.info",
        _ => return None,
    };

    let args = match tool {
        "filesystem.list_dir" => json!({ "path": if arg.is_empty() { "." } else { &arg } }),
        "filesystem.read" => json!({ "path": arg }),
        "shell.run" => json!({ "command": arg }),
        _ => json!({}),
    };

    Some(Route::Tool {
        name: tool.to_string(),
        args,
    })
}

/// Is this a request for machine specs?
fn is_system_info(lower: &str) -> bool {
    if SYSTEM_INFO_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // A bare "os" token, but not "osx" inside another word.
    words(lower).iter().any(|w| *w == "os")
}

/// Remove path-like tokens so they cannot trip the system-info phrases
/// (a path such as `/etc/hostname` contains the word "hostname").
fn strip_paths(lower: &str) -> String {
    lower
        .split_whitespace()
        .filter(|t| {
            let t = t.trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '`' | ',' | ';' | '?' | '!' | ')' | '(' | '[' | ']'
                )
            });
            !(t.starts_with('/')
                || t.starts_with("~/")
                || t.starts_with("./")
                || t.starts_with("../"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command following a verb such as `run`.
fn command_after(raw: &str, verbs: &[&str]) -> Option<String> {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    for (i, w) in parts.iter().enumerate() {
        let lw = clean_word(w);
        if verbs.contains(&lw.as_str()) {
            let rest = parts[i + 1..].join(" ");
            let rest = rest
                .trim()
                .trim_end_matches(['.', '!', '?', ';'])
                .trim()
                .to_string();
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

/// The application following a verb such as `open`.
fn app_after(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    for (i, w) in parts.iter().enumerate() {
        let lw = clean_word(w);
        if lw == "open" || lw == "launch" || lw == "start" {
            let mut app: Vec<String> = Vec::new();
            for t in &parts[i + 1..] {
                let lt = clean_word(t);
                if lt.is_empty() {
                    continue;
                }
                if APP_ARTICLES.contains(&lt.as_str()) {
                    // A determiner before the name is fine: "open the firefox".
                    if !app.is_empty() {
                        break;
                    }
                    continue;
                }
                if APP_STOPWORDS.contains(&lt.as_str()) {
                    // A function word straight after the verb means this is
                    // not an app launch at all ("open at all", "open the file").
                    if app.is_empty() {
                        return None;
                    }
                    break;
                }
                app.push(lt);
                if app.len() >= 2 {
                    break;
                }
            }
            if !app.is_empty() {
                return Some(app.join(" "));
            }
        }
    }
    None
}

/// The first path-like token in the request.
fn first_path(raw: &str, lower: &str) -> Option<String> {
    for token in raw.split_whitespace() {
        let t = token.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | ',' | ';' | '?' | '!' | ')' | '(' | '[' | ']'
            )
        });
        let t = t.trim_end_matches('.');
        if (t.starts_with('/')
            || t.starts_with("~/")
            || t.starts_with("./")
            || t.starts_with("../"))
            && (t.len() > 1 || t == "/")
        {
            return Some(t.to_string());
        }
    }
    // "my home directory" -> ~
    if lower.contains("home directory") || lower.contains("home folder") {
        return Some("~".to_string());
    }
    None
}

/// Should this path be listed or read?
fn path_tool(path: &str, lower: &str) -> &'static str {
    let expanded = expand_tilde(path);
    if is_directory(&expanded) {
        return "filesystem.list_dir";
    }
    if Path::new(&expanded).exists() {
        return "filesystem.read";
    }
    // Unknown path: fall back on the wording.
    if mentions_reading(lower) && !mentions_listing(lower) {
        "filesystem.read"
    } else {
        "filesystem.list_dir"
    }
}

/// Does the wording suggest listing a directory?
fn mentions_listing(lower: &str) -> bool {
    words(lower)
        .iter()
        .any(|w| LISTING_WORDS.contains(&w.as_str()))
        || lower.contains("what files")
        || lower.contains("show me the files")
}

/// Does the wording suggest reading a file?
fn mentions_reading(lower: &str) -> bool {
    words(lower)
        .iter()
        .any(|w| READING_WORDS.contains(&w.as_str()))
        || lower.contains("what's in")
        || lower.contains("whats in")
        || lower.contains("show me the file")
}

/// Does the wording mention files at all?
fn mentions_files(lower: &str) -> bool {
    lower.contains("file") || lower.contains("folder") || lower.contains("director")
}

/// Does the request look like it wants something done on the machine?
fn has_action_word(lower: &str) -> bool {
    words(lower)
        .iter()
        .any(|w| ACTION_WORDS.contains(&w.as_str()))
}

/// Split into lowercase word tokens.
fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(clean_word)
        .filter(|w| !w.is_empty())
        .collect()
}

/// Lowercase a token and strip surrounding punctuation.
fn clean_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Expand a leading `~`.
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir().to_string_lossy().to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest).to_string_lossy().to_string();
    }
    path.to_string()
}

/// The user's home directory.
fn home_dir() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Is this an existing directory?
fn is_directory(path: &str) -> bool {
    Path::new(path).is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_of(r: &Route) -> Option<(&str, Value)> {
        match r {
            Route::Tool { name, args } => Some((name.as_str(), args.clone())),
            _ => None,
        }
    }

    #[test]
    fn identity_is_direct() {
        assert_eq!(route("Who are you?"), Route::Direct);
        assert_eq!(route("what is 2+2"), Route::Direct);
        assert_eq!(route("tell me a joke"), Route::Direct);
        assert_eq!(route("hello there"), Route::Direct);
    }

    #[test]
    fn lists_a_directory() {
        let r = route("list contents of /tmp");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.list_dir");
        assert_eq!(args["path"], "/tmp");
    }

    #[test]
    fn reads_a_file() {
        let r = route("read /etc/hostname");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.read");
        assert_eq!(args["path"], "/etc/hostname");
    }

    #[test]
    fn reads_a_file_by_wording() {
        let r = route("what is inside /etc/hostname");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.read");
        assert_eq!(args["path"], "/etc/hostname");
    }

    #[test]
    fn runs_a_command() {
        let r = route("run echo test");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "shell.run");
        assert_eq!(args["command"], "echo test");
    }

    #[test]
    fn opens_an_application_in_the_background() {
        let r = route("Can you open Firefox browser for me please?");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "shell.run");
        assert_eq!(args["command"], "firefox");
        assert_eq!(args["background"], true);
    }

    #[test]
    fn opens_a_multi_word_application() {
        let r = route("Can you open opencode cli for me");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "shell.run");
        assert_eq!(args["command"], "opencode cli");
        assert_eq!(args["background"], true);
    }

    #[test]
    fn system_info_is_detected() {
        let r = route("what OS am I running on?");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "system.info");
        assert_eq!(args, json!({}));
    }

    #[test]
    fn slash_commands_work() {
        let r = route("/list /tmp");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.list_dir");
        assert_eq!(args["path"], "/tmp");

        let r = route("/read /etc/hostname");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.read");
        assert_eq!(args["path"], "/etc/hostname");

        let r = route("/run ls -la");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "shell.run");
        assert_eq!(args["command"], "ls -la");
    }

    #[test]
    fn unknown_action_falls_back_to_the_model() {
        assert_eq!(route("find all files containing TODO"), Route::Model);
    }

    #[test]
    fn open_with_a_path_reads_the_file() {
        let r = route("open /etc/hostname");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.read");
        assert_eq!(args["path"], "/etc/hostname");
    }

    #[test]
    fn a_trailing_function_word_is_not_an_app() {
        // "open at all" is not an app launch.
        assert_eq!(
            route("Are you lying, I dont see the firefox open at all?"),
            Route::Direct
        );
    }

    #[test]
    fn determiners_do_not_break_app_detection() {
        let r = route("please open the firefox browser now");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "shell.run");
        assert_eq!(args["command"], "firefox");
    }

    #[test]
    fn a_path_containing_a_system_word_is_not_system_info() {
        let r = route("read /etc/hostname");
        let (name, _) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.read");
    }

    #[test]
    fn listing_without_a_path_uses_the_current_directory() {
        let r = route("list the files here");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.list_dir");
        assert_eq!(args["path"], ".");
    }
}
