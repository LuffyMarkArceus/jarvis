//! A copy of each file before Jarvis changes it, so the change can be
//! reversed (docs/design/injection-and-undo.md, section 4).
//!
//! One directory per change: `<id>/before` is the old content (absent when
//! the file did not exist or was too large to copy) and `<id>/meta.json` is
//! written last, so a directory without it is a change that never finished.
//! Undo restores only a file that still is what Jarvis wrote; anything else
//! would silently throw away the owner's own edits.

pub use crate::config::UndoConfig;
use crate::error::{JarvisError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The fingerprint of a file that is not there.
const ABSENT: &str = "absent";

/// A directory without `meta.json` younger than this may be a change
/// another process (the daemon, while the CLI cleans up) is still writing.
const HALF_WRITTEN_GRACE: chrono::Duration = chrono::Duration::hours(1);

/// One file change Jarvis made, as kept on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub id: String,
    pub time: DateTime<Utc>,
    /// The file written: links resolved, so undo restores the file a link
    /// points to and leaves the link alone.
    pub path: PathBuf,
    /// `filesystem.write`, `filesystem.edit`, or `undo` for an undo, which
    /// can itself be undone.
    pub tool: String,
    pub session: String,
    pub existed_before: bool,
    /// Whether a copy was kept; a file over the size limit has none.
    pub kept: bool,
    /// Size of the file before the change.
    pub size: u64,
    pub before_hash: Option<String>,
    /// What Jarvis left: undo restores only a file that still matches.
    pub after_hash: String,
    #[serde(default)]
    pub undone: bool,
}

/// A change that has started: the copy is taken, the write has not
/// finished. `after_change` completes it, `discard` drops it.
#[derive(Debug)]
pub struct Pending {
    dir: PathBuf,
    id: String,
    time: DateTime<Utc>,
    path: PathBuf,
    tool: String,
    session: String,
    existed_before: bool,
    kept: bool,
    size: u64,
    before_hash: Option<String>,
}

/// What undoing a change would do, decided before anything is touched so
/// the owner can see it first.
#[derive(Debug, Clone, PartialEq)]
pub enum UndoPlan {
    /// Put the old content back.
    Restore(Change),
    /// The file did not exist before: remove it.
    Remove(Change),
    /// Undoing would do harm or cannot be done; `reason` says which.
    Refuse { change: Change, reason: String },
}

pub struct UndoStore {
    dir: PathBuf,
    limits: UndoConfig,
    now: fn() -> DateTime<Utc>,
}

impl UndoStore {
    /// Open (or create) the store; the folder is private to its owner.
    pub fn open(dir: PathBuf, limits: &UndoConfig) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(UndoStore {
            dir,
            limits: limits.clone(),
            now: Utc::now,
        })
    }

    /// Use another clock: tests age entries without waiting a week.
    pub fn with_clock(mut self, now: fn() -> DateTime<Utc>) -> Self {
        self.now = now;
        self
    }

    /// Would a change to `path` keep a copy? A missing file needs none.
    pub fn would_keep(&self, path: &Path) -> bool {
        std::fs::metadata(path).map_or(true, |m| m.len() <= self.limits.max_file_bytes)
    }

    /// Take the copy, before the file is written.
    pub fn before_change(&self, path: &Path, tool: &str, session: &str) -> Result<Pending> {
        let path = resolve(path);
        let metadata = std::fs::metadata(&path).ok();
        let existed_before = metadata.is_some();
        let size = metadata.map_or(0, |m| m.len());
        let kept = size <= self.limits.max_file_bytes;

        let time = (self.now)();
        let id = format!(
            "{}-{}",
            time.format("%Y%m%dT%H%M%S%.9fZ"),
            &uuid::Uuid::new_v4().simple().to_string()[..4]
        );
        let dir = self.dir.join(&id);
        std::fs::create_dir(&dir)?;
        let before_hash = if existed_before {
            if kept {
                std::fs::copy(&path, dir.join("before"))?;
            }
            Some(fingerprint(&path)?)
        } else {
            None
        };
        Ok(Pending {
            dir,
            id,
            time,
            path,
            tool: tool.to_string(),
            session: session.to_string(),
            existed_before,
            kept,
            size,
            before_hash,
        })
    }

    /// The write succeeded: record what Jarvis left, then clean up.
    pub fn after_change(&self, pending: Pending) -> Result<Change> {
        let change = Change {
            after_hash: fingerprint(&pending.path)?,
            id: pending.id,
            time: pending.time,
            path: pending.path,
            tool: pending.tool,
            session: pending.session,
            existed_before: pending.existed_before,
            kept: pending.kept,
            size: pending.size,
            before_hash: pending.before_hash,
            undone: false,
        };
        // Last: an entry without it never finished.
        write_meta(&pending.dir, &change)?;
        self.clean_up();
        Ok(change)
    }

    /// The write failed: there is nothing to undo.
    pub fn discard(&self, pending: Pending) {
        let _ = std::fs::remove_dir_all(&pending.dir);
    }

    /// Every finished change, newest first.
    pub fn list(&self) -> Result<Vec<Change>> {
        let mut changes = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let meta = entry?.path().join("meta.json");
            if let Ok(text) = std::fs::read_to_string(&meta) {
                if let Ok(change) = serde_json::from_str::<Change>(&text) {
                    changes.push(change);
                }
            }
        }
        changes.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(changes)
    }

    /// What undoing `id` (or, without one, the latest change that is not
    /// itself an undo - so repeating it goes further back) would do.
    pub fn plan(&self, id: Option<&str>, force: bool) -> Result<UndoPlan> {
        let changes = self.list()?;
        let change = match id {
            Some(id) => changes.iter().find(|c| c.id == id || c.id.starts_with(id)),
            None => changes.iter().find(|c| !c.undone && c.tool != "undo"),
        }
        .cloned()
        .ok_or_else(|| {
            JarvisError::Tool(match id {
                Some(id) => format!("no change with id {}", id),
                None => "no change to undo".to_string(),
            })
        })?;

        let refuse = |reason: String| UndoPlan::Refuse {
            change: change.clone(),
            reason,
        };
        if change.undone {
            return Ok(refuse(format!("{} is already undone", change.id)));
        }
        if !change.kept {
            return Ok(refuse(format!(
                "no copy kept ({} bytes, over the size limit): this change cannot be undone",
                change.size
            )));
        }
        if !force && fingerprint(&change.path)? != change.after_hash {
            let later = changes
                .iter()
                .filter(|c| c.path == change.path && c.id > change.id && !c.undone)
                .min_by(|a, b| a.id.cmp(&b.id));
            return Ok(refuse(match later {
                Some(later) => format!("undo the later change first ({})", later.id),
                None => format!(
                    "{} changed after Jarvis wrote it; undoing would discard those changes (--force to undo anyway)",
                    change.path.display()
                ),
            }));
        }
        Ok(if change.existed_before {
            UndoPlan::Restore(change)
        } else {
            UndoPlan::Remove(change)
        })
    }

    /// Carry out a plan. The file as it is now is kept first, as a change
    /// of its own, so a mistaken undo is reversed the same way.
    pub fn apply(&self, plan: &UndoPlan) -> Result<()> {
        let change = match plan {
            UndoPlan::Restore(c) | UndoPlan::Remove(c) => c,
            UndoPlan::Refuse { reason, .. } => return Err(JarvisError::Tool(reason.clone())),
        };
        let pending = self.before_change(&change.path, "undo", &change.session)?;
        let done = match plan {
            UndoPlan::Restore(_) => std::fs::read(self.dir.join(&change.id).join("before"))
                .map_err(JarvisError::from)
                .and_then(|old| replace(&change.path, &old)),
            _ => std::fs::remove_file(&change.path).map_err(JarvisError::from),
        };
        if let Err(e) = done {
            self.discard(pending);
            return Err(e);
        }
        let mut undone = change.clone();
        undone.undone = true;
        write_meta(&self.dir.join(&change.id), &undone)?;
        self.after_change(pending)?;
        Ok(())
    }

    /// Drop half-written entries once they are old enough not to be in
    /// progress, entries past the age limit, then the oldest until the
    /// folder is under its size limit. The newest change always stays.
    fn clean_up(&self) {
        let now = (self.now)();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let max_age = chrono::Duration::days(self.limits.max_age_days as i64);
        let mut kept: Vec<(String, u64)> = Vec::new();
        for entry in entries.flatten() {
            let dir = entry.path();
            let id = entry.file_name().to_string_lossy().to_string();
            let Some(time) = time_of(&id) else { continue };
            let finished = dir.join("meta.json").exists();
            let expired = if finished {
                now - time > max_age
            } else {
                now - time > HALF_WRITTEN_GRACE
            };
            if expired {
                let _ = std::fs::remove_dir_all(&dir);
            } else if finished {
                let size = std::fs::metadata(dir.join("before")).map_or(0, |m| m.len());
                kept.push((id, size));
            }
        }
        kept.sort();
        let mut total: u64 = kept.iter().map(|(_, size)| size).sum();
        for (id, size) in kept.iter().take(kept.len().saturating_sub(1)) {
            if total <= self.limits.max_total_bytes {
                break;
            }
            let _ = std::fs::remove_dir_all(self.dir.join(id));
            total -= size;
        }
    }
}

/// The file a path names: links resolved, and a file that does not exist
/// yet resolved through its folder.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|p| p.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

fn fingerprint(path: &Path) -> Result<String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(format!("{:x}", Sha256::digest(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ABSENT.to_string()),
        Err(e) => Err(e.into()),
    }
}

/// `20261101T101530.123456789Z-ab12` -> its time.
fn time_of(id: &str) -> Option<DateTime<Utc>> {
    let stamp = id.split_once("Z-")?.0;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S%.f")
        .ok()
        .map(|t| t.and_utc())
}

/// Write beside the target, then rename over it: a crash leaves the old
/// file or the new one, never half of either. The file keeps its mode.
fn replace(path: &Path, content: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{}.jarvis-undo-tmp", name));
    std::fs::write(&temp, content)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, metadata.permissions());
    }
    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(e.into());
    }
    Ok(())
}

fn write_meta(dir: &Path, change: &Change) -> Result<()> {
    let temp = dir.join("meta.json.tmp");
    std::fs::write(
        &temp,
        serde_json::to_vec_pretty(change).map_err(|e| JarvisError::Tool(e.to_string()))?,
    )?;
    std::fs::rename(&temp, dir.join("meta.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Sandbox {
        root: PathBuf,
        store: UndoStore,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn sandbox_with(limits: UndoConfig) -> Sandbox {
        let root = std::env::temp_dir().join(format!("jarvis-undo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("work")).unwrap();
        let store = UndoStore::open(root.join("undo"), &limits).unwrap();
        Sandbox { root, store }
    }

    fn sandbox() -> Sandbox {
        sandbox_with(UndoConfig::default())
    }

    impl Sandbox {
        fn file(&self, name: &str) -> PathBuf {
            self.root.join("work").join(name)
        }

        /// What the orchestrator does around a write.
        fn change(&self, path: &Path, content: &[u8]) -> Change {
            let pending = self
                .store
                .before_change(path, "filesystem.write", "s1")
                .unwrap();
            std::fs::write(path, content).unwrap();
            self.store.after_change(pending).unwrap()
        }

        fn undo(&self, id: Option<&str>, force: bool) -> UndoPlan {
            let plan = self.store.plan(id, force).unwrap();
            if !matches!(plan, UndoPlan::Refuse { .. }) {
                self.store.apply(&plan).unwrap();
            }
            plan
        }
    }

    fn refusal(plan: &UndoPlan) -> &str {
        match plan {
            UndoPlan::Refuse { reason, .. } => reason,
            other => panic!("expected a refusal, got {:?}", other),
        }
    }

    #[test]
    fn restores_the_previous_content() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        s.change(&notes, b"jarvis wrote this");

        assert!(matches!(s.undo(None, false), UndoPlan::Restore(_)));
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "mine");
    }

    #[test]
    fn undoing_a_new_file_removes_it() {
        let s = sandbox();
        let new = s.file("new.md");
        let change = s.change(&new, b"hello");
        assert!(!change.existed_before);

        assert!(matches!(s.undo(None, false), UndoPlan::Remove(_)));
        assert!(!new.exists());
    }

    #[test]
    fn a_file_changed_since_is_refused() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        s.change(&notes, b"jarvis");
        std::fs::write(&notes, "jarvis, then my edit").unwrap();

        let plan = s.undo(None, false);
        assert!(
            refusal(&plan).contains("changed after Jarvis wrote it"),
            "{:?}",
            plan
        );
        assert_eq!(
            std::fs::read_to_string(&notes).unwrap(),
            "jarvis, then my edit"
        );
    }

    #[test]
    fn force_overrides_the_refusal() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        s.change(&notes, b"jarvis");
        std::fs::write(&notes, "my edit").unwrap();

        assert!(matches!(s.undo(None, true), UndoPlan::Restore(_)));
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "mine");
    }

    #[test]
    fn an_earlier_change_waits_for_the_later_one() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "v0").unwrap();
        let first = s.change(&notes, b"v1");
        let second = s.change(&notes, b"v2");

        let plan = s.undo(Some(&first.id), false);
        let reason = refusal(&plan);
        assert!(reason.contains("undo the later change first"), "{}", reason);
        assert!(reason.contains(&second.id), "{}", reason);
    }

    #[test]
    fn undo_twice_goes_further_back() {
        // Like an editor: the second undo reverses the change before, not
        // the first undo. Reversing an undo takes its id.
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "v0").unwrap();
        s.change(&notes, b"v1");
        s.change(&notes, b"v2");

        s.undo(None, false);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "v1");
        s.undo(None, false);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "v0");
    }

    #[test]
    fn an_undo_can_be_undone() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        s.change(&notes, b"jarvis");
        s.undo(None, false);

        let undo = s.store.list().unwrap().into_iter().next().unwrap();
        assert_eq!(undo.tool, "undo");
        s.undo(Some(&undo.id), false);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "jarvis");
    }

    #[test]
    fn an_undone_change_is_listed_as_undone_and_not_undone_twice() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        let change = s.change(&notes, b"jarvis");
        s.undo(None, false);

        let listed = s.store.list().unwrap();
        let entry = listed.iter().find(|c| c.id == change.id).unwrap();
        assert!(entry.undone);
        let plan = s.undo(Some(&change.id), false);
        assert!(refusal(&plan).contains("already undone"), "{:?}", plan);
    }

    #[test]
    fn a_large_file_is_listed_without_a_copy() {
        let s = sandbox_with(UndoConfig {
            max_file_bytes: 4,
            ..UndoConfig::default()
        });
        let big = s.file("big.log");
        std::fs::write(&big, "more than four bytes").unwrap();
        assert!(!s.store.would_keep(&big));
        assert!(s.store.would_keep(&s.file("missing.txt")));

        let change = s.change(&big, b"x");
        assert!(!change.kept);
        assert_eq!(change.size, 20);
        assert_eq!(
            s.store.list().unwrap().len(),
            1,
            "listed, so it never vanishes"
        );
        let plan = s.undo(None, false);
        assert!(refusal(&plan).contains("no copy kept"), "{:?}", plan);
    }

    #[test]
    fn a_failed_write_leaves_no_entry() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        let pending = s
            .store
            .before_change(&notes, "filesystem.write", "s1")
            .unwrap();
        s.store.discard(pending);

        assert!(s.store.list().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(s.root.join("undo")).unwrap().count(), 0);
    }

    fn eight_days_later() -> DateTime<Utc> {
        Utc::now() + chrono::Duration::days(8)
    }

    fn two_hours_later() -> DateTime<Utc> {
        Utc::now() + chrono::Duration::hours(2)
    }

    #[test]
    fn old_entries_expire() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "v0").unwrap();
        s.change(&notes, b"v1");

        let later = UndoStore::open(s.root.join("undo"), &UndoConfig::default())
            .unwrap()
            .with_clock(eight_days_later);
        let other = s.file("other.md");
        let pending = later
            .before_change(&other, "filesystem.write", "s1")
            .unwrap();
        std::fs::write(&other, "x").unwrap();
        later.after_change(pending).unwrap();

        let left = later.list().unwrap();
        assert_eq!(left.len(), 1, "{:?}", left);
        assert_eq!(left[0].path, other.canonicalize().unwrap());
    }

    #[test]
    fn the_total_cap_drops_the_oldest() {
        let s = sandbox_with(UndoConfig {
            max_total_bytes: 10,
            ..UndoConfig::default()
        });
        let mut ids = Vec::new();
        for name in ["a", "b", "c"] {
            let path = s.file(name);
            std::fs::write(&path, "123456").unwrap();
            ids.push(s.change(&path, b"x").id);
        }

        let left: Vec<String> = s.store.list().unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(
            left,
            [ids[2].clone()],
            "6 + 6 bytes is over 10: the oldest go"
        );
    }

    #[test]
    fn restore_goes_through_a_temporary_file() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        s.change(&notes, b"jarvis");
        let inode_before = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&notes).unwrap());
        s.undo(None, false);

        // Renamed into place: a new file, never a half-rewritten one.
        let inode_after = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&notes).unwrap());
        assert_ne!(inode_before, inode_after);
        let leftovers: Vec<_> = std::fs::read_dir(s.root.join("work"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{:?}", leftovers);
    }

    #[test]
    fn a_restored_file_keeps_its_mode() {
        let s = sandbox();
        let script = s.file("deploy.sh");
        std::fs::write(&script, "echo old").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        s.change(&script, b"echo new");
        s.undo(None, false);
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "still executable");
    }

    #[test]
    fn binary_content_round_trips() {
        let s = sandbox();
        let blob = s.file("blob.bin");
        std::fs::write(&blob, [0u8, 159, 146, 150]).unwrap();
        s.change(&blob, b"text now");
        s.undo(None, false);
        assert_eq!(std::fs::read(&blob).unwrap(), [0u8, 159, 146, 150]);
    }

    #[test]
    fn a_write_through_a_symlink_restores_the_target() {
        let s = sandbox();
        let target = s.file("real.md");
        let link = s.file("link.md");
        std::fs::write(&target, "mine").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        s.change(&link, b"jarvis");
        s.undo(None, false);

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
    }

    #[test]
    fn a_half_written_entry_is_skipped_and_cleaned_once_it_is_old() {
        let s = sandbox();
        let notes = s.file("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        // A copy without meta.json: a crash, or another process mid-write.
        let pending = s
            .store
            .before_change(&notes, "filesystem.write", "s1")
            .unwrap();
        let half = pending.dir.clone();
        std::mem::forget(pending);
        assert!(s.store.list().unwrap().is_empty());

        // A fresh one may still be in progress elsewhere: left alone.
        s.change(&s.file("a.md"), b"x");
        assert!(half.exists());

        let later = UndoStore::open(s.root.join("undo"), &UndoConfig::default())
            .unwrap()
            .with_clock(two_hours_later);
        let pending = later
            .before_change(&s.file("b.md"), "filesystem.write", "s1")
            .unwrap();
        std::fs::write(s.file("b.md"), "y").unwrap();
        later.after_change(pending).unwrap();
        assert!(!half.exists());
    }

    #[test]
    fn the_folder_is_private() {
        let s = sandbox();
        let mode = std::fs::metadata(s.root.join("undo"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
