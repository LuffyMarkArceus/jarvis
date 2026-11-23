//! What an approval would change, for its card (Milestone 6.7b): built by
//! the daemon, from the same arguments the gate will run, so the card shows
//! what runs.

use crate::changes::{cut, unified};
use jarvis_common::ipc::Preview;
use serde_json::Value;

/// Lines a card's preview may run to.
const MAX_PREVIEW: usize = 400;
/// Lines of a deleted file shown.
const MAX_DELETED: usize = 200;

/// The preview for `tool` with `args`, if it has one.
pub fn preview(tool: &str, args: &Value) -> Option<Preview> {
    let path = || args.get("path").and_then(Value::as_str);
    let current = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let diff = |text: String| Preview {
        kind: "diff".into(),
        text: cut(&text, MAX_PREVIEW),
    };
    match tool {
        "filesystem.write" => {
            let path = path()?;
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            Some(diff(unified(&current(path), content, path).0))
        }
        "filesystem.edit" => {
            let path = path()?;
            let old = args.get("old_string").and_then(Value::as_str)?;
            let new = args.get("new_string").and_then(Value::as_str).unwrap_or("");
            let before = current(path);
            // As the tool does: every occurrence.
            Some(diff(unified(&before, &before.replace(old, new), path).0))
        }
        "filesystem.delete" => {
            let path = path()?;
            let removed: String = current(path).lines().map(|l| format!("-{l}\n")).collect();
            Some(diff(cut(
                &format!("--- {path}\n{removed}"),
                MAX_DELETED + 1,
            )))
        }
        "shell.run" => {
            let command = args.get("command").and_then(Value::as_str)?;
            let place = if args["outside_sandbox"].as_bool().unwrap_or(false) {
                "OUTSIDE the sandbox"
            } else {
                "in the sandbox"
            };
            Some(Preview {
                kind: "command".into(),
                text: format!("$ {command}\n({place})"),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn a_write_previews_the_diff_of_the_new_content() {
        let path = temp("notes.md", "one\ntwo\n");
        let p = preview(
            "filesystem.write",
            &json!({ "path": path, "content": "one\n2\n" }),
        )
        .unwrap();
        assert_eq!(p.kind, "diff");
        assert!(
            p.text.contains("-two") && p.text.contains("+2"),
            "{}",
            p.text
        );
        let new = path.with_file_name("new.md");
        let p = preview(
            "filesystem.write",
            &json!({ "path": new, "content": "hi\n" }),
        )
        .unwrap();
        assert!(
            p.text.contains("+hi"),
            "a new file diffs from nothing: {}",
            p.text
        );
    }

    #[test]
    fn an_edit_previews_every_replacement() {
        // The tool replaces every occurrence (str::replace): so must the card.
        let path = temp("notes.md", "cat\ndog\ncat\nbird\ncat\n");
        let p = preview(
            "filesystem.edit",
            &json!({ "path": path, "old_string": "cat", "new_string": "lion" }),
        )
        .unwrap();
        assert_eq!(
            p.text.lines().filter(|l| *l == "-cat").count(),
            3,
            "{}",
            p.text
        );
        assert_eq!(
            p.text.lines().filter(|l| *l == "+lion").count(),
            3,
            "{}",
            p.text
        );
    }

    #[test]
    fn a_delete_previews_removed_lines_cut_at_200() {
        let body: String = (0..250).map(|i| format!("line {i}\n")).collect();
        let path = temp("big.log", &body);
        let p = preview("filesystem.delete", &json!({ "path": path })).unwrap();
        assert!(p.text.contains("-line 0"));
        assert!(!p.text.contains("-line 249"));
        assert!(p.text.contains("… (50 more lines)"), "{}", p.text);
    }

    #[test]
    fn shell_previews_the_command_and_where() {
        let p = preview("shell.run", &json!({ "command": "git push" })).unwrap();
        assert_eq!(p.kind, "command");
        assert!(p.text.starts_with("$ git push"));
        assert!(p.text.contains("in the sandbox"), "{}", p.text);
        let out = preview(
            "shell.run",
            &json!({ "command": "git push", "outside_sandbox": true }),
        )
        .unwrap();
        assert!(out.text.contains("OUTSIDE the sandbox"), "{}", out.text);
    }

    #[test]
    fn other_tools_have_none() {
        assert!(preview("filesystem.read", &json!({ "path": "/x" })).is_none());
    }
}
