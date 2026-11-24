//! A turn's file changes (Milestone 6.7b): one entry per file, diffed from
//! its oldest undo copy this turn to the file now.

use chrono::Utc;
use jarvis_core::changes::turn_changes;
use jarvis_core::config::UndoConfig;
use jarvis_core::undo::UndoStore;
use std::path::{Path, PathBuf};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup() -> (Dir, UndoStore) {
    let root = std::env::temp_dir().join(format!("jarvis-changes-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("work")).unwrap();
    let store = UndoStore::open(root.join("undo"), &UndoConfig::default()).unwrap();
    (Dir(root), store)
}

/// What the orchestrator does around a write, in conversation `session`.
fn write(store: &UndoStore, path: &Path, content: &str, session: &str) -> String {
    let pending = store
        .before_change(path, "filesystem.write", session)
        .unwrap();
    std::fs::write(path, content).unwrap();
    store.after_change(pending).unwrap().id
}

#[test]
fn one_change_diffs_before_to_now() {
    let (dir, store) = setup();
    let notes = dir.0.join("work/notes.md");
    std::fs::write(&notes, "one\ntwo\nthree\n").unwrap();
    let since = Utc::now();
    let id = write(&store, &notes, "one\n2\nthree\n", "s1");
    let changes = turn_changes(&store, "s1", since);
    assert_eq!(changes.len(), 1);
    let c = &changes[0];
    assert_eq!(c.undo_ids, vec![id]);
    assert_eq!((c.added, c.removed), (1, 1));
    let diff = c.diff.as_deref().unwrap();
    assert!(diff.contains("-two") && diff.contains("+2"), "{diff}");
}

#[test]
fn a_file_changed_twice_is_one_entry_from_its_oldest_copy() {
    let (dir, store) = setup();
    let notes = dir.0.join("work/notes.md");
    std::fs::write(&notes, "a\n").unwrap();
    let since = Utc::now();
    let first = write(&store, &notes, "b\n", "s1");
    let second = write(&store, &notes, "c\n", "s1");
    let changes = turn_changes(&store, "s1", since);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].undo_ids, vec![second, first], "newest first");
    let diff = changes[0].diff.as_deref().unwrap();
    let lines: Vec<&str> = diff.lines().collect();
    assert!(lines.contains(&"-a") && lines.contains(&"+c"), "{diff}");
    assert!(
        !lines.contains(&"+b") && !lines.contains(&"-b"),
        "the middle state shows: {diff}"
    );
}

#[test]
fn a_new_file_diffs_from_nothing() {
    let (dir, store) = setup();
    let new = dir.0.join("work/new.md");
    let since = Utc::now();
    write(&store, &new, "hello\n", "s1");
    let changes = turn_changes(&store, "s1", since);
    assert_eq!((changes[0].added, changes[0].removed), (1, 0));
}

#[test]
fn a_deleted_file_diffs_to_nothing() {
    let (dir, store) = setup();
    let old = dir.0.join("work/old.md");
    std::fs::write(&old, "x\ny\n").unwrap();
    let since = Utc::now();
    let pending = store
        .before_change(&old, "filesystem.delete", "s1")
        .unwrap();
    std::fs::remove_file(&old).unwrap();
    store.after_change(pending).unwrap();
    let changes = turn_changes(&store, "s1", since);
    assert_eq!((changes[0].added, changes[0].removed), (0, 2));
    assert_eq!(changes[0].tool, "filesystem.delete");
}

#[test]
fn another_conversations_changes_are_not_listed() {
    let (dir, store) = setup();
    let since = Utc::now();
    write(&store, &dir.0.join("work/a.md"), "a\n", "other");
    assert!(turn_changes(&store, "s1", since).is_empty());
}

#[test]
fn changes_before_the_turn_are_not_listed() {
    let (dir, store) = setup();
    write(&store, &dir.0.join("work/a.md"), "a\n", "s1");
    let since = Utc::now();
    write(&store, &dir.0.join("work/b.md"), "b\n", "s1");
    let changes = turn_changes(&store, "s1", since);
    assert_eq!(changes.len(), 1);
    assert!(changes[0].path.ends_with("b.md"));
}

#[test]
fn a_change_with_no_copy_has_no_diff() {
    let root = std::env::temp_dir().join(format!("jarvis-changes-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("work")).unwrap();
    let _dir = Dir(root.clone());
    let store = UndoStore::open(
        root.join("undo"),
        &UndoConfig {
            max_file_bytes: 2,
            ..UndoConfig::default()
        },
    )
    .unwrap();
    let big = root.join("work/big.md");
    std::fs::write(&big, "far too big\n").unwrap();
    let since = Utc::now();
    write(&store, &big, "small\n", "s1");
    let changes = turn_changes(&store, "s1", since);
    assert_eq!(changes.len(), 1);
    assert!(changes[0].diff.is_none());
}
