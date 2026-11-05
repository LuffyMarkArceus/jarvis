//! Turning tool results and outcomes into text.

use super::*;

/// How Jarvis's own reports of what ran begin (`render_execution_report`,
/// `render_blocked_report`). They show tool output as such - not a model's
/// advice - so answer checks that judge what the model said skip them.
pub const REPORT_OPENINGS: &[&str] = &[
    "Here is what actually happened:",
    "I ran part of your request, but one action was not completed.",
    "I did not run that - ",
];

/// Maximum number of directory entries shown in a reply.
pub(super) const MAX_LIST_ENTRIES: usize = 200;

/// Maximum number of characters of file content shown in a reply.
pub(super) const MAX_READ_CHARS: usize = 4000;

/// Render a compact, model-friendly version of a tool's raw JSON result.
pub(super) fn compact_output(tool: &str, res: &Value) -> String {
    match tool {
        "filesystem.list_dir" => {
            let Some(entries) = res.get("entries").and_then(|e| e.as_array()) else {
                return serde_json::to_string(res).unwrap_or_default();
            };
            if entries.is_empty() {
                return "(empty directory)".to_string();
            }
            entries
                .iter()
                .map(|e| {
                    let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let is_dir = e.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false);
                    if is_dir {
                        format!("{}/", name)
                    } else {
                        let size = e.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                        format!("{} ({} bytes)", name, size)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "filesystem.read" => res
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim_end()
            .to_string(),
        "filesystem.write" | "filesystem.edit" => match res.get("path").and_then(|p| p.as_str()) {
            Some(path) => format!("wrote {}", path),
            None => serde_json::to_string(res).unwrap_or_default(),
        },
        // Every field the tool returns. A list of names here once dropped
        // memory_total when the tool gained it.
        "system.info" => {
            let parts: Vec<String> = res
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some(format!("{}={}", k, s)),
                    Value::Null => None,
                    other => Some(format!("{}={}", k, other)),
                })
                .collect();
            if parts.is_empty() {
                serde_json::to_string(res).unwrap_or_default()
            } else {
                parts.join(", ")
            }
        }
        "shell.run" => {
            if res
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return format!(
                    "started in the background: {}",
                    res.get("command").and_then(|c| c.as_str()).unwrap_or("")
                );
            }
            let stdout = res
                .get("stdout")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim_end();
            let stderr = res
                .get("stderr")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            let status = res.get("status").and_then(|s| s.as_i64()).unwrap_or(0);
            let mut out = String::new();
            if !stdout.is_empty() {
                out.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("stderr: ");
                out.push_str(stderr);
            }
            if out.is_empty() {
                out.push_str(&format!("exit status {}", status));
            }
            out
        }
        "filesystem.search" => {
            let Some(matches) = res.get("matches").and_then(|m| m.as_array()) else {
                return serde_json::to_string(res).unwrap_or_default();
            };
            if matches.is_empty() {
                return format!(
                    "no matches ({} entries searched)",
                    res.get("scanned").and_then(|s| s.as_u64()).unwrap_or(0)
                );
            }
            let mut out = matches
                .iter()
                .map(|m| {
                    let path = m.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    match m.get("line").and_then(|l| l.as_u64()) {
                        Some(line) => format!("{}:{}", path, line),
                        None => path.to_string(),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if res
                .get("truncated")
                .and_then(|t| t.as_bool())
                .unwrap_or(false)
            {
                out.push_str("\n(results were capped)");
            }
            out
        }
        "filesystem.stat" => {
            let path = res.get("path").and_then(|p| p.as_str()).unwrap_or("?");
            let kind = if res.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false) {
                "directory"
            } else {
                "file"
            };
            format!(
                "{} is a {}, {} bytes",
                path,
                kind,
                res.get("size").and_then(|s| s.as_u64()).unwrap_or(0)
            )
        }
        "process.list" => {
            let Some(processes) = res.get("processes").and_then(|p| p.as_array()) else {
                return serde_json::to_string(res).unwrap_or_default();
            };
            processes
                .iter()
                .map(|p| {
                    format!(
                        "{} {} cpu={:.1}% mem={:.1}%",
                        p.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                        p.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                        p.get("cpu_percent").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        p.get("memory_percent")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => match res.as_object() {
            Some(map) if !map.is_empty() => {
                let parts: Vec<String> = map
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| format!("{}={}", k, s)))
                    .collect();
                if parts.is_empty() {
                    serde_json::to_string(res).unwrap_or_default()
                } else {
                    parts.join(", ")
                }
            }
            _ => serde_json::to_string(res).unwrap_or_default(),
        },
    }
}

/// Render the user-facing reply for a successful tool call. Deterministic:
/// every fact in the reply comes straight from the tool result.
pub(super) fn render_tool_output(outcome: &ToolOutcome) -> String {
    if outcome.status == OutcomeStatus::Failed {
        return outcome.output.clone();
    }

    let mut out = String::new();
    match outcome.name.as_str() {
        "filesystem.list_dir" => {
            let path = outcome.args["path"].as_str().unwrap_or(".");
            let lines: Vec<&str> = outcome.output.lines().collect();
            out.push_str(&format!("Contents of {}:\n", path));
            for line in lines.iter().take(MAX_LIST_ENTRIES) {
                out.push_str(&format!("  {}\n", line));
            }
            if lines.len() > MAX_LIST_ENTRIES {
                out.push_str(&format!(
                    "  ... and {} more entries\n",
                    lines.len() - MAX_LIST_ENTRIES
                ));
            }
            out.push_str(&format!("\n{} entries.\n", lines.len()));
        }
        "filesystem.read" => {
            let path = outcome.args["path"].as_str().unwrap_or("");
            out.push_str(&format!("Contents of {}:\n", path));
            if outcome.output.chars().count() > MAX_READ_CHARS {
                out.push_str(&truncate(&outcome.output, MAX_READ_CHARS));
                out.push_str("\n... (truncated)\n");
            } else {
                out.push_str(&outcome.output);
                out.push('\n');
            }
        }
        "shell.run" => {
            let cmd = outcome.args["command"].as_str().unwrap_or("");
            let background = outcome.args["background"].as_bool().unwrap_or(false);
            if background {
                out.push_str(&format!("Started in the background: {}\n", cmd));
            } else {
                out.push_str(&format!("$ {}\n", cmd));
                if !outcome.output.is_empty() {
                    out.push_str(&outcome.output);
                    out.push('\n');
                }
            }
        }
        "system.info" => {
            out.push_str(&format!("System: {}\n", outcome.output));
        }
        "filesystem.search" => {
            let root = outcome.args["path"].as_str().unwrap_or(".");
            let res = outcome.result.as_ref();
            let matches = res
                .and_then(|r| r.get("matches"))
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();

            if matches.is_empty() {
                out.push_str(&format!("No matches under {}.\n", root));
                let scanned = res
                    .and_then(|r| r.get("scanned"))
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0);
                out.push_str(&format!("Searched {} entries.\n", scanned));
            } else {
                out.push_str(&format!("{} match(es) under {}:\n", matches.len(), root));
                for m in &matches {
                    let path = m.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    match (
                        m.get("line").and_then(|l| l.as_u64()),
                        m.get("preview").and_then(|p| p.as_str()),
                    ) {
                        (Some(line), Some(preview)) => {
                            out.push_str(&format!("  {}:{}\n      {}\n", path, line, preview));
                        }
                        _ => out.push_str(&format!("  {}\n", path)),
                    }
                }
                if res
                    .and_then(|r| r.get("truncated"))
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false)
                {
                    out.push_str(
                        "\nThe search stopped at its result limit; narrow it to see the rest.\n",
                    );
                }
            }
        }
        "filesystem.stat" => {
            let res = outcome.result.as_ref();
            let field = |key: &str| res.and_then(|r| r.get(key)).cloned().unwrap_or(Value::Null);
            let path = field("path");
            let path = path.as_str().unwrap_or("?");
            let kind = if field("is_dir").as_bool().unwrap_or(false) {
                "directory"
            } else {
                "file"
            };
            out.push_str(&format!("{}:\n", path));
            out.push_str(&format!("  type: {}\n", kind));
            out.push_str(&format!(
                "  size: {} bytes\n",
                field("size").as_u64().unwrap_or(0)
            ));
            if let Some(modified) = field("modified").as_str() {
                out.push_str(&format!("  modified: {}\n", modified));
            }
            if let Some(permissions) = field("permissions").as_str() {
                out.push_str(&format!("  permissions: {}\n", permissions));
            }
        }
        "process.list" => {
            let res = outcome.result.as_ref();
            let processes = res
                .and_then(|r| r.get("processes"))
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            if processes.is_empty() {
                out.push_str("Could not read the process list.\n");
            } else {
                out.push_str(&format!(
                    "{:<8} {:<28} {:>6} {:>6}\n",
                    "PID", "PROCESS", "CPU%", "MEM%"
                ));
                for p in &processes {
                    out.push_str(&format!(
                        "{:<8} {:<28} {:>6.1} {:>6.1}\n",
                        p.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                        p.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                        p.get("cpu_percent").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        p.get("memory_percent")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                    ));
                }
                // `ps` reports CPU time averaged over the process lifetime.
                // Presenting that as current load would be a lie the user
                // could not detect from the numbers alone.
                out.push_str("\nCPU% is each process's lifetime average, not a live sample.\n");
            }
        }
        _ => {
            out.push_str(&format!("{}:\n", outcome.name));
            out.push_str(&outcome.output);
            out.push('\n');
        }
    }
    out
}

/// Compact, length-limited rendering of tool arguments.
pub(super) fn compact_args(args: &Value) -> String {
    let s = serde_json::to_string(args).unwrap_or_default();
    truncate(&s, 300)
}

/// Truncate on a char boundary.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

/// Deterministic report used when at least one action was blocked.
///
/// This is the anti-hallucination guarantee: the model's free text is not
/// consulted at all for these turns.
pub(super) fn render_blocked_report(trace: &[ToolOutcome]) -> String {
    let blocked: Vec<&ToolOutcome> = trace.iter().filter(|t| t.status.is_blocked()).collect();
    let done: Vec<&ToolOutcome> = trace.iter().filter(|t| !t.status.is_blocked()).collect();

    let mut out = String::new();
    if done.is_empty() {
        // Say *why* nothing ran. "Blocked by policy" is wrong when the user
        // themselves clicked no.
        let only_declined = blocked.iter().all(|t| t.status == OutcomeStatus::Declined);
        let only_withheld = blocked.iter().all(|t| t.status == OutcomeStatus::Withheld);
        let only_untrusted = blocked.iter().all(|t| t.status == OutcomeStatus::Untrusted);
        if only_declined {
            out.push_str("I did not run that - you declined the confirmation prompt.\n\n");
        } else if only_untrusted {
            out.push_str("I did not run that - the model asked for it after reading content you have not marked as trusted, and nobody could be asked.\n\n");
        } else if only_withheld {
            out.push_str("I did not run that - it would have sent something off this machine that may not leave it.\n\n");
        } else {
            out.push_str("I did not run that - it was blocked by the permission policy.\n\n");
        }
    } else {
        out.push_str("I ran part of your request, but one action was not completed.\n\n");
    }

    for t in &blocked {
        out.push_str(&format!("  {}\n", t.name));
        if let Some(pattern) = &t.pattern {
            out.push_str(&format!("    matched: \"{}\"\n", pattern));
        }
        out.push_str(&format!("    args:   {}\n", compact_args(&t.args)));
        out.push_str(&format!("    status: {}\n", t.status.label()));
        if matches!(t.status, OutcomeStatus::Withheld | OutcomeStatus::Untrusted) {
            out.push_str(&format!("    why:    {}\n", t.output));
        }
    }

    if !done.is_empty() {
        out.push_str("\nExecuted successfully:\n");
        for t in &done {
            out.push_str(&format!("  {}\n", t.name));
            out.push_str(&format!("    args: {}\n", compact_args(&t.args)));
            if !t.output.is_empty() {
                out.push_str(&format!("    {}\n", indent(&t.output, 4)));
            }
        }
    }

    // Only suggest loosening the policy when the policy is what stopped it,
    // and only at tool level: when a *pattern* denied the call, telling the
    // user to allow the whole tool is the opposite of useful advice.
    let tool_level: Vec<&ToolOutcome> = blocked
        .iter()
        .copied()
        .filter(|t| {
            t.pattern.is_none()
                && !matches!(
                    t.status,
                    OutcomeStatus::Declined | OutcomeStatus::Withheld | OutcomeStatus::Untrusted
                )
        })
        .collect();
    if !tool_level.is_empty() {
        let mut names: Vec<&str> = tool_level.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        out.push_str(&format!(
            "\nNo action was taken for the blocked item(s). To permit them, run:\n  jarvis permissions allow {}\n",
            names.join(" ")
        ));
    }
    out
}

/// Deterministic report used when the iteration budget runs out.
pub(super) fn render_execution_report(trace: &[ToolOutcome]) -> String {
    if trace.is_empty() {
        return "I could not complete that request.".to_string();
    }

    let mut out = String::from(REPORT_OPENINGS[0]);
    out.push_str("\n\n");
    for t in trace {
        out.push_str(&format!("  {}\n", t.name));
        out.push_str(&format!("    args:   {}\n", compact_args(&t.args)));
        out.push_str(&format!("    status: {}\n", t.status.label()));
        if !t.output.is_empty() {
            out.push_str(&format!("    {}\n", indent(&t.output, 4)));
        }
    }
    out
}

/// Indent every line of `text` by `n` spaces.
pub(super) fn indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", pad, l)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// If any action was blocked, render the deterministic report.
pub(super) fn blocked_report(trace: &[ToolOutcome]) -> Option<String> {
    if trace.iter().any(|t| t.status.is_blocked()) {
        Some(render_blocked_report(trace))
    } else {
        None
    }
}
