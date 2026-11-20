//! `jarvis report`: what the turn log (`turns.jsonl`) says about real use -
//! how slow, where the time went, what went wrong, what the owner flagged.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The summary of the log's lines (oldest first).
pub fn summarize(text: &str) -> String {
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let turns: Vec<&Value> = lines.iter().filter(|l| l["type"] == "turn").collect();
    let feedback: Vec<&Value> = lines.iter().filter(|l| l["type"] == "feedback").collect();
    if turns.is_empty() {
        return "No turns logged yet. Use Jarvis for a while, then run `jarvis report` again."
            .to_string();
    }
    let num = |v: &Value| v.as_u64().unwrap_or(0);
    let jarvis_ms =
        |t: &Value| num(&t["timing"]["total_ms"]).saturating_sub(num(&t["timing"]["you_ms"]));
    let mut out = String::new();

    let first = turns[0]["at"].as_str().unwrap_or("?");
    let last = turns[turns.len() - 1]["at"].as_str().unwrap_or("?");
    let sessions: std::collections::BTreeSet<&str> =
        turns.iter().filter_map(|t| t["session"].as_str()).collect();
    let _ = writeln!(
        out,
        "{} turns in {} conversations, {} to {}\n",
        turns.len(),
        sessions.len(),
        &first[..first.len().min(16)],
        &last[..last.len().min(16)]
    );

    // How slow, without the owner's own time.
    let mut times: Vec<u64> = turns.iter().map(|t| jarvis_ms(t)).collect();
    times.sort_unstable();
    let _ = writeln!(
        out,
        "Time (Jarvis, not counting yours): median {}, p95 {}, slowest {}",
        secs(percentile(&times, 50)),
        secs(percentile(&times, 95)),
        secs(*times.last().unwrap_or(&0))
    );

    // Where it went, summed over every turn.
    let (mut load, mut prompt, mut gen, mut other_model, mut tools, mut you) = (0, 0, 0, 0, 0, 0);
    for t in &turns {
        for m in t["timing"]["model"].as_array().into_iter().flatten() {
            let (l, p, g) = (num(&m["load_ms"]), num(&m["prompt_ms"]), num(&m["gen_ms"]));
            load += l;
            prompt += p;
            gen += g;
            other_model += num(&m["wall_ms"]).saturating_sub(l + p + g);
        }
        for tool in t["timing"]["tools"].as_array().into_iter().flatten() {
            tools += num(&tool["ms"]);
        }
        you += num(&t["timing"]["you_ms"]);
    }
    // A tool's time includes any prompt it asked; that part is the owner's.
    let tools = tools.saturating_sub(you);
    let parts = [
        ("loading models", load),
        ("reading prompts", prompt),
        ("writing answers", gen),
        ("model, unaccounted (network, remote)", other_model),
        ("running tools", tools),
    ];
    let spent: u64 = parts.iter().map(|p| p.1).sum::<u64>().max(1);
    let _ = writeln!(out, "Where Jarvis's time went:");
    for (name, ms) in parts.iter().filter(|p| p.1 > 0) {
        let _ = writeln!(
            out,
            "  {:<38} {:>9}  {:>3}%",
            name,
            secs(*ms),
            ms * 100 / spent
        );
    }
    let _ = writeln!(out, "  (and {} waiting for you)\n", secs(you));

    // Per model.
    let mut by_model: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for t in &turns {
        let model = t["model"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or("no model");
        by_model
            .entry(model.to_string())
            .or_default()
            .push(jarvis_ms(t));
    }
    let _ = writeln!(out, "By model:");
    for (model, mut ms) in by_model {
        ms.sort_unstable();
        let _ = writeln!(
            out,
            "  {:<42} {:>4} turns, median {}",
            model,
            ms.len(),
            secs(percentile(&ms, 50))
        );
    }

    // What went wrong.
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for t in &turns {
        for s in t["signals"].as_array().into_iter().flatten() {
            *counts.entry(s.as_str().unwrap_or("?")).or_default() += 1;
        }
    }
    let _ = writeln!(out, "\nFriction:");
    if counts.is_empty() {
        let _ = writeln!(out, "  none");
    }
    let mut counts: Vec<(&str, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (signal, n) in counts {
        let _ = writeln!(out, "  {:<12} {:>4}  {}", signal, n, meaning(signal));
    }

    // The slowest turns, each with its biggest cost.
    let mut slow: Vec<&&Value> = turns.iter().collect();
    slow.sort_by_key(|t| std::cmp::Reverse(jarvis_ms(t)));
    let _ = writeln!(out, "\nSlowest turns:");
    for t in slow.iter().take(5) {
        let _ = writeln!(
            out,
            "  {:>7}  {:<22} {}",
            secs(jarvis_ms(t)),
            t["model"]
                .as_str()
                .filter(|m| !m.is_empty())
                .unwrap_or("no model"),
            excerpt(t["input"].as_str().unwrap_or(""), 70)
        );
    }

    // What the owner flagged.
    if !feedback.is_empty() {
        let inputs: BTreeMap<&str, &str> = turns
            .iter()
            .filter_map(|t| Some((t["id"].as_str()?, t["input"].as_str()?)))
            .collect();
        let _ = writeln!(out, "\nYour feedback:");
        for f in &feedback {
            let asked = f["turn"]
                .as_str()
                .and_then(|id| inputs.get(id))
                .unwrap_or(&"?");
            let note = f["note"].as_str().unwrap_or("");
            let _ = writeln!(
                out,
                "  /{:<4} {}{}",
                f["kind"].as_str().unwrap_or("?"),
                excerpt(asked, 60),
                if note.is_empty() {
                    String::new()
                } else {
                    format!("  - {}", note)
                }
            );
        }
    }
    out
}

/// What each signal means, for a reader who did not write the code.
fn meaning(signal: &str) -> &'static str {
    match signal {
        "declined" => "you said no to a prompt",
        "denied" => "a permission rule refused an action",
        "untrusted" => "an action was stopped after untrusted content",
        "withheld" => "something was not sent to a remote model",
        "failed" => "a tool call failed or was malformed",
        "not_asked" => "an action needed a yes and nobody could answer",
        "model_error" => "the model did not answer",
        "slow" => "Jarvis took over 20 s",
        "rephrased" => "asked again in other words within a minute",
        "cancelled" => "you cancelled the turn",
        "error" => "the turn ended with an error",
        "queued" => "waited for the GPU behind another window",
        "spilled" => "the GPU was busy, so the remote model answered",
        "spill_withheld" => "the GPU was busy, but what the turn held could not go remote",
        _ => "",
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) * p).div_ceil(100);
    sorted[index.min(sorted.len() - 1)]
}

fn secs(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

fn excerpt(text: &str, chars: usize) -> String {
    let flat = text.replace('\n', " ");
    let mut out: String = flat.chars().take(chars).collect();
    if flat.chars().count() > chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_signals_have_plain_words() {
        for signal in ["queued", "spilled", "spill_withheld"] {
            let words = meaning(signal);
            assert!(
                !words.is_empty() && !words.contains('_'),
                "{signal}: {words:?}"
            );
        }
    }
    use serde_json::json;

    fn turn(id: &str, input: &str, model: &str, total: u64, you: u64, signals: &[&str]) -> String {
        json!({
            "type": "turn", "id": id, "at": "2026-11-17T10:00:00Z", "session": "s1",
            "input": input, "model": model, "signals": signals,
            "timing": { "total_ms": total, "you_ms": you,
                "model": [{ "model": model, "wall_ms": total - you, "load_ms": 41000,
                            "prompt_ms": 45100, "gen_ms": 1200 }],
                "tools": [] }
        })
        .to_string()
    }

    #[test]
    fn the_report_says_how_slow_where_the_time_went_and_what_went_wrong() {
        let log = [
            turn(
                "a",
                "whats using space on my disk",
                "llama3.2:1b",
                88_200,
                0,
                &["denied", "slow"],
            ),
            turn("b", "what is 2+2", "llama3.2:1b", 3_000, 0, &[]),
            json!({"type":"feedback","turn":"a","kind":"bad","note":"never asked me"}).to_string(),
        ]
        .join("\n");
        let report = summarize(&log);
        for part in [
            "2 turns in 1 conversations",
            "slowest 88.2 s",
            "loading models",
            "reading prompts",
            "denied",
            "a permission rule refused an action",
            "whats using space on my disk",
            "/bad  whats using space on my disk  - never asked me",
        ] {
            assert!(report.contains(part), "missing {part:?} in:\n{report}");
        }
    }

    #[test]
    fn the_owners_time_is_not_jarvis_time() {
        let log = turn("a", "delete old.log", "qwen2.5:3b", 30_000, 25_000, &[]);
        let report = summarize(&log);
        assert!(report.contains("median 5.0 s"), "{report}");
        assert!(report.contains("waiting for you"), "{report}");
    }

    #[test]
    fn a_prompt_inside_a_tool_call_counts_as_the_owners_time() {
        // The tool's clock ran while the owner read its prompt.
        let log = json!({
            "type": "turn", "id": "a", "at": "2026-11-17T10:00:00Z", "session": "s1",
            "input": "delete old.log", "model": "", "signals": [],
            "timing": { "total_ms": 25_000, "you_ms": 24_000, "model": [],
                        "tools": [{ "name": "filesystem.delete", "ms": 25_000 }] }
        })
        .to_string();
        let report = summarize(&log);
        assert!(report.contains("running tools"), "{report}");
        assert!(report.contains("1.0 s  100%"), "{report}");
    }

    #[test]
    fn an_empty_log_says_what_to_do() {
        assert!(summarize("").contains("No turns logged yet"));
    }
}
