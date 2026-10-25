//! Decides, once per turn, which tools the model is allowed to see.
//!
//! # Why this module exists
//!
//! Before this module there were two independent answers to "which tools are
//! available?":
//!
//! * [`crate::router`] returned a [`Route`] (deterministic tool, model, or
//!   plain conversation).
//! * `Orchestrator::model_with_tools` ran its own keyword filter to build the
//!   `tools` array for the provider request.
//!
//! They disagreed. Worse, the *system prompt* was built from the full
//! catalogue while the request carried only the filtered subset, so the model
//! was told about tools it could not call. On a 1B model that is the
//! difference between a usable assistant and one that keeps ordering items
//! off a menu it cannot have - and it is exactly the class of failure that
//! used to be "fixed" by adding another router rule.
//!
//! The invariant this module enforces is simple and testable:
//!
//! > The tools advertised in the prompt are the tools sent to the provider,
//! > and nothing else is executable in that turn.
//!
//! # How selection works
//!
//! The router answers *what kind of request is this*. The planner answers
//! *which capabilities can serve it*. A capability is a small, stable set
//! (files, shell, processes, system, web) rather than a list of websites, so
//! adding a new site or a new MCP server does not require a new branch.
//!
//! MCP servers are offered in three cases, in priority order:
//!
//! 1. the request names the server or one of its tools (`mcp.git.status`);
//! 2. the server declares `always_offer = true` in its config;
//! 3. the request needs the web and the server is a known web server.
//!
//! Case 3 is deliberately an explicit list rather than guesswork: a server
//! that ships fifteen filesystem tools must not be offered for "what is the
//! latest news", while a search server should be.

use crate::mcp::types::DiscoveredTool;
use crate::router::Route;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};

/// Servers whose tools answer questions about the outside world.
const WEB_SERVERS: &[&str] = &["fetch", "search", "web", "browser", "http", "crawl"];

/// File-ish nouns. Used because the bare verb "write" appears in "write a
/// poem", which must not offer the filesystem tools.
const FILE_NOUNS: &[&str] = &[
    "file",
    "files",
    "folder",
    "folders",
    "directory",
    "directories",
    "dir",
    "dirs",
    "path",
    "paths",
    "script",
    "scripts",
    "repo",
    "repository",
    "codebase",
    "source",
    "document",
    "documents",
    "log",
    "logs",
    "config",
    "configs",
    "readme",
    "csv",
    "json",
    "toml",
    "yaml",
    "yml",
];

/// Verbs that only mean "touch the filesystem" when a file noun or path is
/// also present.
const FILE_VERBS: &[&str] = &[
    "read", "write", "edit", "create", "delete", "remove", "move", "copy", "rename", "grep", "cat",
    "ls", "find", "search", "list",
];

/// Extensions that make a bare token look like a filename (`notes.md`).
const FILE_EXTENSIONS: &[&str] = &[
    "rs", "py", "md", "toml", "json", "yaml", "yml", "txt", "log", "sh", "js", "ts", "tsx", "jsx",
    "html", "css", "csv", "go", "c", "h", "cpp", "hpp", "java", "lock", "cfg", "ini", "conf",
    "xml", "sql",
];

/// A tool the model is allowed to call on this turn.
#[derive(Debug, Clone, PartialEq)]
pub struct OfferedTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl OfferedTool {
    /// The provider's function-schema shape.
    pub fn as_provider_schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.schema,
            }
        })
    }
}

/// The complete decision for one turn: the route, and the tools that route
/// permits.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    pub route: Route,
    pub tools: Vec<OfferedTool>,
    /// The request needs information from outside the model's weights.
    ///
    /// Exposed because more than tool selection depends on it: the
    /// orchestrator searches first on such a turn, and must know an answer
    /// that retrieved nothing is not a sourced answer.
    pub web: bool,
}

impl TurnPlan {
    /// Provider-ready schemas. This is the only list sent to the provider.
    pub fn schemas(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(OfferedTool::as_provider_schema)
            .collect()
    }

    pub fn allows(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }
}

/// The capabilities a request appears to need.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Intent {
    files: bool,
    shell: bool,
    processes: bool,
    system: bool,
    web: bool,
    /// The request explicitly named an MCP server or tool.
    mcp: bool,
}

/// Build the turn plan.
///
/// `route` comes from [`crate::router`]. This function never invents a
/// deterministic tool call; it only narrows what `Route::Model` may offer and
/// validates that the rest of the system agrees with the router.
pub fn plan(input: &str, route: Route, native: &ToolRegistry, mcp: &[DiscoveredTool]) -> TurnPlan {
    let intent = detect_intent(input);

    let tools = match &route {
        // Conversation uses no tools at all.
        Route::Direct => Vec::new(),

        // A deterministic call is the whole turn. Offer exactly that tool so
        // any prompt built from this plan stays truthful.
        Route::Tool { name, .. } => catalogue(native, mcp)
            .into_iter()
            .filter(|t| &t.name == name)
            .collect(),

        Route::Model => select_model_tools(&intent, input, native, mcp),
    };

    // A model turn with nothing to offer is just a conversation. This is the
    // important guard: the old fallback offered `system.info` for every
    // unmatched request, which taught the model to inspect the machine when
    // asked about the weather.
    if matches!(route, Route::Model) && tools.is_empty() {
        return TurnPlan {
            route: Route::Direct,
            tools: Vec::new(),
            web: intent.web,
        };
    }

    TurnPlan {
        route,
        tools,
        web: intent.web,
    }
}

/// Render the "Tools you can call" block for the system prompt.
///
/// Returns `None` when no tools are offered, so callers cannot accidentally
/// emit an empty menu. The returned text lists exactly the offered tools.
pub fn render_tool_prompt(offered: &[OfferedTool]) -> Option<String> {
    if offered.is_empty() {
        return None;
    }
    let mut out = String::from("Tools you can call:\n");
    for tool in offered {
        out.push_str(&format!("- {}: {}\n", tool.name, tool.description));
    }
    Some(out)
}

/// The full catalogue: native tools plus every discovered MCP tool.
fn catalogue(native: &ToolRegistry, mcp: &[DiscoveredTool]) -> Vec<OfferedTool> {
    let schemas = native.schemas();
    let mut out: Vec<OfferedTool> = native
        .describe()
        .into_iter()
        .map(|(name, description)| {
            let schema = schemas
                .iter()
                .find(|s| s["function"]["name"].as_str() == Some(name.as_str()))
                .map(|s| s["function"]["parameters"].clone())
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            OfferedTool {
                name,
                description,
                schema,
            }
        })
        .collect();

    out.extend(mcp.iter().map(|t| OfferedTool {
        // The name already carries the server (`mcp.fetch.fetch`). Prefixing
        // the description with it invites small models to echo the whole
        // definition instead of calling the tool, which wastes the
        // completion budget and produces no action.
        name: t.full_name.clone(),
        description: t.description.clone(),
        schema: t.schema.clone(),
    }));

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn select_model_tools(
    intent: &Intent,
    input: &str,
    native: &ToolRegistry,
    mcp: &[DiscoveredTool],
) -> Vec<OfferedTool> {
    let lower = input.to_lowercase();
    let mut selected: Vec<OfferedTool> = Vec::new();

    for tool in catalogue(native, mcp) {
        let keep = match tool.name.as_str() {
            "filesystem.list_dir"
            | "filesystem.read"
            | "filesystem.search"
            | "filesystem.stat"
            | "filesystem.write"
            | "filesystem.edit" => intent.files,
            "shell.run" => intent.shell,
            "process.list" => intent.processes,
            "system.info" => intent.system,
            name if name.starts_with("mcp.") => mcp_tool_wanted(&tool.name, &lower, intent, mcp),
            _ => false,
        };
        if keep {
            selected.push(tool);
        }
    }

    selected
}

fn mcp_tool_wanted(full_name: &str, lower: &str, intent: &Intent, mcp: &[DiscoveredTool]) -> bool {
    let Some(tool) = mcp.iter().find(|t| t.full_name == full_name) else {
        return false;
    };

    // 1. The request named the tool or its server.
    if lower.contains(&tool.full_name.to_lowercase())
        || has_word(lower, &tool.server.to_lowercase())
    {
        return true;
    }

    // 2. The server is configured as always-on.
    if tool.always_offer {
        return true;
    }

    // 3. The request needs the web and this is a web server.
    let server = tool.server.to_lowercase();
    if intent.web && WEB_SERVERS.contains(&server.as_str()) {
        return true;
    }

    // 4. An MCP-explicit request may use any server that is actually running.
    intent.mcp
}

fn detect_intent(input: &str) -> Intent {
    let lower = input.to_lowercase();
    // Remove URLs before looking for path separators, or every link would
    // also look like a filesystem path.
    let without_urls = strip_urls(&lower);
    let words = word_set(&without_urls);

    let has_file_noun = words.iter().any(|w| FILE_NOUNS.contains(&w.as_str()));
    let has_file_path = has_path(&without_urls) || looks_like_extension(&without_urls);
    let has_file_verb = words.iter().any(|w| FILE_VERBS.contains(&w.as_str()));

    Intent {
        files: has_file_noun || has_file_path || (has_file_verb && has_path(&without_urls)),
        shell: [
            "run", "execute", "exec", "command", "install", "launch", "kill", "sudo", "package",
            "service",
        ]
        .iter()
        .any(|w| words.contains(*w)),
        processes: ["process", "processes", "cpu", "ram", "memory", "top", "ps"]
            .iter()
            .any(|w| words.contains(*w)),
        system: [
            "system",
            "os",
            "hostname",
            "kernel",
            "distro",
            "arch",
            "architecture",
            "spec",
            "specs",
            "disk",
        ]
        .iter()
        .any(|w| words.contains(*w)),
        // The router owns the decision that a request needs the outside
        // world; the planner only maps that intent onto tools.
        web: crate::router::needs_web(input) || crate::router::needs_source(input),
        mcp: lower.contains("mcp.") || words.contains("mcp"),
    }
}

fn strip_urls(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| {
            if word.starts_with("http://") || word.starts_with("https://") {
                "<url>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_path(input: &str) -> bool {
    input.contains('/') || input.contains('~') || input.contains("\\")
}

fn looks_like_extension(input: &str) -> bool {
    input.split_whitespace().any(|word| {
        let trimmed =
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-');
        let Some((stem, ext)) = trimmed.rsplit_once('.') else {
            return false;
        };
        !stem.is_empty() && FILE_EXTENSIONS.contains(&ext.to_lowercase().as_str())
    })
}

fn has_word(lower: &str, needle: &str) -> bool {
    word_set(lower).contains(needle)
}

fn word_set(input: &str) -> std::collections::HashSet<String> {
    input
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(crate::tools::filesystem::ListDirTool));
        tools.register(Box::new(crate::tools::filesystem::ReadFileTool));
        tools.register(Box::new(crate::tools::filesystem::WriteFileTool));
        tools.register(Box::new(crate::tools::shell::RunShellTool));
        tools.register(Box::new(crate::tools::system::GetSystemInfoTool));
        tools
    }

    fn mcp_tool(server: &str, name: &str, always_offer: bool) -> DiscoveredTool {
        DiscoveredTool {
            server: server.to_string(),
            name: name.to_string(),
            full_name: format!("mcp.{}.{}", server, name),
            description: format!("{} tool", name),
            schema: json!({"type": "object", "properties": {}}),
            always_offer,
        }
    }

    fn fetch_tools() -> Vec<DiscoveredTool> {
        vec![mcp_tool("fetch", "fetch", false)]
    }

    #[test]
    fn direct_route_offers_nothing() {
        let plan = plan("hello there", Route::Direct, &native(), &[]);
        assert!(plan.tools.is_empty());
        assert_eq!(plan.route, Route::Direct);
    }

    #[test]
    fn web_intent_offers_only_the_web_server() {
        let plan = plan(
            "what is the latest news on the eu ai act",
            Route::Model,
            &native(),
            &fetch_tools(),
        );
        assert_eq!(plan.route, Route::Model);
        assert_eq!(plan.names(), vec!["mcp.fetch.fetch"]);
        // The orchestrator uses this to decide whether to search first, and
        // whether an answer that retrieved nothing is a sourced answer.
        assert!(plan.web);
    }

    #[test]
    fn a_non_web_model_turn_is_not_marked_web() {
        let plan = plan(
            "read the file notes.md and summarise it",
            Route::Model,
            &native(),
            &fetch_tools(),
        );
        assert!(!plan.web);
    }

    #[test]
    fn file_intent_offers_filesystem_but_not_shell() {
        let plan = plan(
            "read the file notes.md and summarise it",
            Route::Model,
            &native(),
            &fetch_tools(),
        );
        assert!(plan.allows("filesystem.read"));
        assert!(!plan.allows("shell.run"));
        assert!(!plan.allows("mcp.fetch.fetch"));
    }

    #[test]
    fn writing_a_poem_is_not_a_filesystem_request() {
        let plan = plan("write a poem about the sea", Route::Model, &native(), &[]);
        assert_eq!(plan.route, Route::Direct);
        assert!(plan.tools.is_empty());
    }

    #[test]
    fn mentioning_a_server_offers_its_tools() {
        let tools = vec![mcp_tool("git", "status", false)];
        let plan = plan(
            "what does git say about this repo",
            Route::Model,
            &native(),
            &tools,
        );
        assert!(plan.allows("mcp.git.status"));
    }

    #[test]
    fn always_offer_servers_are_always_present() {
        let tools = vec![mcp_tool("notes", "append", true)];
        let plan = plan("hello", Route::Model, &native(), &tools);
        assert_eq!(plan.route, Route::Model);
        assert!(plan.allows("mcp.notes.append"));
    }

    #[test]
    fn deterministic_route_offers_exactly_its_tool() {
        let route = Route::Tool {
            name: "system.info".to_string(),
            args: json!({}),
        };
        let plan = plan("system info", route, &native(), &fetch_tools());
        assert_eq!(plan.names(), vec!["system.info"]);
    }

    #[test]
    fn prompt_lists_exactly_the_offered_tools() {
        let plan = plan(
            "read the file notes.md",
            Route::Model,
            &native(),
            &fetch_tools(),
        );
        let prompt = render_tool_prompt(&plan.tools).expect("prompt");
        for tool in &plan.tools {
            assert!(prompt.contains(&tool.name), "missing {}", tool.name);
        }
        assert!(!prompt.contains("mcp.fetch.fetch"));
    }

    #[test]
    fn no_tools_renders_no_prompt_block() {
        assert!(render_tool_prompt(&[]).is_none());
    }

    #[test]
    fn urls_do_not_imply_filesystem_tools() {
        let intent = detect_intent("fetch https://example.com/docs/page");
        assert!(intent.web);
        assert!(!intent.files);
    }
}
