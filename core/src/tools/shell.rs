use super::Tool;
use crate::error::Result;
use crate::sandbox::{is_secret_var, Sandbox};
use async_trait::async_trait;
use serde_json::{json, Value};

#[derive(Default)]
pub struct RunShellTool {
    /// Where commands run; `None` is wherever the process started.
    pub work_dir: Option<std::path::PathBuf>,
    /// The sandbox commands run in; `None` when there is none, and then
    /// every command runs outside (and the gate asks first).
    pub sandbox: Option<Sandbox>,
    /// Variables holding Jarvis's model keys, removed from every command's
    /// environment along with other secret-looking names.
    pub key_vars: Vec<String>,
}

/// Output that says the sandbox, not the command, was the problem.
const SANDBOX_SIGNS: &[&str] = &[
    "Read-only file system",
    "Could not resolve",
    "Temporary failure in name resolution",
    "Network is unreachable",
    "Name or service not known",
];

impl RunShellTool {
    /// `sh -c script`, in the sandbox unless `outside`.
    fn sh(&self, script: String, outside: bool, background: bool) -> tokio::process::Command {
        let mut command = match (&self.sandbox, outside) {
            (Some(sandbox), false) => {
                let mut c = tokio::process::Command::new("bwrap");
                c.args(sandbox.bwrap_args(self.work_dir.as_deref(), background));
                c.args(["sh", "-c"]).arg(script);
                c
            }
            _ => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(script);
                c
            }
        };
        if let Some(dir) = &self.work_dir {
            command.current_dir(dir);
        }
        for (name, _) in std::env::vars_os() {
            if is_secret_var(&name.to_string_lossy(), &self.key_vars) {
                command.env_remove(name);
            }
        }
        command
    }

    /// What the model is told about where the command ran.
    fn describe(&self, result: &mut Value, outside: bool) {
        let sandbox = match (&self.sandbox, outside) {
            (Some(sandbox), false) => sandbox,
            _ => {
                result["sandboxed"] = json!(false);
                return;
            }
        };
        result["sandboxed"] = json!(true);
        result["writable"] = json!(sandbox
            .writable
            .iter()
            .map(|p| p.display().to_string())
            .chain(std::iter::once("/tmp (discarded)".to_string()))
            .collect::<Vec<_>>());
        result["network"] = json!(sandbox.network);
        let failed = result["status"].as_i64().is_some_and(|s| s != 0);
        let said = format!(
            "{}{}",
            result["stdout"].as_str().unwrap_or(""),
            result["stderr"].as_str().unwrap_or("")
        );
        if failed && SANDBOX_SIGNS.iter().any(|sign| said.contains(sign)) {
            result["hint"] = json!(
                "The sandbox stopped this: it can write only the folders listed and has no network. If the command really needs more, call it again with outside_sandbox: true to run it outside the sandbox; the owner will be asked."
            );
        }
    }
}

#[async_trait]
impl Tool for RunShellTool {
    fn name(&self) -> &str {
        "shell.run"
    }

    fn description(&self) -> &str {
        "Run a shell command on this computer. Commands run in a sandbox: they can write only the work folder and /tmp, and have no network. Set outside_sandbox only when a command cannot work without more (installing, pushing, a GUI app); the owner is always asked."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Command to run"},
                "background": {"type": "boolean", "description": "Start the command in the background and return immediately (use for GUI apps that would otherwise block)"},
                "outside_sandbox": {"type": "boolean", "description": "Run without the sandbox: full access to files and the network. Always asks the owner."}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| crate::error::JarvisError::Tool("Missing command".to_string()))?;
        let background = args["background"].as_bool().unwrap_or(false);
        let outside = args["outside_sandbox"].as_bool().unwrap_or(false);

        if background {
            // Detach the child so a GUI app does not hold the call open.
            // `setsid` detaches from the controlling terminal, which means the
            // process survives the shell that started it.
            let output = self
                .sh(
                    format!("setsid {} >/dev/null 2>&1 &", command),
                    outside,
                    true,
                )
                .output()
                .await?;
            let mut result = json!({
                "success": output.status.success(),
                "status": output.status.code(),
                "background": true,
                "command": command,
            });
            self.describe(&mut result, outside);
            return Ok(result);
        }

        let output = self
            .sh(command.to_string(), outside, false)
            .output()
            .await?;
        let mut result = json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "status": output.status.code(),
        });
        self.describe(&mut result, outside);
        Ok(result)
    }
}
