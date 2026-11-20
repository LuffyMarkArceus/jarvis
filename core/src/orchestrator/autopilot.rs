//! Autopilot (Milestone 6.6): a turn that never prompts. What would ask is
//! deferred by the gate and listed for `/approve`; any other prompt (sending
//! data off the machine, a spill) is answered no and reported as not done.

use super::*;
use crate::conversation::Deferred;
use crate::modes::{protections, Call, Change};

/// Which deferred actions `/approve` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approve {
    /// Show them.
    List,
    All,
    /// The n-th, counting from 1.
    One(usize),
    /// Drop them.
    None,
}

/// The confirmer of an Autopilot turn: no to everything, and keep what.
pub(super) struct NoPrompts<'a> {
    pub(super) not_done: &'a std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ConfirmationHandler for NoPrompts<'_> {
    async fn confirm(&self, tool: &str, args: &Value) -> bool {
        let what = match tool {
            EGRESS_TOOL => {
                let items: Vec<&str> = args["would_send"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                format!("send {} to the remote model", items.join(", "))
            }
            EGRESS_WEB_TOOL => "a web call carrying what this turn may not send".to_string(),
            EGRESS_LOCAL_TOOL => "answer with the local model instead".to_string(),
            EGRESS_SPILL_TOOL => "answer with the remote model while the GPU was busy".to_string(),
            other => execute::describe_step(other, args),
        };
        self.not_done.lock().unwrap().push(what);
        false
    }
}

/// What an Autopilot answer ends with, if anything was left undone.
pub(super) fn report(deferred: &[Deferred], not_done: &[String]) -> Option<String> {
    if deferred.is_empty() && not_done.is_empty() {
        return None;
    }
    let mut out = String::new();
    if !deferred.is_empty() {
        out.push_str("Waiting for you (/approve all, /approve <n>, /approve none):\n");
        out.push_str(&listing(deferred));
    }
    if !not_done.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Not done (Autopilot does not ask):\n");
        for item in not_done {
            out.push_str(&format!("- {item}\n"));
        }
    }
    Some(out)
}

fn listing(deferred: &[Deferred]) -> String {
    deferred
        .iter()
        .enumerate()
        .map(|(i, d)| format!("{}. {}\n", i + 1, execute::describe_step(&d.name, &d.args)))
        .collect()
}

impl Orchestrator {
    /// `/approve`: run (or list, or drop) what Autopilot left. The owner's
    /// yes is the approval; the protections and Plan mode are checked
    /// again, under the conversation's mode now. What cannot run stays.
    pub async fn approve(&self, conv: &Conversation, which: Approve) -> Result<String> {
        let waiting = conv.deferred();
        if waiting.is_empty() {
            return Ok("Nothing is waiting for /approve.".to_string());
        }
        let chosen: Vec<usize> = match which {
            Approve::List => return Ok(listing(&waiting)),
            Approve::None => {
                conv.set_deferred(Vec::new());
                return Ok(format!("Dropped {} waiting action(s).", waiting.len()));
            }
            Approve::All => (0..waiting.len()).collect(),
            Approve::One(n) if (1..=waiting.len()).contains(&n) => vec![n - 1],
            Approve::One(n) => {
                return Err(JarvisError::Config(format!(
                    "There is no waiting action {n}: /approve lists them."
                )))
            }
        };
        let mode = conv.mode();
        let mut out = String::new();
        let mut kept = Vec::new();
        for (i, action) in waiting.into_iter().enumerate() {
            if !chosen.contains(&i) {
                kept.push(action);
                continue;
            }
            let step = execute::describe_step(&action.name, &action.args);
            let outside = action.args["outside_sandbox"].as_bool().unwrap_or(false)
                || self.runs_outside(&action.name, &action.args)
                || (action.name == "shell.run" && !mode.sandbox);
            let call = Call {
                rule: self
                    .permissions
                    .check_permission(&action.name, &action.args)
                    .action,
                change: if outside {
                    Change::Outside
                } else {
                    self.change_of(&action.name, &action.args, false)
                },
                untrusted: !action.untrusted.is_empty(),
                allow_outside: self.config.sandbox.allow_outside,
                sandbox_ready: crate::sandbox::ready(&self.config),
            };
            if let Some(why) = protections(&mode, &call) {
                out.push_str(&format!("{}. {step}: not run - {why}\n", i + 1));
                kept.push(action);
                continue;
            }
            let mut args = action.args.clone();
            if outside {
                args["outside_sandbox"] = json!(true);
            }
            // Plan changes nothing, even approved: a command that only looks
            // runs read-only.
            if mode.autonomy == crate::modes::Autonomy::Plan
                && call.change == (Change::Shell { writes: false })
            {
                args["_read_only"] = json!(true);
            }
            let outcome = self
                .run_tool(conv, action.name.clone(), args, "approved_deferred", None)
                .await;
            let result = match outcome.status {
                OutcomeStatus::Succeeded => "done".to_string(),
                _ => format!("failed - {}", outcome.output),
            };
            out.push_str(&format!("{}. {step}: {result}\n", i + 1));
        }
        conv.set_deferred(kept);
        Ok(out)
    }
}
