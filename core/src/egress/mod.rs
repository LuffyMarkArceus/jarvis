//! What may leave the machine, and in what form.
//!
//! Every message in a request is labelled with where its content came from.
//! A request to a remote provider is checked against the policy before it is
//! sent: allowed origins go (after redaction), file contents only from
//! allow-listed paths, and some paths never. See docs/design/model-tiers.md.

pub mod redact;

use crate::config::ModelConfig;
use crate::error::{JarvisError, Result};
use crate::model::ChatMessage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Where a piece of context came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
/// Stored with each message in memory as JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub origins: BTreeSet<Origin>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub paths: BTreeSet<String>,
}

impl Label {
    /// The label of a message that was stored before labels existed. User
    /// text is the user's; anything else may contain anything, and unknown
    /// must not mean allowed.
    pub fn unknown(role: &str) -> Self {
        Label::of(if role == "user" {
            Origin::User
        } else {
            Origin::Other
        })
    }

    /// A tool's result: its origin, and the file it read if it is a file tool.
    pub fn of_tool(tool: &str, args: &serde_json::Value) -> Self {
        let origin = tool_origin(tool);
        let mut label = Label::of(origin);
        if origin == Origin::File {
            if let Some(path) = args["path"].as_str().filter(|p| !p.is_empty()) {
                label.paths.insert(path.to_string());
            }
        }
        label
    }

    pub fn is_empty(&self) -> bool {
        self.origins.is_empty() && self.paths.is_empty()
    }

    pub fn of(origin: Origin) -> Self {
        Label {
            origins: BTreeSet::from([origin]),
            paths: BTreeSet::new(),
        }
    }

    pub fn absorb(&mut self, other: &Label) {
        self.origins.extend(other.origins.iter().copied());
        self.paths.extend(other.paths.iter().cloned());
    }
}

/// Label every message of a request.
///
/// A message that carries a label (set where it was created, or stored with
/// it in memory) keeps it. Otherwise a tool result takes its origin from the
/// tool and its path from the call that asked for it. An assistant message
/// also takes every origin before it in the request: after a file is read,
/// the reply may quote it.
pub fn label(messages: &[ChatMessage]) -> Vec<Label> {
    let mut seen = Label::default();
    let mut calls: Vec<(String, Option<String>)> = Vec::new();
    let mut labels = Vec::with_capacity(messages.len());
    for message in messages {
        let mut label = match message.label.clone() {
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

    /// Is `path` somewhere nothing is ever sent from?
    pub fn is_never(&self, path: &Path) -> bool {
        self.never(path)
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

/// The folders the user allowed from a prompt or `jarvis egress allow`,
/// kept in Jarvis's data directory rather than in the hand-edited config,
/// whose comments a rewrite would destroy.
pub const ALLOW_FILE: &str = "egress-allow.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
struct AllowFile {
    #[serde(default)]
    file_allow: Vec<String>,
}

/// Folders in the allow file. A missing file is an empty list; an unreadable
/// one is too, which can only make Jarvis ask more, never send more.
pub fn allowed_folders(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<AllowFile>(&text).ok())
        .map(|f| f.file_allow)
        .unwrap_or_default()
}

/// Add `folder` to the allow file. Returns false if it was already there.
pub fn allow_folder(path: &Path, folder: &str) -> Result<bool> {
    let mut file = AllowFile {
        file_allow: allowed_folders(path),
    };
    if file.file_allow.iter().any(|f| f == folder) {
        return Ok(false);
    }
    file.file_allow.push(folder.to_string());
    write_allow_file(path, &file)?;
    Ok(true)
}

/// Remove `folder` from the allow file. Returns false if it was not there.
pub fn disallow_folder(path: &Path, folder: &str) -> Result<bool> {
    let mut file = AllowFile {
        file_allow: allowed_folders(path),
    };
    let before = file.file_allow.len();
    file.file_allow.retain(|f| f != folder);
    if file.file_allow.len() == before {
        return Ok(false);
    }
    write_allow_file(path, &file)?;
    Ok(true)
}

fn write_allow_file(path: &Path, file: &AllowFile) -> Result<()> {
    let text = toml::to_string(file).map_err(|e| JarvisError::Config(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// The folders to offer for "always allow": the parent of every file
/// concerned, but only if every concern is a file with a known path.
/// Approving "always" for command output or machine details would be a far
/// wider grant than one key suggests, so that is never offered.
pub fn folders_to_allow(concerns: &[Concern]) -> Vec<String> {
    let mut folders: Vec<String> = Vec::new();
    for concern in concerns {
        let (Origin::File, Some(path)) = (concern.origin, &concern.path) else {
            return Vec::new();
        };
        let Some(parent) = Path::new(path).parent() else {
            return Vec::new();
        };
        let parent = parent.display().to_string();
        if !folders.contains(&parent) {
            folders.push(parent);
        }
    }
    folders
}

/// Whether a request leaves the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    Local,
    Remote,
}

/// What a remote provider says it does with requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPolicy {
    NoTraining,
    MayTrain,
    /// Not declared. Treated as `MayTrain` wherever it matters.
    Unknown,
}

/// Where a turn's requests go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Destination {
    pub locality: Locality,
    pub data_policy: DataPolicy,
}

impl Destination {
    pub fn is_remote(&self) -> bool {
        self.locality == Locality::Remote
    }

    /// The notice shown in `jarvis status` and when the provider is chosen.
    /// Not repeated per turn, where it would stop being read.
    pub fn notice(&self) -> &'static str {
        match (self.locality, self.data_policy) {
            (Locality::Local, _) => "local: nothing leaves this machine",
            (Locality::Remote, DataPolicy::NoTraining) => {
                "remote: requests leave this machine; the provider declares it does not train on them"
            }
            (Locality::Remote, _) => {
                "remote: requests leave this machine, and this provider may log or train on what you send"
            }
        }
    }
}

/// Read and check the provider's locality and data policy. A misspelt value
/// is an error: guessing would decide where prompts go.
pub fn destination(model: &ModelConfig) -> Result<Destination> {
    let locality = match model.locality.as_deref() {
        Some("local") => Locality::Local,
        Some("remote") => Locality::Remote,
        Some(other) => {
            return Err(JarvisError::Config(format!(
                "model.locality '{other}' - expected 'local' or 'remote'"
            )))
        }
        None if is_loopback(&model.base_url) => Locality::Local,
        None => Locality::Remote,
    };
    let data_policy = match model.data_policy.as_deref() {
        Some("no_training") => DataPolicy::NoTraining,
        Some("may_train") => DataPolicy::MayTrain,
        Some("unknown") | None => DataPolicy::Unknown,
        Some(other) => {
            return Err(JarvisError::Config(format!(
                "model.data_policy '{other}' - expected 'no_training', 'may_train' or 'unknown'"
            )))
        }
    };
    Ok(Destination {
        locality,
        data_policy,
    })
}

/// Is the URL's host this machine? An unparsable URL is not.
fn is_loopback(base_url: &str) -> bool {
    let Some(host) = reqwest::Url::parse(base_url).ok().and_then(|u| {
        u.host_str()
            .map(|h| h.trim_matches(['[', ']']).to_lowercase())
    }) else {
        return false;
    };
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host == "localhost" || host.ends_with(".localhost"),
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
        check(&label(messages), &policy())
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
        let labels = label(&m);
        assert!(labels[2].origins.contains(&Origin::File));
        assert!(labels[2].paths.contains("/home/u/notes/todo.txt"));
    }

    #[test]
    fn a_stored_label_overrides_the_role() {
        // An assistant answer saved from an earlier turn that showed a file.
        let mut stored = Label::of(Origin::File);
        stored.paths.insert("/home/u/notes/todo.txt".into());
        let m = [
            ChatMessage::text("assistant", "Contents of /home/u/notes/todo.txt: buy milk")
                .with_label(stored),
        ];
        let labels = label(&m);
        assert!(matches!(check(&labels, &policy()), Verdict::Ask(_)));
    }

    #[test]
    fn a_tool_result_sent_as_user_text_keeps_its_origin() {
        // Text-embedded tool calls return results in a "user" message.
        let m = [
            ChatMessage::text("user", "Result of filesystem.read:\nbuy milk").with_label(
                Label::of_tool(
                    "filesystem.read",
                    &json!({"path": "/home/u/notes/todo.txt"}),
                ),
            ),
        ];
        assert!(matches!(verdict(&m), Verdict::Ask(_)));
    }

    #[test]
    fn a_message_stored_before_labels_existed_is_not_trusted() {
        let m = [
            ChatMessage::text("assistant", "old answer").with_label(Label::unknown("assistant"))
        ];
        assert!(matches!(verdict(&m), Verdict::Ask(_)));
        assert_eq!(Label::unknown("user"), Label::of(Origin::User));
    }

    #[test]
    fn the_label_never_reaches_a_provider() {
        let m = ChatMessage::text("user", "hi").with_label(Label::of(Origin::File));
        let wire = serde_json::to_string(&m).unwrap();
        assert!(
            !wire.contains("label") && !wire.contains("file"),
            "{}",
            wire
        );
        let openai = crate::openai::to_openai_messages(&[m]);
        assert!(!openai[0].to_string().contains("label"), "{}", openai[0]);
    }

    #[test]
    fn a_label_survives_storage_as_json() {
        let mut l = Label::of(Origin::File);
        l.paths.insert("/a".into());
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, r#"{"origins":["file"],"paths":["/a"]}"#);
        assert_eq!(serde_json::from_str::<Label>(&json).unwrap(), l);
    }

    fn model(base_url: &str, locality: Option<&str>, policy: Option<&str>) -> ModelConfig {
        ModelConfig {
            base_url: base_url.into(),
            locality: locality.map(String::from),
            data_policy: policy.map(String::from),
            ..ModelConfig::default()
        }
    }

    #[test]
    fn only_this_machine_is_local_unless_declared() {
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:8080/v1",
            "http://[::1]:11434",
        ] {
            assert_eq!(
                destination(&model(url, None, None)).unwrap().locality,
                Locality::Local,
                "{}",
                url
            );
        }
        for url in [
            "https://openrouter.ai/api/v1",
            "http://192.168.1.5:11434",
            "not a url",
        ] {
            assert!(
                destination(&model(url, None, None)).unwrap().is_remote(),
                "{}",
                url
            );
        }
        // Your own server on the home network, declared.
        let own = destination(&model("http://192.168.1.5:11434", Some("local"), None)).unwrap();
        assert_eq!(own.locality, Locality::Local);
    }

    #[test]
    fn an_undeclared_data_policy_reads_as_may_train() {
        let d = destination(&model("https://x.ai/v1", None, None)).unwrap();
        assert_eq!(d.data_policy, DataPolicy::Unknown);
        assert!(d.notice().contains("may log or train"));
        let d = destination(&model("https://x.ai/v1", None, Some("no_training"))).unwrap();
        assert!(!d.notice().contains("may log"));
    }

    #[test]
    fn a_misspelt_locality_or_policy_is_an_error() {
        assert!(destination(&model("http://127.0.0.1", Some("lcoal"), None)).is_err());
        assert!(destination(&model("http://127.0.0.1", None, Some("never"))).is_err());
    }

    #[test]
    fn the_allow_file_adds_and_removes_folders() {
        let dir = std::env::temp_dir().join(format!("jarvis-allow-{}", uuid::Uuid::new_v4()));
        let file = dir.join(ALLOW_FILE);
        assert!(allowed_folders(&file).is_empty());
        assert!(allow_folder(&file, "/home/u/notes").unwrap());
        assert!(
            !allow_folder(&file, "/home/u/notes").unwrap(),
            "no duplicates"
        );
        assert_eq!(allowed_folders(&file), vec!["/home/u/notes"]);
        assert!(disallow_folder(&file, "/home/u/notes").unwrap());
        assert!(allowed_folders(&file).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn always_is_offered_only_for_files() {
        let file = |p: &str| Concern {
            origin: Origin::File,
            path: Some(p.into()),
        };
        assert_eq!(
            folders_to_allow(&[file("/n/a.txt"), file("/n/b.txt"), file("/p/c")]),
            vec!["/n", "/p"]
        );
        let shell = Concern {
            origin: Origin::Shell,
            path: None,
        };
        assert!(folders_to_allow(&[file("/n/a.txt"), shell]).is_empty());
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
