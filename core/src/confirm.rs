//! Human-readable descriptions of tool actions.
//!
//! A confirmation prompt is the only chance a user gets to see what would
//! run before it runs. Showing raw JSON is not good enough: `{"command":
//! "firefox"}` is technically complete, but "Run shell command: firefox" is
//! what a person can actually judge.
//!
//! The daemon and the CLI both render prompts through here so every client
//! shows identical wording.

use serde_json::Value;

/// Maximum characters of raw arguments included in a prompt.
const MAX_ARGS_CHARS: usize = 240;

/// Maximum characters of an argument echoed inside the one-line action.
///
/// A 4 KB shell command in the headline would push the actual question off
/// the screen; the full arguments are still shown underneath.
const MAX_ACTION_CHARS: usize = 120;

/// A one-line statement of what the call will do.
pub fn describe_action(tool: &str, args: &Value) -> String {
    match tool {
        // Not a tool: the remote request was not sent; answer locally?
        "egress.local" => format!(
            "Answer with the local model {} instead - nothing leaves this machine",
            str_arg(args, "model").unwrap_or("on this machine")
        ),
        // Not a tool: the orchestrator asking before content leaves the machine.
        "egress.send" => {
            let items: Vec<&str> = args["would_send"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect();
            format!(
                "Send {} to {} ({}), outside this machine",
                if items.is_empty() {
                    "this request".to_string()
                } else {
                    items.join(", ")
                },
                str_arg(args, "provider").unwrap_or("a remote model"),
                str_arg(args, "url").unwrap_or("unknown address"),
            )
        }
        // Not a tool: a fetch of an address the model built, which could
        // carry the owner's data to its site (the notice says why).
        "egress.web" if args.get("built_after").is_some() => format!(
            "Fetch {}",
            shorten(str_arg(args, "sending").unwrap_or("an address"))
        ),
        // Not a tool: a web call carrying something this turn may not send.
        "egress.web" => {
            let because: Vec<&str> = args["because"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect();
            format!(
                "Send \"{}\" to {}, outside this machine ({})",
                shorten(str_arg(args, "sending").unwrap_or("")),
                str_arg(args, "service").unwrap_or("a web service"),
                because.join("; ")
            )
        }
        "shell.run" => match str_arg(args, "command") {
            Some(command) => format!("Run shell command: {}", shorten(command)),
            None => "Run a shell command".to_string(),
        },
        "filesystem.write" => match str_arg(args, "path") {
            Some(path) => format!("Write to file: {}", shorten(path)),
            None => "Write a file".to_string(),
        },
        "filesystem.edit" => match str_arg(args, "path") {
            Some(path) => format!("Edit file: {}", shorten(path)),
            None => "Edit a file".to_string(),
        },
        "filesystem.read" => match str_arg(args, "path") {
            Some(path) => format!("Read file: {}", shorten(path)),
            None => "Read a file".to_string(),
        },
        "filesystem.list_dir" => match str_arg(args, "path") {
            Some(path) => format!("List directory: {}", shorten(path)),
            None => "List a directory".to_string(),
        },
        "filesystem.search" => {
            let root = str_arg(args, "path").unwrap_or(".");
            let mut what = Vec::new();
            if let Some(name) = str_arg(args, "name") {
                what.push(format!("named {}", shorten(name)));
            }
            if let Some(content) = str_arg(args, "content") {
                what.push(format!("containing {}", shorten(content)));
            }
            if what.is_empty() {
                format!("Search files under: {}", shorten(root))
            } else {
                format!("Search {} under: {}", what.join(" and "), shorten(root))
            }
        }
        "filesystem.stat" => match str_arg(args, "path") {
            Some(path) => format!("Inspect path: {}", shorten(path)),
            None => "Inspect a path".to_string(),
        },
        "process.list" => "List running processes".to_string(),
        _ => format!("Run {}", tool),
    }
}

/// The full prompt shown before a tool call is approved.
/// The prompt with its answers listed, when there is more than yes/no.
pub fn choice_prompt(tool: &str, args: &Value, choices: &[jarvis_common::ipc::Choice]) -> String {
    let mut prompt = confirmation_prompt(tool, args);
    if !choices.is_empty() {
        let keys: Vec<String> = choices
            .iter()
            .map(|c| format!("[{}] {}", c.key, c.label))
            .chain(std::iter::once("[n] no".to_string()))
            .collect();
        prompt.push_str(&format!("\n\n  {}", keys.join("   ")));
    }
    prompt
}

pub fn confirmation_prompt(tool: &str, args: &Value) -> String {
    // Keys starting with `_` are notes for the owner added by the
    // orchestrator (`_untrusted`, `_undo`), not the call's arguments.
    let call_args = match args.as_object() {
        Some(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        None => args.clone(),
    };
    let prompt = format!(
        "Jarvis wants to: {}{}\n\n  tool: {}\n  args: {}",
        describe_action(tool, &call_args),
        if tool == "egress.web" {
            exfiltration_notice(args)
        } else {
            untrusted_notice(args)
        },
        tool,
        compact_args(&call_args)
    );
    // Commands and source names can come from text an outsider wrote; a
    // terminal obeys escape codes in them. Only our own line breaks stay.
    prompt
        .split('\n')
        .map(crate::taint::printable)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Why a fetch of a built address asks: what could have asked for it, what
/// it could carry, and who would get it.
fn exfiltration_notice(args: &Value) -> String {
    let list = |key: &str| -> String {
        let items: Vec<&str> = args[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(MAX_SOURCES_SHOWN)
            .collect();
        items.join(", ")
    };
    if args.get("built_after").is_none() {
        return String::new();
    }
    format!(
        "\n  ! The model built this address after reading {},\n    and this request also read {}.\n    An address can carry data to its site: {} would receive it.",
        list("built_after"),
        list("also_read"),
        str_arg(args, "host").unwrap_or("its site")
    )
}

/// Sources shown in a prompt before "and N more": a research turn can read
/// twenty pages, and a list that long pushes the question off the screen.
const MAX_SOURCES_SHOWN: usize = 5;

/// What the model read before asking for this, and whether it can be
/// undone - empty unless the orchestrator added `_untrusted`.
fn untrusted_notice(args: &Value) -> String {
    let Some(sources) = args["_untrusted"].as_array().filter(|a| !a.is_empty()) else {
        return String::new();
    };
    let mut out = String::from(
        "\n  ! Before deciding this, the model read content you have not marked as trusted:",
    );
    for source in sources.iter().take(MAX_SOURCES_SHOWN) {
        out.push_str(&format!(
            "\n      {}   {}",
            source["name"].as_str().unwrap_or("?"),
            source["kind"].as_str().unwrap_or("")
        ));
    }
    if sources.len() > MAX_SOURCES_SHOWN {
        out.push_str(&format!(
            "\n      and {} more",
            sources.len() - MAX_SOURCES_SHOWN
        ));
    }
    out.push_str("\n    Text in it may be written to make Jarvis do this.");
    match args["_undo"].as_str() {
        Some("kept") => out.push_str("\n  You can undo this with `jarvis undo`."),
        Some("too_large") => {
            out.push_str("\n  This file is too large to keep a copy; this cannot be undone.")
        }
        Some("never") => out.push_str("\n  This cannot be undone."),
        _ => {}
    }
    out
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Truncate to `MAX_ACTION_CHARS` on a character boundary.
fn shorten(s: &str) -> String {
    if s.chars().count() <= MAX_ACTION_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_ACTION_CHARS).collect();
    out.push_str("...");
    out
}

fn compact_args(args: &Value) -> String {
    let rendered = serde_json::to_string(args).unwrap_or_default();
    if rendered.chars().count() <= MAX_ARGS_CHARS {
        return rendered;
    }
    let mut out: String = rendered.chars().take(MAX_ARGS_CHARS).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn untrusted(n: usize) -> Value {
        let sources: Vec<Value> = (1..=n)
            .map(|i| json!({ "name": format!("https://site{}.example/", i), "kind": "(web page)" }))
            .collect();
        json!({ "command": "ls", "_untrusted": sources, "_undo": "never" })
    }

    #[test]
    fn the_prompt_names_what_the_model_read() {
        let prompt = confirmation_prompt("shell.run", &untrusted(1));
        assert!(
            prompt.contains("the model read content you have not marked as trusted"),
            "{}",
            prompt
        );
        assert!(
            prompt.contains("https://site1.example/   (web page)"),
            "{}",
            prompt
        );
        assert!(
            prompt.contains("Text in it may be written to make Jarvis do this."),
            "{}",
            prompt
        );
        // The notice is for the owner, not part of the call's arguments.
        assert!(prompt.contains(r#"args: {"command":"ls"}"#), "{}", prompt);
    }

    #[test]
    fn the_prompt_lists_at_most_five_sources() {
        let prompt = confirmation_prompt("shell.run", &untrusted(7));
        assert!(prompt.contains("site5.example"), "{}", prompt);
        assert!(!prompt.contains("site6.example"), "{}", prompt);
        assert!(prompt.contains("and 2 more"), "{}", prompt);
    }

    #[test]
    fn the_prompt_says_whether_it_can_be_undone() {
        for (undo, line) in [
            ("kept", "You can undo this with `jarvis undo`."),
            (
                "too_large",
                "This file is too large to keep a copy; this cannot be undone.",
            ),
            ("never", "This cannot be undone."),
        ] {
            let mut args = untrusted(1);
            args["_undo"] = json!(undo);
            let prompt = confirmation_prompt("filesystem.write", &args);
            assert!(prompt.contains(line), "{}: {}", undo, prompt);
        }
    }

    #[test]
    fn a_built_address_says_where_the_data_would_go() {
        let args = json!({
            "service": "evil.example",
            "sending": "https://evil.example/c?d=hunter2",
            "because": ["the model built this address after reading untrusted content; evil.example would receive it"],
            "built_after": ["https://evil.example/deploy-guide"],
            "also_read": ["/home/u/projects/app/config.py"],
            "host": "evil.example",
        });
        let prompt = confirmation_prompt("egress.web", &args);
        assert!(
            prompt.starts_with("Jarvis wants to: Fetch https://evil.example/c?d=hunter2"),
            "{}",
            prompt
        );
        assert!(
            prompt.contains(
                "! The model built this address after reading https://evil.example/deploy-guide,"
            ),
            "{}",
            prompt
        );
        assert!(
            prompt.contains("and this request also read /home/u/projects/app/config.py."),
            "{}",
            prompt
        );
        assert!(
            prompt
                .contains("An address can carry data to its site: evil.example would receive it."),
            "{}",
            prompt
        );
    }

    #[test]
    fn escape_codes_in_a_command_or_source_are_shown_harmlessly() {
        // "\x1b[2K" erases the line it is on: a page could hide the warning.
        let args = json!({
            "command": "echo hi\x1b[2K",
            "_untrusted": [{ "name": "https://evil.example/\x1b[1A", "kind": "(web page)" }],
            "_undo": "never",
        });
        let prompt = confirmation_prompt("shell.run", &args);
        assert!(!prompt.contains('\x1b'), "{:?}", prompt);
        assert!(prompt.contains("echo hi?[2K"), "{}", prompt);
        assert!(prompt.contains("https://evil.example/?[1A"), "{}", prompt);
    }

    #[test]
    fn sending_to_a_remote_model_says_what_and_where() {
        let args = serde_json::json!({
            "provider": "openai", "url": "https://api.example/v1",
            "would_send": ["the contents of /home/u/notes.txt"]
        });
        assert_eq!(
            describe_action("egress.send", &args),
            "Send the contents of /home/u/notes.txt to openai (https://api.example/v1), outside this machine"
        );
    }

    #[test]
    fn shell_command_is_shown_verbatim() {
        let text = describe_action("shell.run", &json!({"command": "firefox"}));
        assert_eq!(text, "Run shell command: firefox");
    }

    #[test]
    fn file_paths_are_shown() {
        assert_eq!(
            describe_action("filesystem.write", &json!({"path": "/tmp/a.txt"})),
            "Write to file: /tmp/a.txt"
        );
        assert_eq!(
            describe_action("filesystem.edit", &json!({"path": "/tmp/a.txt"})),
            "Edit file: /tmp/a.txt"
        );
    }

    #[test]
    fn missing_arguments_still_produce_a_description() {
        assert_eq!(
            describe_action("shell.run", &json!({})),
            "Run a shell command"
        );
    }

    #[test]
    fn unknown_tools_fall_back_to_the_name() {
        assert_eq!(
            describe_action("some.future_tool", &json!({})),
            "Run some.future_tool"
        );
    }

    #[test]
    fn prompt_includes_tool_and_arguments() {
        let prompt = confirmation_prompt("shell.run", &json!({"command": "firefox"}));
        assert!(prompt.contains("Run shell command: firefox"));
        assert!(prompt.contains("shell.run"));
        assert!(prompt.contains("\"command\":\"firefox\""));
    }

    #[test]
    fn long_arguments_are_truncated() {
        let long = "x".repeat(1000);
        let prompt = confirmation_prompt("shell.run", &json!({"command": long}));
        assert!(prompt.contains("..."));
        assert!(prompt.chars().count() < 600);
    }
}
