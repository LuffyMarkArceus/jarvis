use super::Tool;
use crate::error::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct GetSystemInfoTool;

#[async_trait]
impl Tool for GetSystemInfoTool {
    fn name(&self) -> &str {
        "system.info"
    }

    fn description(&self) -> &str {
        "Get the operating system, CPU architecture and hostname of this computer. Use ONLY when the user asks about system specs. Do NOT use to list files or directories."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> Result<Value> {
        let hostname = tokio::fs::read_to_string("/etc/hostname")
            .await
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(json!({
            "hostname": hostname,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        }))
    }
}

/// How many processes to return when the caller does not say.
const DEFAULT_PROCESS_LIMIT: usize = 10;

pub struct ListProcessesTool;

#[async_trait]
impl Tool for ListProcessesTool {
    fn name(&self) -> &str {
        "process.list"
    }

    fn description(&self) -> &str {
        "List running processes, busiest first, with their CPU and memory use. Use when the user asks what is running or what is using CPU or memory. The CPU percentage is an average over each process's lifetime, not a live sample."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "How many processes to return, busiest first (default 10, max 100)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PROCESS_LIMIT as u64)
            .clamp(1, 100) as usize;

        // `ps` rather than /proc: parsing /proc by hand means sampling twice
        // with a sleep in between to get a meaningful CPU number, and `ps`
        // already does that bookkeeping.
        let output = tokio::process::Command::new("ps")
            .args(["-eo", "pid=,comm=,%cpu=,%mem="])
            .output()
            .await?;

        if !output.status.success() {
            return Err(crate::error::JarvisError::Tool(format!(
                "ps failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let mut processes: Vec<Value> = text.lines().filter_map(parse_process_line).collect();
        let total = processes.len();

        // Busiest first. Ties fall back to memory so the order is stable
        // rather than dependent on `ps` output order.
        processes.sort_by(|a, b| {
            let cpu_a = a["cpu_percent"].as_f64().unwrap_or(0.0);
            let cpu_b = b["cpu_percent"].as_f64().unwrap_or(0.0);
            cpu_b
                .partial_cmp(&cpu_a)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let mem_a = a["memory_percent"].as_f64().unwrap_or(0.0);
                    let mem_b = b["memory_percent"].as_f64().unwrap_or(0.0);
                    mem_b
                        .partial_cmp(&mem_a)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        processes.truncate(limit);

        Ok(json!({
            "processes": processes,
            "total": total,
            "cpu_note": "CPU% is the average since each process started, not a live sample.",
        }))
    }
}

/// Parse one `ps -eo pid=,comm=,%cpu=,%mem=` line.
///
/// `comm` is the last free-text field, so the numeric columns are read from
/// the right rather than assuming a fixed number of leading tokens.
fn parse_process_line(line: &str) -> Option<Value> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let pid: u32 = parts[0].parse().ok()?;
    let memory = parts[parts.len() - 1].parse::<f64>().unwrap_or(0.0);
    let cpu = parts[parts.len() - 2].parse::<f64>().unwrap_or(0.0);
    let name = parts[1..parts.len() - 2].join(" ");
    if name.is_empty() {
        return None;
    }
    Some(json!({
        "pid": pid,
        "name": name,
        "cpu_percent": cpu,
        "memory_percent": memory,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn parses_a_ps_line() {
        let p = parse_process_line("  1234 firefox  12.3  4.1").expect("parsed");
        assert_eq!(p["pid"], 1234);
        assert_eq!(p["name"], "firefox");
        assert_eq!(p["cpu_percent"], 12.3);
        assert_eq!(p["memory_percent"], 4.1);
    }

    #[test]
    fn keeps_a_name_with_spaces_together() {
        let p = parse_process_line("42 my process 1.0 2.0").expect("parsed");
        assert_eq!(p["name"], "my process");
    }

    #[test]
    fn ignores_a_truncated_line() {
        assert!(parse_process_line("1234 firefox").is_none());
    }

    #[tokio::test]
    async fn lists_real_processes_busiest_first() {
        let res = ListProcessesTool
            .execute(json!({ "limit": 3 }))
            .await
            .expect("process list");

        let processes = res["processes"].as_array().expect("processes");
        assert!(!processes.is_empty(), "ps should report something");
        assert!(processes.len() <= 3);

        let cpus: Vec<f64> = processes
            .iter()
            .map(|p| p["cpu_percent"].as_f64().unwrap_or(0.0))
            .collect();
        assert!(
            cpus.windows(2).all(|w| w[0] >= w[1]),
            "processes should be sorted by cpu: {:?}",
            cpus
        );
        // The lifetime-average caveat has to travel with the data, or the
        // answer built from it will overstate what `ps` measured.
        assert!(res["cpu_note"].as_str().unwrap().contains("average"));
    }

    #[tokio::test]
    async fn the_limit_is_clamped() {
        let res = ListProcessesTool
            .execute(json!({ "limit": 0 }))
            .await
            .expect("process list");
        assert_eq!(res["processes"].as_array().unwrap().len(), 1);
    }
}
