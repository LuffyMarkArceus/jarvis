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
    let diff = |text: String| Preview {
        kind: "diff".into(),
        text: cut(&text, MAX_PREVIEW),
    };
    let note = |text: String| Preview {
        kind: "diff".into(),
        text,
    };
    match tool {
        "filesystem.write" => {
            let path = path()?;
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            Some(match current(path) {
                Now::Text(before) => diff(unified(&before, content, path).0),
                Now::Missing => diff(unified("", content, path).0),
                other => note(format!("--- {path}\n(overwrites {})", other.describe())),
            })
        }
        "filesystem.edit" => {
            let path = path()?;
            let old = args.get("old_string").and_then(Value::as_str)?;
            let new = args.get("new_string").and_then(Value::as_str).unwrap_or("");
            Some(match current(path) {
                // As the tool does: every occurrence.
                Now::Text(before) if before.contains(old) => {
                    diff(unified(&before, &before.replace(old, new), path).0)
                }
                Now::Text(_) => note(format!(
                    "--- {path}\n(the text to replace is not in the file)"
                )),
                other => note(format!("--- {path}\n(edits {})", other.describe())),
            })
        }
        "filesystem.delete" => {
            let path = path()?;
            Some(match current(path) {
                Now::Text(before) => {
                    let removed: String = before
                        .lines()
                        .take(MAX_DELETED + 1)
                        .map(|l| format!("-{l}\n"))
                        .collect();
                    let total = before.lines().count();
                    let mut text = format!("--- {path}\n{removed}");
                    if total > MAX_DELETED {
                        text = cut(&text, MAX_DELETED + 1);
                        text = text.replace(
                            &format!(
                                "… ({} more lines)",
                                (total + 1).min(MAX_DELETED + 2) - (MAX_DELETED + 1)
                            ),
                            &format!("… ({} more lines)", total - MAX_DELETED),
                        );
                    }
                    diff(text)
                }
                other => note(format!("--- {path}\n(deletes {})", other.describe())),
            })
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

/// Files past this size are not read for a preview.
const MAX_READ: u64 = 2 * 1024 * 1024;

/// What is at `path` now.
enum Now {
    Text(String),
    Missing,
    Binary(u64),
    Large(u64),
    Unreadable(String),
}

impl Now {
    fn describe(&self) -> String {
        match self {
            Now::Binary(n) => format!("a binary file of {n} bytes; no diff"),
            Now::Large(n) => format!("a large file of {} KB; no diff", n / 1024),
            Now::Unreadable(e) => format!("a file Jarvis cannot read now ({e}); no diff"),
            Now::Text(_) | Now::Missing => String::new(),
        }
    }
}

fn current(path: &str) -> Now {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Now::Missing,
        Err(e) => return Now::Unreadable(e.to_string()),
        Ok(m) if m.len() > MAX_READ => return Now::Large(m.len()),
        Ok(_) => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let n = bytes.len() as u64;
            String::from_utf8(bytes).map_or(Now::Binary(n), Now::Text)
        }
        Err(e) => Now::Unreadable(e.to_string()),
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
    fn a_huge_diff_returns_quickly() {
        // Review of 6.7b: 50k differing lines took 44 s, on a daemon thread.
        let before: String = (0..60_000).map(|i| format!("a{i}\n")).collect();
        let after: String = (0..60_000).map(|i| format!("b{i}\n")).collect();
        let path = temp("big.log", &before);
        let started = std::time::Instant::now();
        let p = preview(
            "filesystem.write",
            &json!({ "path": path, "content": after }),
        )
        .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(!p.text.is_empty());
    }

    #[test]
    fn overwriting_a_binary_file_says_so() {
        let dir = std::env::temp_dir().join(format!("jarvis-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("photo.jpg");
        std::fs::write(&bin, [0xFFu8, 0xD8, 0x00, 0x9C, 0xFE]).unwrap();
        let p = preview(
            "filesystem.write",
            &json!({ "path": bin, "content": "text" }),
        )
        .unwrap();
        assert!(p.text.contains("binary file"), "{}", p.text);
        let d = preview("filesystem.delete", &json!({ "path": bin })).unwrap();
        assert!(d.text.contains("binary file"), "{}", d.text);
    }

    #[test]
    fn an_unreadable_file_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp("locked.md", "secret\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let p = preview("filesystem.write", &json!({ "path": path, "content": "x" })).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(p.text.contains("cannot read"), "{}", p.text);
    }

    #[test]
    fn other_tools_have_none() {
        assert!(preview("filesystem.read", &json!({ "path": "/x" })).is_none());
    }
}
