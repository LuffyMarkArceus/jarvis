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
                 ORDER BY created_at ASC, id ASC 
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
