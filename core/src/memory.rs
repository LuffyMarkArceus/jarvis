//! Memory system - episodic memory with SQLite

use crate::config::Config;
use crate::error::{JarvisError, Result};
use chrono::{DateTime, Utc};
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
                let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                    .unwrap_or_else(|_| Utc::now().into())
                    .with_timezone(&Utc);
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
                let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                    .unwrap_or_else(|_| Utc::now().into())
                    .with_timezone(&Utc);
                let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
                    .unwrap_or_else(|_| Utc::now().into())
                    .with_timezone(&Utc);
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
                let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                    .unwrap_or_else(|_| Utc::now().into())
                    .with_timezone(&Utc);
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
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        // Use FTS5 match with porter stemming; fall back to LIKE if query is invalid
        let mut stmt = match conn.prepare(
            "SELECT session_id, role, content, rowid, rank,
                    snippet(messages_fts, 2, '«', '»', ' ... ', 40) as snip
             FROM messages_fts
             WHERE messages_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => {
                // Fallback to LIKE search
                let mut stmt = conn
                    .prepare(
                        "SELECT session_id, role, content, id, 0.0 as rank, content as snip
                         FROM messages
                         WHERE content LIKE '%' || ?1 || '%'
                         ORDER BY created_at DESC
                         LIMIT ?2",
                    )
                    .map_err(|e| JarvisError::Memory(format!("Query failed: {}", e)))?;
                let rows = stmt
                    .query_map(params![query, limit as i64], |row| {
                        let content: String = row.get(2)?;
                        let snip = content.chars().take(120).collect::<String>();
                        Ok(RecallResult {
                            session_id: row.get(0)?,
                            role: row.get(1)?,
                            content,
                            snippet: snip,
                            rank: row.get(4)?,
                            message_id: row.get(3)?,
                        })
                    })
                    .map_err(|e| JarvisError::Memory(format!("Query map failed: {}", e)))?;
                let mut results = Vec::new();
                for r in rows {
                    results.push(r.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
                }
                return Ok(results);
            }
        };
        let rows = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok(RecallResult {
                    session_id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    message_id: row.get(3)?,
                    rank: row.get(4)?,
                    snippet: row.get(5)?,
                })
            })
            .map_err(|e| JarvisError::Memory(format!("Query map failed: {}", e)))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| JarvisError::Memory(format!("Row error: {}", e)))?);
        }
        Ok(results)
    }
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
