use super::Tool;
use crate::error::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Default)]
pub struct RunShellTool {
    /// Where commands run; `None` is wherever the process started.
    pub work_dir: Option<std::path::PathBuf>,
}

impl RunShellTool {
    fn sh(&self, script: String) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(script);
        if let Some(dir) = &self.work_dir {
            command.current_dir(dir);
        }
        command
    }
}

#[async_trait]
impl Tool for RunShellTool {
    fn name(&self) -> &str {
        "shell.run"
    }

    fn description(&self) -> &str {
        "Run a shell command on this computer. Use for launching apps or running commands. Requires user confirmation."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Command to run"},
                "background": {"type": "boolean", "description": "Start the command in the background and return immediately (use for GUI apps that would otherwise block)"}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing command".to_string()))?;
        let background = args["background"].as_bool().unwrap_or(false);

        if background {
            // Detach the child so a GUI app does not hold the call open.
            // `setsid` detaches from the controlling terminal, which means the
            // process survives the shell that started it.
            let output = self
                .sh(format!("setsid {} >/dev/null 2>&1 &", command))
                .output()
                .await?;
            return Ok(json!({
                "success": output.status.success(),
                "status": output.status.code(),
                "background": true,
                "command": command,
            }));
        }

        let output = self.sh(command.to_string()).output().await?;
        Ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "status": output.status.code(),
        }))
    }
}
