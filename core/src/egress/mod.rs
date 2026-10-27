//! What may leave the machine, and in what form.
//!
//! Every message in a request is labelled with where its content came from.
//! A request to a remote provider is checked against the policy before it is
//! sent: allowed origins go (after redaction), file contents only from
//! allow-listed paths, and some paths never. See docs/design/model-tiers.md.

pub mod redact;

use crate::model::ChatMessage;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Where a piece of context came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    User,
    Memory,
    Web,
    File,
    Shell,
    System,
    /// A tool this module does not know, such as a new MCP server.
    Other,
}

pub fn tool_origin(tool: &str) -> Origin {
    match tool {
        t if t.starts_with("filesystem.") => Origin::File,
        "shell.run" => Origin::Shell,
        "process.list" | "system.info" => Origin::System,
        t if t.starts_with("mcp.search.") || t.starts_with("mcp.fetch.") => Origin::Web,
        _ => Origin::Other,
    }
}

/// The origins of one message's content, and the files it came from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Label {
    pub origins: BTreeSet<Origin>,
    pub paths: BTreeSet<String>,
}

impl Label {
    pub fn of(origin: Origin) -> Self {
        Label {
            origins: BTreeSet::from([origin]),
            paths: BTreeSet::new(),
        }
    }

    fn absorb(&mut self, other: &Label) {
        self.origins.extend(other.origins.iter().copied());
        self.paths.extend(other.paths.iter().cloned());
    }
}

/// Label every message of a request.
///
/// `stored` holds labels persisted with earlier messages, by index; `None`
/// means not known and the message is labelled from its role. A tool result
/// takes its origin from the tool and its path from the call that asked for
/// it. An assistant message takes every origin before it in the request:
/// after a file is read, the reply may quote it.
pub fn label(messages: &[ChatMessage], stored: &[Option<Label>]) -> Vec<Label> {
    let mut seen = Label::default();
    let mut calls: Vec<(String, Option<String>)> = Vec::new();
    let mut labels = Vec::with_capacity(messages.len());
    for (i, message) in messages.iter().enumerate() {
        let mut label = match stored.get(i).cloned().flatten() {
            Some(known) => known,
            None => match message.role.as_str() {
                "tool" => {
                    let name = message.tool_name.clone().unwrap_or_default();
                    let mut label = Label::of(tool_origin(&name));
                    // Results follow their calls in order.
                    if let Some(pos) = calls.iter().position(|(n, _)| *n == name) {
                        if let (_, Some(path)) = calls.remove(pos) {
                            label.paths.insert(path);
                        }
                    }
                    label
                }
                "user" => Label::of(Origin::User),
                // Instructions written by Jarvis carry nothing of the user's.
                "system" => Label::default(),
                _ => Label::default(),
            },
        };
        if message.role == "assistant" {
            label.absorb(&seen);
            for call in message.tool_calls.iter().flatten() {
                // Only a file tool's `path` names a file; another tool's
                // `path` argument may be anything.
                let name = &call.function.name;
                let path = (tool_origin(name) == Origin::File)
                    .then(|| call.function.arguments["path"].as_str())
                    .flatten()
                    .filter(|p| !p.is_empty())
                    .map(String::from);
                calls.push((name.clone(), path));
            }
        }
        seen.absorb(&label);
        labels.push(label);
    }
    labels
}

/// The rules for a remote provider. Paths are absolute, `~` already expanded.
#[derive(Debug, Clone)]
pub struct Policy {
    pub allow: BTreeSet<Origin>,
    /// Directories whose files may be sent.
    pub file_allow: Vec<PathBuf>,
    /// Directories never sent, whatever else allows them.
    pub never_dirs: Vec<PathBuf>,
    /// File names never sent wherever they are, such as `.env`.
    pub never_names: Vec<String>,
}

impl Policy {
    /// The owner's defaults: user text, memory and web content may go
    /// (redacted); files only from allow-listed directories.
    pub fn defaults(home: &Path) -> Self {
        Policy {
            allow: BTreeSet::from([Origin::User, Origin::Memory, Origin::Web]),
            file_allow: Vec::new(),
            never_dirs: [".ssh", ".gnupg", ".aws", ".config/jarvis"]
                .iter()
                .map(|d| home.join(d))
                .collect(),
            never_names: vec![".env".into()],
        }
    }

    fn never(&self, path: &Path) -> bool {
        let path = normalize(path);
        self.never_dirs
            .iter()
            .any(|d| path.starts_with(normalize(d)))
            || path
                .file_name()
                .is_some_and(|n| self.never_names.iter().any(|x| n == x.as_str()))
    }

    fn file_allowed(&self, path: &Path) -> bool {
        let path = normalize(path);
        self.file_allow
            .iter()
            .any(|d| path.starts_with(normalize(d)))
    }
}

/// `..` and `.` resolved without touching the disk, so `~/proj/../.ssh/id`
/// is judged as `~/.ssh/id`. Symlinks are not followed.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Something in a request the policy does not allow by itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Concern {
    pub origin: Origin,
    pub path: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Send,
    /// The user may approve these.
    Ask(Vec<Concern>),
    /// Nothing to approve: at least one item is never sent.
    Never(Vec<Concern>),
}

/// May a request with these labels go to a remote provider?
pub fn check(labels: &[Label], policy: &Policy) -> Verdict {
    let mut ask: Vec<Concern> = Vec::new();
    let mut never: Vec<Concern> = Vec::new();
    let note = |list: &mut Vec<Concern>, concern: Concern| {
        if !list.contains(&concern) {
            list.push(concern);
        }
    };
    for label in labels {
        for path in &label.paths {
            let concern = Concern {
                origin: Origin::File,
                path: Some(path.clone()),
            };
            if policy.never(Path::new(path)) {
                note(&mut never, concern);
            } else if !policy.file_allowed(Path::new(path)) {
                note(&mut ask, concern);
            }
        }
        for origin in &label.origins {
            // File content is judged by its paths above; file content with
            // no known path cannot be judged, so it is asked about.
            let pathless_file = *origin == Origin::File && label.paths.is_empty();
            if (*origin != Origin::File && !policy.allow.contains(origin)) || pathless_file {
                note(
                    &mut ask,
                    Concern {
                        origin: *origin,
                        path: None,
                    },
                );
            }
        }
    }
    if !never.is_empty() {
        Verdict::Never(never)
    } else if !ask.is_empty() {
        Verdict::Ask(ask)
    } else {
        Verdict::Send
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelFunction, ModelToolCall};
    use serde_json::json;

    fn call(tool: &str, path: &str) -> ChatMessage {
        ChatMessage::assistant_with_tool_calls(
            "",
            vec![ModelToolCall {
                id: None,
                function: ModelFunction {
                    index: None,
                    name: tool.into(),
                    arguments: json!({ "path": path }),
                },
            }],
        )
    }

    fn policy() -> Policy {
        let mut p = Policy::defaults(Path::new("/home/u"));
        p.file_allow = vec![PathBuf::from("/home/u/proj")];
        p
    }

    fn verdict(messages: &[ChatMessage]) -> Verdict {
        check(&label(messages, &[]), &policy())
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage::text("user", text)
    }

    #[test]
    fn plain_conversation_may_go() {
        let m = [user("hi"), ChatMessage::text("assistant", "hello")];
        assert_eq!(verdict(&m), Verdict::Send);
    }

    #[test]
    fn a_file_from_an_allowed_directory_may_go() {
        let m = [
            user("read it"),
            call("filesystem.read", "/home/u/proj/README.md"),
            ChatMessage::tool_result("filesystem.read", "# Jarvis"),
        ];
        assert_eq!(verdict(&m), Verdict::Send);
    }

    #[test]
    fn a_file_elsewhere_is_asked_about_with_its_path() {
        let m = [
            call("filesystem.read", "/home/u/notes/todo.txt"),
            ChatMessage::tool_result("filesystem.read", "buy milk"),
        ];
        assert_eq!(
            verdict(&m),
            Verdict::Ask(vec![Concern {
                origin: Origin::File,
                path: Some("/home/u/notes/todo.txt".into()),
            }])
        );
    }

    #[test]
    fn an_allowed_directory_does_not_allow_its_namesake() {
        let m = [
            call("filesystem.read", "/home/u/proj2/a"),
            ChatMessage::tool_result("filesystem.read", "x"),
        ];
        assert!(matches!(verdict(&m), Verdict::Ask(_)));
    }

    #[test]
    fn never_paths_cannot_be_approved_even_by_climbing_out_of_an_allowed_one() {
        for path in [
            "/home/u/.ssh/id_ed25519",
            "/home/u/proj/../.ssh/id_ed25519",
            "/home/u/proj/.env",
        ] {
            let m = [
                call("filesystem.read", path),
                ChatMessage::tool_result("filesystem.read", "secret"),
            ];
            assert!(matches!(verdict(&m), Verdict::Never(_)), "{}", path);
        }
    }

    #[test]
    fn shell_and_system_output_are_asked_about() {
        let m = [
            call("shell.run", ""),
            ChatMessage::tool_result("shell.run", "out"),
            ChatMessage::tool_result("system.info", "linux"),
        ];
        let Verdict::Ask(concerns) = verdict(&m) else {
            panic!("expected Ask")
        };
        let origins: Vec<Origin> = concerns.iter().map(|c| c.origin).collect();
        assert_eq!(origins, vec![Origin::Shell, Origin::System]);
    }

    #[test]
    fn an_assistant_reply_carries_what_it_saw() {
        let m = [
            call("filesystem.read", "/home/u/notes/todo.txt"),
            ChatMessage::tool_result("filesystem.read", "buy milk"),
            ChatMessage::text("assistant", "Your note says: buy milk"),
        ];
        let labels = label(&m, &[]);
        assert!(labels[2].origins.contains(&Origin::File));
        assert!(labels[2].paths.contains("/home/u/notes/todo.txt"));
    }

    #[test]
    fn a_stored_label_overrides_the_role() {
        // An assistant answer saved from an earlier turn that showed a file.
        let m = [ChatMessage::text(
            "assistant",
            "Contents of /home/u/notes/todo.txt: buy milk",
        )];
        let mut stored = Label::of(Origin::File);
        stored.paths.insert("/home/u/notes/todo.txt".into());
        let labels = label(&m, &[Some(stored)]);
        assert!(matches!(check(&labels, &policy()), Verdict::Ask(_)));
    }

    #[test]
    fn unknown_tools_are_asked_about() {
        let m = [ChatMessage::tool_result(
            "mcp.calendar.list",
            "dentist at 3",
        )];
        assert!(matches!(verdict(&m), Verdict::Ask(_)));
    }

    #[test]
    fn web_content_may_go() {
        let m = [
            ChatMessage::tool_result("mcp.search.search", "results"),
            ChatMessage::tool_result("mcp.fetch.fetch", "page"),
        ];
        assert_eq!(verdict(&m), Verdict::Send);
    }
}
