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
//! 1. Explicit slash commands (`/list /tmp`, `/find *.rs`, `/run <cmd>`)
//! 2. `run` / `execute` / `eval` followed by a command
//! 3. A search (`find files containing X`, `grep for X in <dir>`)
//! 4. A question about one path's size or metadata
//! 5. A path mentioned in the request -> list it or read it
//! 6. System-information requests
//! 7. Questions about running processes
//! 8. `open` / `launch` / `start` followed by an application
//! 9. "list the files" with no path -> the current directory
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
///
/// `find`, `search` and `grep` are deliberately absent: the search route
/// above already claims every phrasing they can honestly handle, and what
/// remains ("find me a joke") is conversation. Sending that to the model
/// with a tool catalogue is how a request for a joke ends up reading a
/// README.
const ACTION_WORDS: &[&str] = &[
    "check",
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
    "use",
    "invoke",
    "call",
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

    // 2b. Fetch/web requests - route to MCP fetch if available
    if let Some(url) = extract_url(raw) {
        if lower.contains("fetch")
            || lower.contains("get ")
            || lower.contains("download")
            || lower.starts_with("curl")
        {
            return Route::Tool {
                name: "mcp.fetch.fetch".to_string(),
                args: json!({ "url": url }),
            };
        }
    }

    // 2c. Factual queries that benefit from web search/fetch
    if lower.contains("millennium prize")
        || lower.contains("millenium prize")
        || (lower.contains("solved")
            && lower.contains("conjecture")
            && !lower.contains("can you solve"))
        || lower.contains("who won the prize")
        || lower.contains("nobel prize")
        || lower.contains("clay institute")
    {
        if lower.contains("millennium")
            || lower.contains("millenium")
            || lower.contains("clay institute")
        {
            return Route::Tool {
                name: "mcp.fetch.fetch".to_string(),
                args: json!({ "url": "https://en.wikipedia.org/wiki/Millennium_Prize_Problems" }),
            };
        }
    }

    // 3. A search. Checked before the path rule because "search for X in
    //    /some/dir" mentions a path that must become the search root, not the
    //    thing to list.
    if let Some(request) = parse_search(raw) {
        if looks_like_a_search(&lower, raw) {
            return Route::Tool {
                name: "filesystem.search".to_string(),
                args: request.to_args(),
            };
        }
    }

    // 4. A question about one path's size or metadata. Also before the path
    //    rule: "how big is /var/log" is not a request to list it.
    if let Some(r) = route_stat(raw, &lower) {
        return r;
    }

    // 5. A path was mentioned. This is checked before the system-info rules
    //    because a path such as /etc/hostname contains the word "hostname".
    if let Some(path) = first_path(raw, &lower) {
        return Route::Tool {
            name: path_tool(&path, &lower).to_string(),
            args: json!({ "path": path }),
        };
    }

    // 6. System information.
    if is_system_info(&strip_paths(&lower)) {
        return Route::Tool {
            name: "system.info".to_string(),
            args: json!({}),
        };
    }

    // 7. Questions about running processes and what is using the machine.
    if let Some(r) = route_processes(&lower) {
        return r;
    }

    // 8. An application was named.
    if let Some(app) = app_after(raw) {
        return Route::Tool {
            name: "shell.run".to_string(),
            args: json!({ "command": app, "background": true }),
        };
    }

    // 9. "list the files" with no path.
    if mentions_listing(&lower) && mentions_files(&lower) {
        return Route::Tool {
            name: "filesystem.list_dir".to_string(),
            args: json!({ "path": "." }),
        };
    }

    // 10. Looks like a machine action the router cannot pin down, or MCP usage.
    if lower.contains("mcp.") || has_action_word(&lower) {
        return Route::Model;
    }

    // 11. Conversation.
    Route::Direct
}

/// Handle `/list`, `/read`, `/run`, `/info`, `/ls`, `/cat`, `/find`, `/stat`.
fn route_command(rest: &str) -> Option<Route> {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let verb = parts.next()?.trim().to_lowercase();
    let arg = parts.next().unwrap_or("").trim().to_string();

    // `/find` and `/stat` need the whole remainder, not just one argument.
    match verb.as_str() {
        "find" | "search" | "grep" => {
            let full = rest.trim();
            let request = parse_search(full)?;
            return Some(Route::Tool {
                name: "filesystem.search".to_string(),
                args: request.to_args(),
            });
        }
        "stat" => {
            return Some(Route::Tool {
                name: "filesystem.stat".to_string(),
                args: json!({ "path": if arg.is_empty() { "." } else { &arg } }),
            });
        }
        _ => {}
    }

    let tool = match verb.as_str() {
        "list" | "ls" => "filesystem.list_dir",
        "read" | "cat" => "filesystem.read",
        "run" | "sh" | "shell" => "shell.run",
        "info" | "system" => "system.info",
        "processes" | "ps" | "top" => "process.list",
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

/// Whether the request to search looks like it is about files at all.
///
/// Without this, "find me a joke" would be routed into a filesystem search.
/// The verb alone is not enough evidence of intent.
fn looks_like_a_search(lower: &str, raw: &str) -> bool {
    mentions_files(lower)
        || lower.contains("contain")
        || lower.contains("grep")
        || lower.contains("inside")
        || lower.contains("search")
        || first_path(raw, lower).is_some()
        || raw
            .split_whitespace()
            .any(|t| t.contains('*') || t.contains('?'))
}

/// Whether a search should match file names or file contents.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SearchMode {
    Name,
    Content,
}

/// A parsed search request.
struct SearchRequest {
    query: String,
    root: String,
    mode: SearchMode,
}

impl SearchRequest {
    fn to_args(&self) -> Value {
        let mut args = json!({
            "path": expand_tilde(&self.root),
        });
        match self.mode {
            SearchMode::Name => args["name"] = json!(self.query),
            SearchMode::Content => args["content"] = json!(self.query),
        }
        args
    }
}

/// Words that carry no information in a search query.
const SEARCH_FILLER: &[&str] = &[
    "for", "the", "a", "an", "all", "any", "some", "me", "please", "my", "files", "file",
    "folders", "folder", "text", "string", "lines", "line", "matches", "match", "that", "which",
    "whose",
];

/// Parse `find files containing X in <dir>` and its relatives.
///
/// This is a deliberately small vocabulary, not a natural-language parser.
/// It handles the phrasings a person actually types for this task and
/// returns `None` for everything else, which sends the request down the
/// normal fallback path instead of guessing.
fn parse_search(raw: &str) -> Option<SearchRequest> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let (verb_index, verb) = tokens.iter().enumerate().find_map(|(i, t)| {
        let w = clean_word(t);
        if matches!(
            w.as_str(),
            "find" | "search" | "grep" | "locate" | "contain"
        ) {
            Some((i, w))
        } else {
            None
        }
    })?;

    let mut mode = match verb.as_str() {
        "grep" | "contain" => Some(SearchMode::Content),
        _ => None,
    };

    // Split at an "in"/"inside" that introduces the directory to search.
    let mut root: Option<String> = None;
    let mut query_end = tokens.len();
    for j in (verb_index + 1)..tokens.len() {
        let w = clean_word(tokens[j]);
        if (w == "in" || w == "inside") && j + 1 < tokens.len() {
            root = Some(tokens[j + 1..].join(" "));
            query_end = j;
            break;
        }
    }

    let mut query_tokens: Vec<String> = tokens[verb_index + 1..query_end]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // "find files in /tmp named *.rs": the qualifier follows the root.
    if let Some(root_text) = root.clone() {
        let root_parts: Vec<String> = root_text.split_whitespace().map(String::from).collect();
        if let Some(k) = root_parts.iter().position(|t| {
            matches!(
                clean_word(t).as_str(),
                "named" | "called" | "matching" | "containing" | "contains" | "with"
            )
        }) {
            mode = Some(match clean_word(&root_parts[k]).as_str() {
                "containing" | "contains" | "with" => SearchMode::Content,
                _ => SearchMode::Name,
            });
            query_tokens = root_parts[k + 1..].to_vec();
            root = Some(root_parts[..k].join(" "));
        }
    }

    // Strip filler and pick up qualifiers that sit before the root.
    let mut kept: Vec<String> = Vec::new();
    for token in &query_tokens {
        match clean_word(token).as_str() {
            "named" | "called" | "matching" => mode = Some(SearchMode::Name),
            "containing" | "contains" | "with" => mode = Some(SearchMode::Content),
            other if SEARCH_FILLER.contains(&other) => {}
            _ => kept.push(token.clone()),
        }
    }

    let query = kept
        .join(" ")
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim_end_matches(['.', '!', '?', ',', ';'])
        .trim()
        .to_string();
    if query.is_empty() {
        return None;
    }

    let root = root
        .map(|r| {
            r.trim()
                .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
                .to_string()
        })
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| ".".to_string());

    let mode = mode.unwrap_or_else(|| {
        if query.contains('*') || query.contains('?') || looks_like_a_filename(&query) {
            SearchMode::Name
        } else {
            // A bare word is far more likely to be text to find than a file
            // called "TODO".
            SearchMode::Content
        }
    });

    Some(SearchRequest { query, root, mode })
}

/// Does this token read like a file name rather than a word to search for?
fn looks_like_a_filename(query: &str) -> bool {
    if query.chars().any(char::is_whitespace) {
        return false;
    }
    match query.rfind('.') {
        None | Some(0) => false,
        Some(_) => true,
    }
}

/// Route "how big is /x", "size of /x", "stat /x".
fn route_stat(raw: &str, lower: &str) -> Option<Route> {
    let wants_metadata = lower.contains("how big")
        || lower.contains("how large")
        || lower.contains("size of")
        || lower.contains("file info")
        || lower.contains("file details")
        || lower.starts_with("stat ")
        || lower.contains("permissions of")
        || lower.contains("modified");
    if !wants_metadata {
        return None;
    }
    let path = first_path(raw, lower)?;
    Some(Route::Tool {
        name: "filesystem.stat".to_string(),
        args: json!({ "path": path }),
    })
}

/// Route "what processes are running" and "what is using my cpu".
fn route_processes(lower: &str) -> Option<Route> {
    let tokens = words(lower);
    // Word-level matching, not substring: "because" contains "use", and a
    // sentence that happens to include that must not become a process list.
    let consuming = tokens.iter().any(|w| {
        matches!(
            w.as_str(),
            "using" | "use" | "uses" | "consuming" | "hogging"
        )
    });
    let resource = tokens
        .iter()
        .any(|w| matches!(w.as_str(), "cpu" | "ram" | "memory" | "processor"));

    let wants = lower.contains("process")
        || (consuming && resource)
        || lower.contains("cpu usage")
        || lower.contains("memory usage")
        || lower.contains("ram usage");
    if !wants {
        return None;
    }

    // "top 5 processes" asks for a count. Anything else gets the default.
    let mut args = json!({});
    if lower.contains("top") || lower.contains("first") {
        if let Some(n) = tokens.iter().find_map(|w| w.parse::<u64>().ok()) {
            args["limit"] = json!(n.clamp(1, 100));
        }
    }

    Some(Route::Tool {
        name: "process.list".to_string(),
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

/// Extract a URL from the text
fn extract_url(raw: &str) -> Option<String> {
    for token in raw.split_whitespace() {
        let t = token.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | ',' | ';' | '?' | '!' | ')' | '(' | '[' | ']' | '>'
            )
        });
        if t.starts_with("http://") || t.starts_with("https://") {
            return Some(t.to_string());
        }
    }
    None
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
        assert_eq!(route("install docker"), Route::Model);
    }

    // ------------------------------------------------------------- searching

    #[test]
    fn finds_files_containing_text() {
        let r = route("find all files containing TODO");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["content"], "TODO");
        assert_eq!(args["path"], ".");
        assert!(args.get("name").is_none());
    }

    #[test]
    fn finds_files_by_name() {
        let r = route("find files named *.rs");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["name"], "*.rs");
        assert_eq!(args["path"], ".");
    }

    #[test]
    fn searches_a_specific_directory() {
        let r = route("search for router.rs in /mnt/storage/CS/jarvis");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["path"], "/mnt/storage/CS/jarvis");
        // A dotted token reads as a file name, not as text to find.
        assert_eq!(args["name"], "router.rs");
    }

    #[test]
    fn grep_searches_contents() {
        let r = route("grep for PermissionManager in core");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["content"], "PermissionManager");
        assert_eq!(args["path"], "core");
    }

    #[test]
    fn search_root_can_come_before_the_qualifier() {
        let r = route("find files in /tmp named *.log");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["path"], "/tmp");
        assert_eq!(args["name"], "*.log");
    }

    #[test]
    fn a_bare_word_is_searched_in_contents() {
        // "TODO" is far more likely text to find than a file called TODO.
        let r = route("search for TODO");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["content"], "TODO");
    }

    #[test]
    fn a_search_verb_alone_is_not_a_search() {
        // "find me a joke" has no file or text signal; it must not walk the
        // filesystem looking for the word "joke", and it is not a machine
        // action either - it is conversation.
        assert_eq!(route("find me a joke"), Route::Direct);
    }

    #[test]
    fn slash_find_works() {
        let r = route("/find *.toml in /mnt/storage/CS/jarvis");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.search");
        assert_eq!(args["path"], "/mnt/storage/CS/jarvis");
        assert_eq!(args["name"], "*.toml");
    }

    // ------------------------------------------------------------ file detail

    #[test]
    fn size_questions_use_stat() {
        let r = route("how big is /var/log");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.stat");
        assert_eq!(args["path"], "/var/log");
    }

    #[test]
    fn slash_stat_works() {
        let r = route("/stat /etc/hostname");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.stat");
        assert_eq!(args["path"], "/etc/hostname");
    }

    #[test]
    fn reading_is_not_mistaken_for_stat() {
        let r = route("read /etc/hostname");
        let (name, _) = tool_of(&r).unwrap();
        assert_eq!(name, "filesystem.read");
    }

    // --------------------------------------------------------------- processes

    #[test]
    fn process_questions_list_processes() {
        let r = route("what processes are running?");
        let (name, _) = tool_of(&r).unwrap();
        assert_eq!(name, "process.list");
    }

    #[test]
    fn cpu_questions_list_processes() {
        let r = route("what's using my CPU right now?");
        let (name, _) = tool_of(&r).unwrap();
        assert_eq!(name, "process.list");
    }

    #[test]
    fn a_process_count_is_passed_through() {
        let r = route("show me the top 5 processes");
        let (name, args) = tool_of(&r).unwrap();
        assert_eq!(name, "process.list");
        assert_eq!(args["limit"], 5);
    }

    #[test]
    fn memory_size_questions_are_not_process_questions() {
        // "how much memory does this machine have" is not "what is using
        // memory right now", and answering it with a process list would be
        // wrong.
        assert_eq!(
            route("how much memory does this machine have"),
            Route::Direct
        );
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
