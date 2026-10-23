use super::Tool;
use crate::error::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "filesystem.list_dir"
    }

    fn description(&self) -> &str {
        "List the names of files and directories in a directory. Use when the user asks what files or folders exist at a path."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing path".to_string()))?;
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(path).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "is_dir": metadata.is_dir(),
                "size": metadata.len(),
            }));
        }
        Ok(json!({"entries": entries}))
    }
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "filesystem.read"
    }

    fn description(&self) -> &str {
        "Read the text contents of a file. Use when the user asks what is inside a specific file."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing path".to_string()))?;
        let content = tokio::fs::read_to_string(path).await?;
        Ok(json!({"content": content}))
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "filesystem.write"
    }

    fn description(&self) -> &str {
        "Write content to a file (creates or overwrites)"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "content": {"type": "string", "description": "Content to write"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing path".to_string()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing content".to_string()))?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content).await?;
        Ok(json!({"success": true, "path": path}))
    }
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "filesystem.edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing old string with new string"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "old_string": {"type": "string", "description": "String to find"},
                "new_string": {"type": "string", "description": "String to replace with"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing path".to_string()))?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing old_string".to_string()))?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing new_string".to_string()))?;
        let content = tokio::fs::read_to_string(path).await?;
        if !content.contains(old_string) {
            return Err(crate::error::JarvisError::Tool(
                "old_string not found".to_string(),
            ));
        }
        let new_content = content.replace(old_string, new_string);
        tokio::fs::write(path, new_content).await?;
        Ok(json!({"success": true, "path": path}))
    }
}

/// Directories that are never worth walking.
///
/// A search from `~` would otherwise spend all its time inside build output
/// and caches. These are the ones that dominate on a developer machine.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".cache",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    ".cargo",
];

/// How deep to descend before giving up. Deep enough for normal projects,
/// shallow enough that a wrong turn cannot walk the whole filesystem.
const MAX_SEARCH_DEPTH: usize = 8;

/// How many directory entries to look at before returning what we have.
const MAX_SEARCH_ENTRIES: usize = 20_000;

/// Files larger than this are skipped for content search. Reading a 2 GB
/// log to find one line is not worth it.
const MAX_SEARCH_FILE_BYTES: u64 = 1_000_000;

const DEFAULT_MAX_RESULTS: usize = 50;

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "filesystem.search"
    }

    fn description(&self) -> &str {
        "Search a directory tree for files by name and/or by text inside them. Use when the user asks to find files, or to find where something is written. Returns file paths, and for text matches the line number and the matching line."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to search. Defaults to the current directory."
                },
                "name": {
                    "type": "string",
                    "description": "Glob for file names, e.g. \"*.rs\" or \"notes*\""
                },
                "content": {
                    "type": "string",
                    "description": "Text to find inside file contents"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default 50)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let root = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let name_pattern = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let needle = args
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_RESULTS as u64)
            .clamp(1, 200) as usize;

        if name_pattern.is_none() && needle.is_none() {
            return Err(crate::error::JarvisError::Tool(
                "filesystem.search needs a name pattern, a content string, or both".to_string(),
            ));
        }

        let root_path = std::path::PathBuf::from(&root);
        if !root_path.is_dir() {
            return Err(crate::error::JarvisError::Tool(format!(
                "{} is not a directory",
                root
            )));
        }

        // An explicit stack rather than recursion: async recursion needs
        // boxing, and a depth-tracked stack is easier to bound anyway.
        let mut stack = vec![(root_path, 0usize)];
        let mut matches: Vec<Value> = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = false;

        'walk: while let Some((dir, depth)) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
                // An unreadable directory is not a failure of the search.
                continue;
            };

            while let Ok(Some(entry)) = entries.next_entry().await {
                scanned += 1;
                if scanned > MAX_SEARCH_ENTRIES {
                    truncated = true;
                    break 'walk;
                }

                let path = entry.path();
                let file_name = entry.file_name().to_string_lossy().to_string();
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };

                if file_type.is_dir() {
                    if SKIPPED_DIRS.contains(&file_name.as_str()) {
                        continue;
                    }
                    if depth < MAX_SEARCH_DEPTH {
                        stack.push((path, depth + 1));
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }

                let name_hit = name_pattern
                    .as_deref()
                    .map(|p| crate::permissions::glob_match(p, &file_name))
                    .unwrap_or(false);

                let mut content_hit: Option<(usize, String)> = None;
                if let Some(needle) = needle.as_deref() {
                    content_hit = match_content(&path, needle).await;
                }

                if name_hit || content_hit.is_some() {
                    let mut item = json!({ "path": path.to_string_lossy() });
                    if let Some((line, preview)) = content_hit {
                        item["line"] = json!(line);
                        item["preview"] = json!(preview);
                    }
                    matches.push(item);

                    if matches.len() >= max_results {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
        }

        Ok(json!({
            "root": root,
            "matches": matches,
            "scanned": scanned,
            "truncated": truncated,
        }))
    }
}

/// Find the first line in `path` containing `needle`.
async fn match_content(path: &std::path::Path, needle: &str) -> Option<(usize, String)> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if metadata.len() > MAX_SEARCH_FILE_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    // A NUL byte in the first block is a cheap, reliable-enough binary
    // check. Without it a search would happily match inside a `.so`.
    if bytes.iter().take(512).any(|b| *b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    for (index, line) in text.lines().enumerate() {
        if line.contains(needle) {
            let preview: String = line.trim().chars().take(160).collect();
            return Some((index + 1, preview));
        }
    }
    None
}

pub struct StatFileTool;

#[async_trait]
impl Tool for StatFileTool {
    fn name(&self) -> &str {
        "filesystem.stat"
    }

    fn description(&self) -> &str {
        "Get metadata for a single path: whether it is a file or directory, its size in bytes, when it was last modified, and its permissions. Use when the user asks how big something is or for details about a path."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to inspect"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing path".to_string()))?;
        let metadata = tokio::fs::metadata(path).await?;

        let modified = metadata
            .modified()
            .ok()
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339());

        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            Some(format!("{:o}", metadata.permissions().mode() & 0o777))
        };
        #[cfg(not(unix))]
        let permissions: Option<String> = None;

        Ok(json!({
            "path": path,
            "is_dir": metadata.is_dir(),
            "is_file": metadata.is_file(),
            "size": metadata.len(),
            "modified": modified,
            "permissions": permissions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-fs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn as_arg(path: &std::path::Path) -> String {
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn search_finds_files_by_name() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();

        let res = SearchFilesTool
            .execute(json!({"path": as_arg(&dir), "name": "*.rs"}))
            .await
            .expect("search");

        let matches = res["matches"].as_array().expect("matches");
        assert_eq!(matches.len(), 1);
        assert!(matches[0]["path"].as_str().unwrap().ends_with("main.rs"));
    }

    #[tokio::test]
    async fn search_reports_the_line_for_content_matches() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "first line\nfind me here\nthird\n").unwrap();

        let res = SearchFilesTool
            .execute(json!({"path": as_arg(&dir), "content": "find me here"}))
            .await
            .expect("search");

        let m = &res["matches"].as_array().unwrap()[0];
        assert_eq!(m["line"], 2);
        assert_eq!(m["preview"], "find me here");
    }

    #[tokio::test]
    async fn search_ignores_build_output() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("target").join("debug")).unwrap();
        std::fs::write(dir.join("target").join("debug").join("gen.rs"), "x").unwrap();
        std::fs::write(dir.join("keep.rs"), "x").unwrap();

        let res = SearchFilesTool
            .execute(json!({"path": as_arg(&dir), "name": "*.rs"}))
            .await
            .expect("search");

        let matches = res["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0]["path"].as_str().unwrap().ends_with("keep.rs"));
    }

    #[tokio::test]
    async fn search_does_not_match_inside_binary_files() {
        let dir = temp_dir();
        std::fs::write(dir.join("blob.bin"), b"\0needle\0").unwrap();

        let res = SearchFilesTool
            .execute(json!({"path": as_arg(&dir), "content": "needle"}))
            .await
            .expect("search");

        assert!(res["matches"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_needs_something_to_look_for() {
        let dir = temp_dir();
        assert!(SearchFilesTool
            .execute(json!({"path": as_arg(&dir)}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn search_rejects_a_root_that_is_not_a_directory() {
        assert!(SearchFilesTool
            .execute(json!({"path": "/definitely/not/here", "name": "*"}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn search_says_when_it_stopped_early() {
        let dir = temp_dir();
        for i in 0..5 {
            std::fs::write(dir.join(format!("f{}.txt", i)), "x").unwrap();
        }

        let res = SearchFilesTool
            .execute(json!({"path": as_arg(&dir), "name": "*", "max_results": 2}))
            .await
            .expect("search");

        assert_eq!(res["matches"].as_array().unwrap().len(), 2);
        assert_eq!(res["truncated"], true);
    }

    #[tokio::test]
    async fn stat_reports_size_and_type() {
        let dir = temp_dir();
        let file = dir.join("x.txt");
        std::fs::write(&file, "12345").unwrap();

        let res = StatFileTool
            .execute(json!({"path": file.to_string_lossy()}))
            .await
            .expect("stat");

        assert_eq!(res["is_file"], true);
        assert_eq!(res["is_dir"], false);
        assert_eq!(res["size"], 5);
        assert!(res["modified"].as_str().is_some());
        assert!(res["permissions"].as_str().is_some());
    }

    #[tokio::test]
    async fn stat_reports_a_directory() {
        let dir = temp_dir();
        let res = StatFileTool
            .execute(json!({"path": as_arg(&dir)}))
            .await
            .expect("stat");
        assert_eq!(res["is_dir"], true);
    }

    #[tokio::test]
    async fn stat_fails_for_a_missing_path() {
        assert!(StatFileTool
            .execute(json!({"path": "/definitely/not/here"}))
            .await
            .is_err());
    }
}
