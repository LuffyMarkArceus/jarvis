use super::Tool;
use crate::error::Result;
use crate::sandbox::{is_secret_var, Sandbox, SESSION_VARS};
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

/// The program a command line runs, if it cannot be found: a path that is
/// not an executable file, or a name on no directory of `PATH`. Leading
/// `NAME=value` assignments are skipped.
fn missing_program(command: &str) -> Option<String> {
    let program = command
        .split_whitespace()
        .find(|word| !word.contains('=') || word.starts_with('='))?;
    let is_exe = |p: &std::path::Path| {
        use std::os::unix::fs::PermissionsExt as _;
        p.is_file()
            && p.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    };
    let found = if program.contains('/') {
        is_exe(std::path::Path::new(program))
    } else {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| is_exe(&dir.join(program))))
            .unwrap_or(false)
    };
    (!found).then(|| program.to_string())
}

/// Output that says the sandbox, not the command, was the problem.
/// A command approved without asking ran read-only and tried to write.
pub const READ_ONLY_HINT: &str =
    "This ran read-only and needs to write. Call it again with writes: true to ask the owner.";

const SANDBOX_SIGNS: &[&str] = &[
    "Read-only file system",
    "Could not resolve",
    "Temporary failure in name resolution",
    "Network is unreachable",
    "Name or service not known",
];

impl RunShellTool {
    /// `sh -c script`, in the sandbox unless `outside`; read-only (nothing
    /// writable, no network) when `read_only`.
    fn sh(
        &self,
        script: String,
        outside: bool,
        background: bool,
        read_only: bool,
    ) -> tokio::process::Command {
        let mut command = match (&self.sandbox, outside) {
            (Some(sandbox), false) => {
                let mut c = tokio::process::Command::new("bwrap");
                let sandbox = if read_only {
                    sandbox.read_only()
                } else {
                    sandbox.clone()
                };
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
        let sandboxed = self.sandbox.is_some() && !outside;
        for (name, _) in std::env::vars_os() {
            let name_str = name.to_string_lossy();
            if is_secret_var(&name_str, &self.key_vars)
                || (sandboxed && SESSION_VARS.contains(&name_str.as_ref()))
            {
                command.env_remove(name);
            }
        }
        command
    }

    /// What the model is told about where the command ran.
    fn describe(&self, result: &mut Value, outside: bool, read_only: bool) {
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
        if read_only {
            result["writable"] = json!(["/tmp (discarded)"]);
            result["network"] = json!(false);
            result["read_only"] = json!(true);
        }
        if read_only && failed && said.contains("Read-only file system") {
            result["hint"] = json!(READ_ONLY_HINT);
        } else if failed && SANDBOX_SIGNS.iter().any(|sign| said.contains(sign)) {
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
                "outside_sandbox": {"type": "boolean", "description": "Run without the sandbox: full access to files and the network. Always asks the owner."},
                "writes": {"type": "boolean", "description": "Set true when the command changes, creates or deletes files. Commands that only look can leave it out."}
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
        // Set by the gate, never by the model's word alone: only narrows.
        let read_only = args["_read_only"].as_bool().unwrap_or(false);

        if background {
            // `setsid X &` succeeds whether or not X exists, so check first:
            // "Started in the background" for nothing is the worst kind of
            // failure - confidently false.
            if let Some(program) = missing_program(command) {
                return Err(crate::error::JarvisError::Tool(format!(
                    "No program called '{program}' is installed on this machine, so nothing was started."
                )));
            }
            // Detach the child so a GUI app does not hold the call open.
            // `setsid` detaches from the controlling terminal, which means the
            // process survives the shell that started it.
            let output = self
                .sh(
                    format!("setsid {} >/dev/null 2>&1 &", command),
                    outside,
                    true,
                    read_only,
                )
                .output()
                .await?;
            let mut result = json!({
                "success": output.status.success(),
                "status": output.status.code(),
                "background": true,
                "command": command,
            });
            self.describe(&mut result, outside, read_only);
            return Ok(result);
        }

        let output = self
            .sh(command.to_string(), outside, false, read_only)
            .output()
            .await?;
        let mut result = json!({
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
            "status": output.status.code(),
        });
        self.describe(&mut result, outside, read_only);
        Ok(result)
    }
}
