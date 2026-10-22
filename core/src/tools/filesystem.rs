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
        "List contents of a directory"
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
        "Read contents of a file"
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
