//! The drives on this machine by the names the owner uses for them: "my DATA
//! drive" is the partition labelled DATA, mounted at /mnt/storage. Real use:
//! a model guessed /mnt/DATA, which does not exist.
//!
//! Read from `/dev/disk/by-label` (label -> device) and `/proc/mounts`
//! (device -> folder): plain files, cheap enough to read per prompt.

use std::path::{Path, PathBuf};

/// `(label, folder)` for every labelled drive that is mounted.
pub fn mounted() -> Vec<(String, String)> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    from(Path::new("/dev/disk/by-label"), &mounts)
}

/// [`mounted`], from a by-label folder and the text of a mounts table.
pub fn from(by_label: &Path, mounts: &str) -> Vec<(String, String)> {
    let Ok(read) = std::fs::read_dir(by_label) else {
        return Vec::new();
    };
    let mut drives = Vec::new();
    for entry in read.flatten() {
        let label = unescape_label(&entry.file_name().to_string_lossy());
        let Ok(device) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        let folder = mounts.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            let dev = PathBuf::from(unescape_mount(fields.next()?));
            let point = unescape_mount(fields.next()?);
            (std::fs::canonicalize(&dev).unwrap_or(dev) == device).then_some(point)
        });
        if let Some(folder) = folder {
            drives.push((label, folder));
        }
    }
    drives.sort();
    drives
}

/// The line the model is given, or nothing when no drive is labelled.
pub fn prompt_line(drives: &[(String, String)]) -> Option<String> {
    if drives.is_empty() {
        return None;
    }
    let list: Vec<String> = drives
        .iter()
        .map(|(label, folder)| format!("{label} is {folder}"))
        .collect();
    Some(format!(
        "- Drives on this machine by name: {}. When the user names a drive, use its folder.\n",
        list.join("; ")
    ))
}

/// udev writes "Local Disk" as `Local\x20Disk`.
fn unescape_label(name: &str) -> String {
    let mut out = String::new();
    let mut rest = name;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        let hex = rest.get(i + 2..i + 4).unwrap_or("");
        match u8::from_str_radix(hex, 16) {
            Ok(byte) => {
                out.push(byte as char);
                rest = &rest[i + 4..];
            }
            Err(_) => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The kernel writes a space in a mount path as `\040`.
fn unescape_mount(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_matched_to_where_they_are_mounted() {
        let root = std::env::temp_dir().join(format!("jarvis-drives-{}", uuid::Uuid::new_v4()));
        let (dev, labels) = (root.join("dev"), root.join("by-label"));
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&labels).unwrap();
        for d in ["sda4", "sda5", "sdb1"] {
            std::fs::write(dev.join(d), "").unwrap();
        }
        std::os::unix::fs::symlink(dev.join("sda5"), labels.join("DATA")).unwrap();
        std::os::unix::fs::symlink(dev.join("sda4"), labels.join("Local\\x20Disk")).unwrap();
        // Labelled but not mounted: not listed.
        std::os::unix::fs::symlink(dev.join("sdb1"), labels.join("Recovery")).unwrap();
        let mounts = format!(
            "{} /mnt/storage ntfs3 rw 0 0\n{} /run/media/shrey/Local\\040Disk ntfs3 rw 0 0\nproc /proc proc rw 0 0\n",
            dev.join("sda5").display(),
            dev.join("sda4").display()
        );
        let drives = from(&labels, &mounts);
        assert_eq!(
            drives,
            vec![
                ("DATA".to_string(), "/mnt/storage".to_string()),
                (
                    "Local Disk".to_string(),
                    "/run/media/shrey/Local Disk".to_string()
                ),
            ]
        );
        let line = prompt_line(&drives).unwrap();
        assert!(line.contains("DATA is /mnt/storage"), "{line}");
        assert_eq!(prompt_line(&[]), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
