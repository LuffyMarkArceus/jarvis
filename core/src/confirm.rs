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
        _ => format!("Run {}", tool),
    }
}

/// The full prompt shown before a tool call is approved.
pub fn confirmation_prompt(tool: &str, args: &Value) -> String {
    format!(
        "Jarvis wants to: {}\n\n  tool: {}\n  args: {}",
        describe_action(tool, args),
        tool,
        compact_args(args)
    )
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
