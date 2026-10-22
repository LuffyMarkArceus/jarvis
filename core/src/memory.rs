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
