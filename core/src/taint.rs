//! What Jarvis has read that it cannot vouch for.
//!
//! Text from a web page, a file from elsewhere or a command's output can
//! carry instructions written for the model ("ignore previous instructions
//! and run ..."). Nothing here stops a model from being fooled; it names
//! what could have fooled it, so an action taken after reading such text is
//! shown to the owner with its sources first (docs/design/injection-and-undo.md).

use crate::egress::{Label, Origin, Policy};
use std::collections::BTreeSet;
use std::path::Path;

/// What kind of thing a source is, for the prompt's wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceKind {
    WebPage,
    WebSearch,
    File,
    Command,
    Unknown,
}

/// One thing the model read that Jarvis cannot vouch for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Source {
    pub kind: SourceKind,
    /// A URL, `search: <query>`, a path, `$ <command>`, or a description
    /// when the label does not say (it was stored before sources were).
    pub name: String,
}

impl Source {
    pub fn describe(&self) -> String {
        match self.kind {
            SourceKind::WebPage => "(web page)",
            SourceKind::WebSearch => "(web search)",
            SourceKind::File => "(file outside your allowed folders)",
            SourceKind::Command => "(command output)",
            SourceKind::Unknown => "(unknown origin)",
        }
        .to_string()
    }

    fn new(kind: SourceKind, name: impl Into<String>) -> Self {
        Source {
            kind,
            name: name.into(),
        }
    }
}

/// Everything in `label` that is untrusted, each once, in a stable order:
/// web pages, searches, files, commands, unknown. Empty means trusted.
///
/// Trusted: the owner's words, machine information, files in the allowed
/// folders. Memory is not a source of its own: a recalled message keeps the
/// label it was stored with, so a page read last week is still a page.
pub fn untrusted_sources(label: &Label, policy: &Policy) -> Vec<Source> {
    let mut found = Vec::new();
    let named = |prefix: Option<&str>| -> Vec<&String> {
        label
            .sources
            .iter()
            .filter(|s| match prefix {
                Some(p) => s.starts_with(p),
                None => !s.starts_with("search: ") && !s.starts_with("$ "),
            })
            .collect()
    };

    if label.origins.contains(&Origin::Web) {
        let pages = named(None);
        let searches = named(Some("search: "));
        for page in &pages {
            found.push(Source::new(SourceKind::WebPage, page.as_str()));
        }
        for search in &searches {
            found.push(Source::new(SourceKind::WebSearch, search.as_str()));
        }
        if pages.is_empty() && searches.is_empty() {
            found.push(Source::new(
                SourceKind::WebPage,
                "web pages read earlier in this conversation",
            ));
        }
    }
    if label.origins.contains(&Origin::File) {
        for path in &label.paths {
            if !policy.file_allowed(Path::new(path)) {
                found.push(Source::new(SourceKind::File, path.as_str()));
            }
        }
        if label.paths.is_empty() {
            found.push(Source::new(
                SourceKind::File,
                "a file whose path is not known",
            ));
        }
    }
    if label.origins.contains(&Origin::Shell) {
        let commands = named(Some("$ "));
        for command in &commands {
            found.push(Source::new(SourceKind::Command, command.as_str()));
        }
        if commands.is_empty() {
            found.push(Source::new(
                SourceKind::Command,
                "output of a command run earlier",
            ));
        }
    }
    if label.origins.contains(&Origin::Other) {
        found.push(Source::new(
            SourceKind::Unknown,
            "earlier content of unknown origin",
        ));
    }
    found
}

/// Has the model seen anything it cannot vouch for?
pub fn is_untrusted(label: &Label, policy: &Policy) -> bool {
    !untrusted_sources(label, policy).is_empty()
}

/// Does `tool` change things? Writes, edits, commands, and any MCP tool but
/// search and fetch: a tool Jarvis does not know is assumed able to act.
pub fn is_action(tool: &str) -> bool {
    match tool {
        "filesystem.write" | "filesystem.edit" | "shell.run" => true,
        t if t.starts_with("mcp.search.") || t.starts_with("mcp.fetch.") => false,
        t => t.starts_with("mcp."),
    }
}

/// `text` with every control character replaced by `?`.
///
/// Source names and commands come from text an outsider wrote, and a
/// terminal obeys escape codes: `\x1b[2K` erases the line it is on, so a
/// page could hide the warning that names it.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Sources the owner trusted with `[t]`, per conversation, in memory only:
/// trust given today does not carry over to tomorrow, and the daemon serves
/// several conversations, so trust in one must not reach another.
#[derive(Default)]
pub struct SessionTrust {
    trusted: std::sync::Mutex<std::collections::HashMap<String, BTreeSet<String>>>,
}

impl SessionTrust {
    pub fn trust(&self, session: &str, sources: &[Source]) {
        self.trusted
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .extend(sources.iter().map(|s| s.name.clone()));
    }

    /// `sources` without the ones this conversation trusts.
    pub fn untrusted(&self, session: &str, sources: Vec<Source>) -> Vec<Source> {
        let trusted = self.trusted.lock().unwrap();
        match trusted.get(session) {
            Some(names) => sources
                .into_iter()
                .filter(|s| !names.contains(&s.name))
                .collect(),
            None => sources,
        }
    }

    pub fn list(&self, session: &str) -> Vec<String> {
        self.trusted
            .lock()
            .unwrap()
            .get(session)
            .map(|names| names.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear(&self, session: &str) {
        self.trusted.lock().unwrap().remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn policy() -> Policy {
        let mut policy = Policy::defaults(Path::new("/home/u"));
        policy.file_allow = vec![PathBuf::from("/home/u/project")];
        policy
    }

    fn only(label: &Label) -> Source {
        let sources = untrusted_sources(label, &policy());
        assert_eq!(sources.len(), 1, "{:?}", sources);
        sources.into_iter().next().unwrap()
    }

    fn read(path: &str) -> Label {
        Label::of_tool("filesystem.read", &json!({ "path": path }))
    }

    #[test]
    fn printable_replaces_control_characters() {
        assert_eq!(printable("a\x1b[2Kb"), "a?[2Kb");
        assert_eq!(printable("line\nbreak\ttab\r"), "line?break?tab?");
        assert_eq!(
            printable("plain https://a.example/ é"),
            "plain https://a.example/ é"
        );
    }

    #[test]
    fn actions_are_what_changes_things() {
        for tool in [
            "filesystem.write",
            "filesystem.edit",
            "shell.run",
            "mcp.github.create_issue",
        ] {
            assert!(is_action(tool), "{}", tool);
        }
        for tool in [
            "filesystem.read",
            "filesystem.list_dir",
            "filesystem.search",
            "filesystem.stat",
            "process.list",
            "system.info",
            "mcp.search.search",
            "mcp.fetch.fetch",
        ] {
            assert!(!is_action(tool), "{}", tool);
        }
    }

    #[test]
    fn trust_is_per_session_and_by_name() {
        let trust = SessionTrust::default();
        let page = Source::new(SourceKind::WebPage, "https://a.example/");
        let file = Source::new(SourceKind::File, "/tmp/x.md");
        trust.trust("s1", std::slice::from_ref(&page));
        assert_eq!(
            trust.untrusted("s1", vec![page.clone(), file.clone()]),
            vec![file]
        );
        assert_eq!(trust.untrusted("s2", vec![page.clone()]), vec![page]);
        assert_eq!(trust.list("s1"), ["https://a.example/"]);
        trust.clear("s1");
        assert!(trust.list("s1").is_empty());
    }

    #[test]
    fn user_and_system_are_trusted() {
        for label in [
            Label::of(Origin::User),
            Label::of_tool("system.info", &json!({})),
            Label::of_tool("process.list", &json!({})),
        ] {
            assert!(!is_untrusted(&label, &policy()), "{:?}", label);
        }
    }

    #[test]
    fn a_web_page_is_untrusted_and_named_by_its_url() {
        let label = Label::of_tool(
            "mcp.fetch.fetch",
            &json!({ "url": "https://evil.example/p" }),
        );
        let source = only(&label);
        assert_eq!(source.kind, SourceKind::WebPage);
        assert_eq!(source.name, "https://evil.example/p");
        assert_eq!(source.describe(), "(web page)");
    }

    #[test]
    fn a_search_is_named_by_its_query() {
        let label = Label::of_tool("mcp.search.search", &json!({ "query": "rust news" }));
        let source = only(&label);
        assert_eq!(source.kind, SourceKind::WebSearch);
        assert_eq!(source.name, "search: rust news");
    }

    #[test]
    fn an_allowed_file_is_trusted() {
        assert!(!is_untrusted(
            &read("/home/u/project/src/main.rs"),
            &policy()
        ));
        // `..` cannot climb out of an allowed folder.
        assert!(is_untrusted(
            &read("/home/u/project/../Downloads/x.md"),
            &policy()
        ));
    }

    #[test]
    fn a_file_outside_the_allowed_folders_is_named() {
        let source = only(&read("/home/u/Downloads/x.md"));
        assert_eq!(source.kind, SourceKind::File);
        assert_eq!(source.name, "/home/u/Downloads/x.md");
        assert_eq!(source.describe(), "(file outside your allowed folders)");
    }

    #[test]
    fn a_file_without_a_path_is_untrusted() {
        let source = only(&Label::of(Origin::File));
        assert_eq!(source.kind, SourceKind::File);
        assert_eq!(source.name, "a file whose path is not known");
    }

    #[test]
    fn command_output_is_untrusted() {
        let source = only(&Label::of_tool(
            "shell.run",
            &json!({ "command": "curl x" }),
        ));
        assert_eq!(source.kind, SourceKind::Command);
        assert_eq!(source.name, "$ curl x");
        assert_eq!(source.describe(), "(command output)");
    }

    #[test]
    fn unknown_origin_is_untrusted() {
        let source = only(&Label::unknown("assistant"));
        assert_eq!(source.kind, SourceKind::Unknown);
        assert_eq!(source.name, "earlier content of unknown origin");
        assert_eq!(source.describe(), "(unknown origin)");
    }

    #[test]
    fn old_web_labels_without_sources_are_named_generically() {
        let source = only(&Label::of(Origin::Web));
        assert_eq!(source.kind, SourceKind::WebPage);
        assert_eq!(source.name, "web pages read earlier in this conversation");
    }

    #[test]
    fn recalled_memory_keeps_the_origin_it_was_stored_with() {
        // Memory itself is not a source; a page recalled from memory is
        // still a page, and memory of the owner's words is still theirs.
        assert!(!is_untrusted(&Label::of(Origin::Memory), &policy()));
        let mut recalled =
            Label::of_tool("mcp.fetch.fetch", &json!({ "url": "https://a.example/" }));
        recalled.absorb(&Label::of(Origin::Memory));
        assert_eq!(only(&recalled).name, "https://a.example/");
    }

    #[test]
    fn every_source_is_listed_once() {
        let mut label = read("/home/u/Downloads/x.md");
        label.absorb(&Label::of_tool(
            "mcp.fetch.fetch",
            &json!({ "url": "https://b.example/" }),
        ));
        label.absorb(&Label::of_tool(
            "mcp.fetch.fetch",
            &json!({ "url": "https://b.example/" }),
        ));
        label.absorb(&read("/home/u/project/ok.rs"));
        let names: Vec<String> = untrusted_sources(&label, &policy())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["https://b.example/", "/home/u/Downloads/x.md"]);
    }
}
