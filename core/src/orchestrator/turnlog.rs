//! Measuring real use (Milestone 6.4): one line per turn in
//! `<data_dir>/turns.jsonl` - where the time went and what went wrong - and
//! the owner's `/bad` and `/good`. `jarvis report` reads it.
//!
//! Append-only JSON lines, not a table: readable with any editor or `jq`, no
//! schema to migrate, and the data folder is hidden from sandboxed commands.

use super::*;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A turn this slow (without the owner's time) is friction by itself.
pub(super) const SLOW_MS: u64 = 20_000;
/// Asked again this soon, in other words: the first answer likely failed.
const REPHRASE_WITHIN: Duration = Duration::from_secs(60);
/// Past this size the log moves to `turns.1.jsonl`, replacing the older one.
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Times the owner's answers to prompts: that time is theirs, not Jarvis's,
/// and must not make a turn look slow.
pub(super) struct TimedConfirm<'a> {
    inner: &'a dyn ConfirmationHandler,
    waited: std::sync::Mutex<Duration>,
}

impl<'a> TimedConfirm<'a> {
    pub(super) fn new(inner: &'a dyn ConfirmationHandler) -> Self {
        TimedConfirm {
            inner,
            waited: std::sync::Mutex::new(Duration::ZERO),
        }
    }

    pub(super) fn waited_ms(&self) -> u64 {
        self.waited.lock().unwrap().as_millis() as u64
    }

    fn add(&self, since: Instant) {
        *self.waited.lock().unwrap() += since.elapsed();
    }
}

#[async_trait::async_trait]
impl ConfirmationHandler for TimedConfirm<'_> {
    async fn confirm(&self, tool: &str, args: &Value) -> bool {
        let started = Instant::now();
        let answer = self.inner.confirm(tool, args).await;
        self.add(started);
        answer
    }

    // Forwarded, not left to the default: a handler's own `choose` (with
    // its choices) must stay in force.
    async fn choose(&self, tool: &str, args: &Value, choices: &[Choice]) -> Option<char> {
        let started = Instant::now();
        let answer = self.inner.choose(tool, args, choices).await;
        self.add(started);
        answer
    }

    async fn answer(&self, tool: &str, args: &Value, choices: &[Choice]) -> Answer {
        let started = Instant::now();
        let answer = self.inner.answer(tool, args, choices).await;
        self.add(started);
        answer
    }
}

/// The words that carry a request's meaning.
pub(super) fn meaning_words(text: &str) -> HashSet<String> {
    const FILLER: &[&str] = &[
        "the", "and", "with", "please", "can", "you", "for", "that", "this", "what", "how", "are",
        "is", "was", "its", "into", "from", "your", "my", "me", "could", "would", "will", "just",
        "again", "about",
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !FILLER.contains(w))
        .map(String::from)
        .collect()
}

/// The same request in other words: most of the shorter one's words recur.
pub(super) fn rephrases(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    let shared = a.intersection(b).count();
    let shorter = a.len().min(b.len());
    shorter > 0 && shared >= 2 && shared * 10 >= shorter * 6
}

/// What went wrong in a turn, in a fixed vocabulary `jarvis report` counts.
pub(super) fn signals(report: &TurnReport) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let mut add = |s: &'static str| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for call in &report.calls {
        match call.status {
            "declined" => add("declined"),
            "denied" => add("denied"),
            "untrusted" => add("untrusted"),
            "withheld" => add("withheld"),
            "failed" | "not_offered" => add("failed"),
            "needs_confirmation" => add("not_asked"),
            "deferred" => add("deferred"),
            _ => {}
        }
    }
    if report.withheld {
        add("withheld");
    }
    if report.model_error.is_some() {
        add("model_error");
    }
    if report.timing.jarvis_ms() > SLOW_MS {
        add("slow");
    }
    out
}

fn excerpt(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

impl Orchestrator {
    /// A turn's signals, with "queued" when it waited for the GPU longer
    /// than a spill would have waited.
    pub(super) fn turn_signals(&self, report: &TurnReport) -> Vec<&'static str> {
        let mut found = signals(report);
        let spill_after = self.config.remote.as_ref().map_or(5, |r| r.spill_after);
        if report.timing.queue_ms > spill_after * 1000 {
            found.push("queued");
        }
        if report.spilled {
            found.push("spilled");
        }
        if report.spill_withheld {
            found.push("spill_withheld");
        }
        if report.plan_refused {
            found.push("plan_refused");
        }
        found
    }

    fn turn_log_path(&self) -> std::path::PathBuf {
        self.config.general.data_dir.join("turns.jsonl")
    }

    /// Append one line. Measuring must never break a turn: failures are
    /// logged and dropped.
    fn append_turn_log(&self, line: &Value) {
        // Turns run at once (6.5b): one writer at a time, so a rotation
        // cannot race an append and no line is torn.
        static TURN_LOG: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _append = TURN_LOG.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.turn_log_path();
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
                std::fs::rename(&path, path.with_file_name("turns.1.jsonl"))?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(file, "{}", line)
        };
        if let Err(e) = write() {
            warn!("could not write the turn log {}: {}", path.display(), e);
        }
    }

    /// Log a finished, cancelled or failed turn, and remember it for `/bad`
    /// and for spotting a rephrase.
    pub(super) fn log_turn(
        &self,
        conv: &Conversation,
        input: &str,
        report: &TurnReport,
        mut signals: Vec<&'static str>,
    ) {
        let words = meaning_words(input);
        let id = Uuid::new_v4().to_string();
        {
            let last = conv.last_turn.lock().unwrap();
            if let Some(last) = last.as_ref() {
                if last.ended.elapsed() <= REPHRASE_WITHIN && rephrases(&last.words, &words) {
                    signals.push("rephrased");
                }
            }
        }
        let calls: Vec<Value> = report
            .calls
            .iter()
            .map(|c| json!({ "name": c.name, "status": c.status }))
            .collect();
        self.append_turn_log(&json!({
            "type": "turn",
            "id": id,
            "at": chrono::Utc::now().to_rfc3339(),
            "session": conv.id,
            "input": excerpt(input, 300),
            "route": report.route,
            "model": report.model,
            "provider": report.provider,
            "remote": report.remote,
            "calls": calls,
            "answer": excerpt(&report.answer, 300),
            "timing": report.timing,
            "signals": signals,
        }));
        *conv.last_turn.lock().unwrap() = Some(LastTurn {
            id,
            ended: Instant::now(),
            words,
        });
    }

    /// `/bad` or `/good` about the last turn of this conversation.
    pub fn feedback(&self, conv: &Conversation, kind: &str, note: &str) -> Result<()> {
        let turn = {
            let last = conv.last_turn.lock().unwrap();
            match last.as_ref() {
                Some(t) => t.id.clone(),
                _ => {
                    return Err(JarvisError::Config(
                        "There is no turn in this conversation yet to give feedback on.".into(),
                    ))
                }
            }
        };
        self.append_turn_log(&json!({
            "type": "feedback",
            "at": chrono::Utc::now().to_rfc3339(),
            "session": conv.id,
            "turn": turn,
            "kind": kind,
            "note": note.trim(),
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rephrase_shares_most_of_the_shorter_requests_words() {
        let a = meaning_words("create a file called notes.md containing the word hi");
        let b = meaning_words("please create notes.md with the word hi in it");
        assert!(rephrases(&a, &b));
        let c = meaning_words("what is the weather in Paris?");
        assert!(!rephrases(&a, &c));
        // One shared word is a coincidence, not a rephrase.
        // The boundary: 3 of the shorter request's 5 words is a rephrase,
        // 2 of 5 is not.
        let set = |w: &[&str]| w.iter().map(|x| x.to_string()).collect::<HashSet<_>>();
        let five = set(&["alpha", "bravo", "charlie", "delta", "echo"]);
        assert!(rephrases(
            &five,
            &set(&["alpha", "bravo", "charlie", "x1", "x2", "x3"])
        ));
        assert!(!rephrases(
            &five,
            &set(&["alpha", "bravo", "x1", "x2", "x3", "x4"])
        ));
        let d = meaning_words("delete notes");
        let e = meaning_words("show notes");
        assert!(!rephrases(&d, &e));
    }
}
