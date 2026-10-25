//! Memory system - episodic memory with SQLite

use crate::config::Config;
use crate::error::{JarvisError, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryManager {
    conn: Arc<Mutex<Connection>>,
    config: Config,
}

impl MemoryManager {
    pub fn new(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.general.data_dir)
            .map_err(|e| JarvisError::Memory(format!("Failed to create data dir: {}", e)))?;

        let conn = Connection::open(&config.memory.db_path)
            .map_err(|e| JarvisError::Memory(format!("Failed to open db: {}", e)))?;

        let mgr = Self {
            conn: Arc::new(Mutex::new(conn)),
            config,
        };
        mgr.init_schema()?;
        Ok(mgr)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at);
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                session_id UNINDEXED,
                role UNINDEXED,
                content,
                tokenize = 'porter unicode61'
            );
            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, session_id, role, content)
                VALUES (new.id, new.session_id, new.role, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, session_id, role, content)
                VALUES('delete', old.id, old.session_id, old.role, old.content);
            END;
            CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, session_id, role, content)
                VALUES('delete', old.id, old.session_id, old.role, old.content);
                INSERT INTO messages_fts(rowid, session_id, role, content)
                VALUES (new.id, new.session_id, new.role, new.content);
            END;
            ",
        )
        .map_err(|e| JarvisError::Memory(format!("Schema init failed: {}", e)))?;
        Ok(())
    }

    pub fn create_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, title) VALUES (?1, ?2)",
            params![session_id, "New Session"],
        )
        .map_err(|e| JarvisError::Memory(format!("Failed to create session: {}", e)))?;
        Ok(())
    }

    /// Does this session still exist?
    ///
    /// Used when resuming the saved active session: the file outlives a
    /// `session delete`, so its id alone is not proof the conversation is
    /// still there.
    pub fn session_exists(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT 1 FROM sessions WHERE id = ?1")
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
        stmt.exists(params![session_id])
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))
    }

    pub fn add_message(&self, session_id: &str, role: &str, content: &str) -> Result<i64> {
        self.create_session(session_id)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content) VALUES (?1, ?2, ?3)",
            params![session_id, role, content],
        )
        .map_err(|e| JarvisError::Memory(format!("Failed to add message: {}", e)))?;

        // Update session timestamp
        let _ = conn.execute(
            "UPDATE sessions SET updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
            params![session_id],
        );

        Ok(conn.last_insert_rowid())
    }

    pub fn get_recent_messages(&self, session_id: &str, limit: usize) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, role, content, created_at 
                 FROM messages 
                 WHERE session_id = ?1 
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;

        let rows = stmt
            .query_map(params![session_id, limit as i64], |row| {
                let created_at_str: String = row.get(4)?;
                let created_at = parse_timestamp(&created_at_str);
                Ok(Message {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    created_at,
                })
            })
            .map_err(|e| JarvisError::Memory(format!("Query map failed: {}", e)))?;

        let mut messages = Vec::new();
        for msg in rows {
            messages.push(msg.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
        }
        // The query selects newest-first so LIMIT keeps the latest messages;
        // flip back to chronological order for callers.
        messages.reverse();
        Ok(messages)
    }

    pub fn get_db_path(&self) -> &PathBuf {
        &self.config.memory.db_path
    }
}

impl MemoryManager {
    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT s.id, s.title, s.created_at, s.updated_at, COUNT(m.id) as msg_count
                 FROM sessions s
                 LEFT JOIN messages m ON m.session_id = s.id
                 GROUP BY s.id
                 ORDER BY s.updated_at DESC
                 LIMIT ?1",
            )
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let created_at_str: String = row.get(2)?;
                let updated_at_str: String = row.get(3)?;
                let created_at = parse_timestamp(&created_at_str);
                let updated_at = parse_timestamp(&updated_at_str);
                Ok(SessionInfo {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    created_at,
                    updated_at,
                    message_count: row.get(4)?,
                })
            })
            .map_err(|e| JarvisError::Memory(format!("Query map failed: {}", e)))?;

        let mut sessions = Vec::new();
        for s in rows {
            sessions.push(s.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
        }
        Ok(sessions)
    }

    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            params![title, session_id],
        )
        .map_err(|e| JarvisError::Memory(format!("Failed to update title: {}", e)))?;
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(|e| JarvisError::Memory(format!("Failed to delete messages: {}", e)))?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .map_err(|e| JarvisError::Memory(format!("Failed to delete session: {}", e)))?;
        Ok(())
    }

    pub fn get_message_count(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM messages WHERE session_id = ?1")
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
        let count: i64 = stmt
            .query_row(params![session_id], |row| row.get(0))
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
        Ok(count)
    }

    pub fn get_all_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, role, content, created_at 
                 FROM messages 
                 WHERE session_id = ?1 
                 ORDER BY created_at ASC, id ASC",
            )
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                let created_at_str: String = row.get(4)?;
                let created_at = parse_timestamp(&created_at_str);
                Ok(Message {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    created_at,
                })
            })
            .map_err(|e| JarvisError::Memory(format!("Query map failed: {}", e)))?;

        let mut messages = Vec::new();
        for msg in rows {
            messages.push(msg.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
        }
        Ok(messages)
    }

    pub fn prune_empty_sessions(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT s.id FROM sessions s LEFT JOIN messages m ON m.session_id = s.id GROUP BY s.id HAVING COUNT(m.id) = 0",
            )
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;

        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;

        let mut to_delete: Vec<String> = Vec::new();
        for r in rows.flatten() {
            to_delete.push(r);
        }
        let count = to_delete.len();
        for id in to_delete {
            let _ = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id]);
        }
        Ok(count)
    }

    /// Backfill FTS for messages that exist but aren't indexed.
    pub fn backfill_fts(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT m.id, m.session_id, m.role, m.content FROM messages m
                 LEFT JOIN messages_fts f ON f.rowid = m.id
                 WHERE f.rowid IS NULL",
            )
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
        let mut items = Vec::new();
        for r in rows {
            items.push(r.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
        }
        let count = items.len();
        for (id, session_id, role, content) in items {
            let _ = conn.execute(
                "INSERT INTO messages_fts(rowid, session_id, role, content) VALUES (?1, ?2, ?3, ?4)",
                params![id, session_id, role, content],
            );
        }
        Ok(count)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub session_id: String,
    pub message_id: i64,
    pub role: String,
    pub content: String,
    pub snippet: String,
    pub rank: f64,
    pub created_at: DateTime<Utc>,
}

/// Words that carry no retrieval signal on their own.
///
/// FTS5 cannot know that "the" or "what" is noise, and an OR-query containing
/// them matches nearly every message, which pushes the useful result out of
/// the top `limit`.
const RECALL_STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by", "can", "could",
    "did", "do", "does", "for", "from", "get", "had", "has", "have", "how", "i", "if", "in",
    "into", "is", "it", "its", "just", "me", "my", "of", "on", "or", "our", "please", "should",
    "so", "some", "tell", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will",
    "with", "would", "you", "your",
];

/// Self-admissions that an answer was not sourced.
///
/// Remembering these teaches the model its own past failure: the string "as of
/// my knowledge cutoff, 1.54.0" was a wrong answer, and injecting it into a
/// later turn makes the next answer more likely to repeat it. Our own
/// grounding notes are included for the same reason.
const UNRELIABLE_MARKERS: &[&str] = &[
    "does not appear in the sources i read",
    "do not appear in the sources i read",
    "could not retrieve a source",
    "as of my knowledge cutoff",
];

/// The most terms one recall query may use.
const MAX_RECALL_TERMS: usize = 8;

/// The terms a recall query should match on.
///
/// A natural question is not an FTS5 query: punctuation is syntax there, and
/// stopwords dominate the match. The request is reduced to significant words,
/// each quoted so a word like `OR` cannot become an operator.
pub fn recall_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    // Hyphens and underscores stay inside a term: `rust-analyzer` and `ICE-9`
    // are single subjects, not two. FTS5 tokenises the quoted phrase itself.
    for raw in query.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
        let term = raw.trim_matches(|c: char| c == '-' || c == '_');
        let length = term.chars().count();
        // Short numbers still carry meaning in a version or an error code.
        let significant = length >= 3 || (length >= 2 && term.chars().all(|c| c.is_ascii_digit()));
        if !significant {
            continue;
        }
        let lower = term.to_lowercase();
        if RECALL_STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if !terms.iter().any(|seen| seen.to_lowercase() == lower) {
            terms.push(term.to_string());
        }
        // A pasted document has hundreds of terms. The expression only needs
        // enough to find related material, and FTS ranking degrades when every
        // word is OR-ed together.
        if terms.len() == MAX_RECALL_TERMS {
            break;
        }
    }
    terms
}

/// An FTS5 expression that matches any significant term.
fn fts_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Comparable form of a message: lowercase, alphanumeric words only.
///
/// Used for duplicate detection and for recognising the current request in
/// recall results. Punctuation carries no meaning for either job, and treating
/// `France?` and `France` as different would let duplicates through.
pub fn normalize_content(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Is this recalled message the current request itself?
///
/// Asking the same question again is the most common repeat in real use, and
/// the FTS match for it is the question from older sessions. Handing that back
/// as "memory" tells the model nothing and spends prompt budget, so it is
/// dropped. Only an identical request is dropped: a stored answer to it is
/// still context.
pub fn is_echo(result: &RecallResult, input: &str) -> bool {
    if result.role != "user" {
        return false;
    }
    normalize_content(&result.content) == normalize_content(input)
}

/// Answers that said they had no source are not worth remembering.
fn is_unreliable(result: &RecallResult) -> bool {
    if result.role != "assistant" {
        return false;
    }
    let lower = result.content.to_lowercase();
    UNRELIABLE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Render recalled messages as a bounded block for the prompt.
///
/// Returns `None` when there is nothing worth adding. The block is labelled
/// as earlier material so the model does not read it as the current request.
pub fn render_recall(results: &[RecallResult], max_chars: usize) -> Option<String> {
    if results.is_empty() || max_chars == 0 {
        return None;
    }
    let mut block = String::from(
        "Earlier exchanges from previous sessions, for context only (not part of the current request):\n",
    );
    for result in results {
        let content = result
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let content = truncate_chars(&content, 240);
        let line = format!(
            "- {} ({}): {}\n",
            format_relative_time(&result.created_at),
            result.role,
            content
        );
        if block.len() + line.len() > max_chars {
            break;
        }
        block.push_str(&line);
    }
    if block.lines().count() <= 1 {
        return None;
    }
    Some(block)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", cut.trim_end())
}

impl MemoryManager {
    pub fn stats(&self) -> Result<MemoryStats> {
        let conn = self.conn.lock().unwrap();
        let total_messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap_or(0);
        let total_sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap_or(0);
        drop(conn);
        let db_size_bytes = std::fs::metadata(&self.config.memory.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(MemoryStats {
            total_messages,
            total_sessions,
            db_size_bytes,
        })
    }

    /// Recall relevant messages across all sessions using FTS5.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<RecallResult>> {
        self.recall_in(query, limit, None)
    }

    /// Recall from every session except `exclude_session`.
    ///
    /// The current session is excluded when recall feeds a turn, because its
    /// recent messages are already in the prompt; repeating them spends the
    /// budget on text the model has just read.
    pub fn recall_in(
        &self,
        query: &str,
        limit: usize,
        exclude_session: Option<&str>,
    ) -> Result<Vec<RecallResult>> {
        let limit = limit.max(1);
        let terms = recall_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock().unwrap();
        // Over-fetch: deduplication and the unreliable-answer filter both drop
        // rows, and the caller asked for `limit` useful ones.
        let fetch = (limit * 4).max(limit) as i64;
        let expression = fts_query(&terms);

        let sql = "SELECT f.session_id, f.role, f.content, f.rowid, f.rank,
                          snippet(messages_fts, 2, '', '', ' ... ', 40) as snip,
                          m.created_at
                   FROM messages_fts f
                   JOIN messages m ON m.id = f.rowid
                   WHERE messages_fts MATCH ?1
                     AND (?2 IS NULL OR f.session_id != ?2)
                   ORDER BY f.rank
                   LIMIT ?3";

        // A prepare or step failure (corrupt index, unknown function) falls
        // back to substring search. The statement is collected inside the
        // match because the row iterator borrows it.
        let mut results: Vec<RecallResult> = Vec::new();
        let outcome = match conn.prepare(sql) {
            Ok(mut stmt) => {
                let rows =
                    stmt.query_map(params![expression, exclude_session, fetch], row_to_recall);
                match rows {
                    Ok(rows) => {
                        let mut failed = false;
                        for row in rows {
                            match row {
                                Ok(result) => results.push(result),
                                Err(_) => {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                        if failed {
                            Err(())
                        } else {
                            Ok(())
                        }
                    }
                    Err(_) => Err(()),
                }
            }
            Err(_) => Err(()),
        };
        if outcome.is_err() {
            return self.like_fallback(&conn, &terms, limit as i64, exclude_session);
        }

        Ok(self.curate(results, limit, exclude_session))
    }

    /// Substring search used only when FTS cannot run (a corrupt or missing
    /// index). It matches the first significant term, not the whole question,
    /// which would almost never appear verbatim.
    fn like_fallback(
        &self,
        conn: &Connection,
        terms: &[String],
        limit: i64,
        exclude_session: Option<&str>,
    ) -> Result<Vec<RecallResult>> {
        let term = terms.first().cloned().unwrap_or_default();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, role, content, id, 0.0 as rank, content, created_at
                 FROM messages
                 WHERE content LIKE '%' || ?1 || '%'
                   AND (?2 IS NULL OR session_id != ?2)
                 ORDER BY created_at DESC
                 LIMIT ?3",
            )
            .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
        let rows = stmt
            .query_map(params![term, exclude_session, limit], row_to_recall)
            .map_err(|e| JarvisError::Memory(format!("Query map failed: {}", e)))?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
        }
        Ok(self.curate(results, limit as usize, exclude_session))
    }

    /// Drop duplicates, unreliable answers and the current session, preserving
    /// FTS ranking, then cap to `limit`.
    fn curate(
        &self,
        results: Vec<RecallResult>,
        limit: usize,
        exclude_session: Option<&str>,
    ) -> Vec<RecallResult> {
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for result in results {
            if result.role != "user" && result.role != "assistant" {
                continue;
            }
            if exclude_session.is_some_and(|id| result.session_id == id) {
                continue;
            }
            if is_unreliable(&result) {
                continue;
            }
            let key = normalize_content(&result.content);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(result);
            if out.len() == limit {
                break;
            }
        }
        out
    }
}

fn row_to_recall(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecallResult> {
    let created_at: String = row.get(6)?;
    Ok(RecallResult {
        session_id: row.get(0)?,
        role: row.get(1)?,
        content: row.get(2)?,
        message_id: row.get(3)?,
        rank: row.get(4)?,
        snippet: row.get(5)?,
        created_at: parse_timestamp(&created_at),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_messages: i64,
    pub total_sessions: i64,
    pub db_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: i64,
}

/// Parse a stored timestamp.
///
/// SQLite's `CURRENT_TIMESTAMP` writes `YYYY-MM-DD HH:MM:SS` in UTC, which is
/// not RFC3339. Accepting both formats keeps relative ages honest: a memory
/// from last month must not read as "just now".
fn parse_timestamp(text: &str) -> DateTime<Utc> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return parsed.with_timezone(&Utc);
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return naive.and_utc();
        }
    }
    Utc::now()
}

pub fn format_relative_time(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(*dt);
    if diff.num_seconds() < 60 {
        "just now".to_string()
    } else if diff.num_minutes() < 60 {
        format!("{}m ago", diff.num_minutes())
    } else if diff.num_hours() < 24 {
        format!("{}h ago", diff.num_hours())
    } else if diff.num_days() < 7 {
        format!("{}d ago", diff.num_days())
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn recall_result(role: &str, content: &str) -> RecallResult {
        RecallResult {
            session_id: "s1".to_string(),
            message_id: 1,
            role: role.to_string(),
            content: content.to_string(),
            snippet: content.to_string(),
            rank: 1.0,
            created_at: Utc::now(),
        }
    }

    // ---------------------------------------------------------------- terms

    #[test]
    fn recall_terms_drop_question_words_and_short_words() {
        assert_eq!(
            recall_terms("what is the latest version of the rust programming language"),
            vec!["latest", "version", "rust", "programming", "language"]
        );
    }

    #[test]
    fn recall_terms_split_on_punctuation_but_keep_identifiers() {
        // A raw question is not a valid FTS5 expression: "?" and "," would be
        // syntax. Hyphenated identifiers stay whole.
        assert_eq!(
            recall_terms("did I fix the ICE-9 bug, or not?"),
            vec!["fix", "ICE-9", "bug", "not"]
        );
        assert_eq!(
            recall_terms("upgrade rust-analyzer to 1.99"),
            vec!["upgrade", "rust-analyzer", "99"]
        );
    }

    #[test]
    fn a_two_digit_number_survives_but_a_two_letter_word_does_not() {
        assert_eq!(recall_terms("error 42"), vec!["error", "42"]);
        assert_eq!(recall_terms("go to it"), Vec::<String>::new());
    }

    #[test]
    fn recall_terms_deduplicate_ignoring_case() {
        assert_eq!(recall_terms("Rust rust RUST"), vec!["Rust"]);
    }

    #[test]
    fn a_long_paste_is_bounded_to_a_few_terms() {
        let paste = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
        let terms = recall_terms(paste);
        assert_eq!(terms.len(), MAX_RECALL_TERMS);
        assert_eq!(terms[0], "alpha");
    }

    #[test]
    fn a_request_of_only_stopwords_has_no_terms() {
        assert!(recall_terms("what is it").is_empty());
        assert!(recall_terms("   ").is_empty());
    }

    #[test]
    fn the_fts_expression_quotes_every_term() {
        let terms = vec!["rust".to_string(), "OR".to_string(), "version".to_string()];
        assert_eq!(fts_query(&terms), "\"rust\" OR \"OR\" OR \"version\"");
    }

    #[test]
    fn a_quote_in_a_term_cannot_escape_the_quoting() {
        let terms = vec!["ru\"st".to_string()];
        assert_eq!(fts_query(&terms), "\"rust\"");
    }

    // ------------------------------------------------------------ filtering

    #[test]
    fn an_unsourced_assistant_answer_is_not_recalled() {
        let result = recall_result("assistant", "As of my knowledge cutoff, Rust is 1.54.0.");
        assert!(is_unreliable(&result));
    }

    #[test]
    fn our_own_grounding_note_marks_an_answer_unreliable() {
        let result = recall_result(
            "assistant",
            "It is 1.64.\n\nNote: 1.64 does not appear in the sources I read, so treat it with caution.",
        );
        assert!(is_unreliable(&result));
    }

    #[test]
    fn a_user_message_is_never_filtered_as_unreliable() {
        let result = recall_result("user", "you said as of my knowledge cutoff last time");
        assert!(!is_unreliable(&result));
    }

    #[test]
    fn a_repeated_question_is_an_echo_not_memory() {
        let result = recall_result("user", "What is the capital of France?");
        assert!(is_echo(&result, "what is the capital of france"));
        assert!(is_echo(&result, "  What is the capital of France?  "));
    }

    #[test]
    fn a_question_that_merely_shares_words_is_not_an_echo() {
        let result = recall_result("user", "what is the capital of france");
        assert!(!is_echo(&result, "what is the capital of germany"));
    }

    #[test]
    fn a_stored_answer_to_the_same_question_is_still_context() {
        let result = recall_result("assistant", "The capital of France is Paris.");
        assert!(!is_echo(&result, "what is the capital of france"));
    }

    #[test]
    fn a_sourced_answer_is_worth_recalling() {
        let result = recall_result("assistant", "The latest stable version of Rust is 1.99.1.");
        assert!(!is_unreliable(&result));
    }

    // -------------------------------------------------------------- render

    #[test]
    fn rendering_labels_the_block_as_earlier_context() {
        let block = render_recall(&[recall_result("user", "my name is Shrey")], 600)
            .expect("a non-empty block");
        assert!(block.contains("previous sessions"));
        assert!(block.contains("user"));
        assert!(block.contains("my name is Shrey"));
        assert!(block.contains("not part of the current request"));
    }

    #[test]
    fn rendering_nothing_is_none() {
        assert!(render_recall(&[], 600).is_none());
    }

    #[test]
    fn rendering_respects_the_character_budget() {
        let results: Vec<RecallResult> = (0..5)
            .map(|i| recall_result("user", &format!("{}{}", "x".repeat(200), i)))
            .collect();
        let block = render_recall(&results, 320).expect("a non-empty block");
        assert!(block.len() <= 320, "block was {} chars", block.len());
        // One over-long line would blow the budget, so the whole entry is
        // dropped rather than half-written.
        assert!(block.lines().count() > 1);
    }

    #[test]
    fn rendering_truncates_an_over_long_message() {
        let long = "a".repeat(500);
        let block = render_recall(&[recall_result("user", &long)], 1000).expect("a block");
        assert!(block.contains("..."));
    }

    // ------------------------------------------------------------ timestamps

    #[test]
    fn a_sqlite_timestamp_is_parsed_not_replaced_with_now() {
        // CURRENT_TIMESTAMP format, not RFC3339.
        let parsed = parse_timestamp("2020-01-02 03:04:05");
        assert_eq!(
            parsed.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2020-01-02 03:04:05"
        );
    }

    #[test]
    fn an_rfc3339_timestamp_is_also_accepted() {
        let parsed = parse_timestamp("2020-01-02T03:04:05Z");
        assert_eq!(parsed.format("%Y-%m-%d").to_string(), "2020-01-02");
    }

    // ------------------------------------------------------------- database

    fn temp_manager() -> (MemoryManager, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("jarvis-memory-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut config = Config::default();
        config.general.data_dir = dir.clone();
        config.memory.db_path = dir.join("memory.db");
        let manager = MemoryManager::new(config).expect("manager");
        (manager, dir)
    }

    #[test]
    fn recall_finds_a_message_from_another_session() {
        let (manager, dir) = temp_manager();
        manager.create_session("old").unwrap();
        manager
            .add_message("old", "user", "my project lives in /mnt/storage/CS/jarvis")
            .unwrap();

        let found = manager
            .recall_in("where does my project live", 5, Some("current"))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].content.contains("/mnt/storage/CS/jarvis"));

        drop(manager);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recall_excludes_the_current_session() {
        let (manager, dir) = temp_manager();
        manager.create_session("current").unwrap();
        manager
            .add_message("current", "user", "the deployment password is hunter2")
            .unwrap();

        // Only the current session has this text, so excluding it must leave
        // nothing - the recent window already carries those messages.
        let found = manager
            .recall_in("deployment password", 5, Some("current"))
            .unwrap();
        assert!(found.is_empty());

        drop(manager);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recall_deduplicates_identical_messages() {
        let (manager, dir) = temp_manager();
        manager.create_session("a").unwrap();
        manager.create_session("b").unwrap();
        for session in ["a", "b"] {
            manager
                .add_message(
                    session,
                    "user",
                    "remember the staging host is db-staging.internal",
                )
                .unwrap();
        }

        let found = manager.recall_in("staging host", 5, None).unwrap();
        assert_eq!(found.len(), 1, "identical text appeared twice");

        drop(manager);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recall_skips_an_unsourced_answer_but_keeps_the_question() {
        let (manager, dir) = temp_manager();
        manager.create_session("old").unwrap();
        manager
            .add_message("old", "user", "what version of rust is current")
            .unwrap();
        manager
            .add_message(
                "old",
                "assistant",
                "As of my knowledge cutoff, the current rust version is 1.54.0.",
            )
            .unwrap();

        let found = manager.recall_in("current rust version", 5, None).unwrap();
        assert!(found.iter().any(|r| r.role == "user"));
        assert!(
            !found.iter().any(|r| r.role == "assistant"),
            "a self-admitted-unsourced answer was recalled"
        );

        drop(manager);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recall_finds_a_multi_word_question_by_any_term() {
        let (manager, dir) = temp_manager();
        manager.create_session("old").unwrap();
        manager
            .add_message("old", "user", "the ceremony is in Reykjavik this year")
            .unwrap();

        // Natural questions are OR-of-terms, so an unrelated word does not
        // make the whole query miss.
        let found = manager
            .recall_in("when is the ceremony happening", 5, None)
            .unwrap();
        assert_eq!(found.len(), 1);

        drop(manager);
        let _ = std::fs::remove_dir_all(dir);
    }
}
