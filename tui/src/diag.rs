//! `tui.log`: what this window did, one line per event - connections,
//! requests and how long their answers took, prompts, anything that could not
//! be sent. Real use: a turn "did not respond" and nothing recorded what the
//! window had sent. Never what was typed.

use crate::client::ClientEvent;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant;
use uuid::Uuid;

/// Past this size the log moves to `tui.1.log`, replacing the older one.
const ROTATE_BYTES: u64 = 1024 * 1024;

pub struct Diag {
    path: PathBuf,
    sent: HashMap<Uuid, (&'static str, Instant)>,
}

impl Diag {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Diag {
            path: data_dir.join("tui.log"),
            sent: HashMap::new(),
        }
    }

    /// Append one line. A log that cannot be written must not stop the UI.
    pub fn line(&self, text: &str) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
                std::fs::rename(&self.path, self.path.with_file_name("tui.1.log"))?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            writeln!(
                file,
                "{} pid={} {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                std::process::id(),
                text
            )
        };
        let _ = write();
    }

    pub fn sent(&mut self, id: Uuid, method: &'static str) {
        self.sent.insert(id, (method, Instant::now()));
        self.line(&format!("sent {method} {}", short(id)));
    }

    /// What arrived from the daemon. Chunks and progress are too frequent to
    /// log one by one; the answer that ends them is.
    pub fn event(&mut self, event: &ClientEvent) {
        match event {
            ClientEvent::Connected => self.line("connected"),
            ClientEvent::Disconnected(reason) => self.line(&format!("disconnected: {reason}")),
            ClientEvent::Confirmation(request) => {
                self.line(&format!("prompt shown: {}", request.tool))
            }
            ClientEvent::Response(response) => {
                let (method, ms) = match self.sent.remove(&response.id) {
                    Some((method, at)) => (method, at.elapsed().as_millis()),
                    None => ("unknown", 0),
                };
                self.line(&format!(
                    "answered {method} {} {} in {ms} ms",
                    short(response.id),
                    if response.ok { "ok" } else { "error" }
                ));
            }
            ClientEvent::Chunk { .. } | ClientEvent::Progress { .. } | ClientEvent::Step(_) => {}
        }
    }
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_common::ipc::Response;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-diag-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_request_and_its_answer_are_logged_with_how_long_it_took() {
        let dir = temp();
        let mut diag = Diag::new(&dir);
        let id = Uuid::new_v4();
        diag.sent(id, "ask");
        diag.event(&ClientEvent::Response(Response::ok(
            id,
            serde_json::json!({}),
        )));
        diag.event(&ClientEvent::Disconnected(
            "the daemon closed the connection".into(),
        ));
        let log = std::fs::read_to_string(dir.join("tui.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 3, "{log}");
        assert!(lines[0].contains("sent ask"), "{log}");
        assert!(
            lines[1].contains("answered ask") && lines[1].contains(" ok in "),
            "{log}"
        );
        assert!(
            lines[2].contains("disconnected: the daemon closed"),
            "{log}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_full_log_moves_aside() {
        let dir = temp();
        std::fs::write(dir.join("tui.log"), vec![b'x'; (ROTATE_BYTES + 1) as usize]).unwrap();
        Diag::new(&dir).line("fresh");
        assert!(dir.join("tui.1.log").exists());
        let now = std::fs::read_to_string(dir.join("tui.log")).unwrap();
        assert!(now.contains("fresh") && now.len() < 200);
        let _ = std::fs::remove_dir_all(dir);
    }
}
