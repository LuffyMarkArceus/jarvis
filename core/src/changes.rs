//! The files a turn changed, for the window's row and diff view
//! (Milestone 6.7b): from the undo copies, one entry per file.

use crate::undo::UndoStore;
use chrono::{DateTime, Utc};
use jarvis_common::ipc::FileChange;

/// Text past this many bytes, before and after together, is not diffed.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

/// Lines a diff may run to before it is cut.
const MAX_DIFF_LINES: usize = 2000;

/// A unified diff of `before` against `after` (3 lines of context), and
/// the lines added and removed. Cut at 2000 lines, saying so.
pub fn unified(before: &str, after: &str, path: &str) -> (String, usize, usize) {
    // Bounded: a diff runs on the daemon, before a card or after a turn.
    if before.len() + after.len() > MAX_DIFF_BYTES {
        let kb = (before.len() + after.len()) / 1024;
        return (format!("--- {path}\n(too large to diff: {kb} KB)"), 0, 0);
    }
    let diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_millis(500))
        .diff_lines(before, after);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    (cut(&text, MAX_DIFF_LINES), added, removed)
}

/// `text`, cut at `max` lines with a note of how many more there were.
pub fn cut(text: &str, max: usize) -> String {
    let total = text.lines().count();
    if total <= max {
        return text.to_string();
    }
    let mut out: Vec<&str> = text.lines().take(max).collect();
    let note = format!("… ({} more lines)", total - max);
    out.push(&note);
    out.join("\n")
}

/// The files conversation `session` changed since `since`, one entry per
/// file: its undo ids newest first, diffed from its oldest copy this turn
/// to the file now.
pub fn turn_changes(store: &UndoStore, session: &str, since: DateTime<Utc>) -> Vec<FileChange> {
    let Ok(mut all) = store.list() else {
        return Vec::new();
    };
    all.retain(|c| c.session == session && c.time >= since && c.tool != "undo");
    all.sort_by_key(|c| c.time);
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for c in &all {
        if !paths.contains(&c.path) {
            paths.push(c.path.clone());
        }
    }
    paths
        .into_iter()
        .map(|path| {
            let of_path: Vec<_> = all.iter().filter(|c| c.path == path).collect();
            let oldest = of_path[0];
            let newest = of_path[of_path.len() - 1];
            let shown = path.display().to_string();
            let before = if oldest.existed_before {
                if oldest.kept {
                    store.before(&oldest.id).ok().flatten()
                } else {
                    None
                }
            } else {
                Some(Vec::new())
            };
            let after = std::fs::read(&path).unwrap_or_default();
            let (diff, added, removed) = match before {
                None => (None, 0, 0),
                Some(before) => match (String::from_utf8(before), String::from_utf8(after)) {
                    (Ok(before), Ok(after)) => {
                        let (diff, added, removed) = unified(&before, &after, &shown);
                        (Some(diff), added, removed)
                    }
                    _ => (Some("binary file".to_string()), 0, 0),
                },
            };
            FileChange {
                undo_ids: of_path.iter().rev().map(|c| c.id.clone()).collect(),
                path: shown,
                tool: newest.tool.clone(),
                added,
                removed,
                diff,
            }
        })
        .collect()
}
