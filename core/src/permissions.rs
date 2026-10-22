//! Permissions and audit system

use crate::config::Config;
use crate::error::{JarvisError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    pub action: PermissionAction,
    pub patterns: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default = "default_rules")]
    pub rules: Vec<PermissionRule>,
}

fn default_rules() -> Vec<PermissionRule> {
    vec![
        PermissionRule {
            tool: "filesystem.list_dir".to_string(),
            action: PermissionAction::Allow,
            patterns: None,
        },
        PermissionRule {
            tool: "filesystem.read".to_string(),
            action: PermissionAction::Allow,
            patterns: None,
        },
        PermissionRule {
            tool: "system.info".to_string(),
            action: PermissionAction::Allow,
            patterns: None,
        },
        PermissionRule {
            tool: "shell.run".to_string(),
            action: PermissionAction::Ask,
            patterns: None,
        },
        PermissionRule {
            tool: "filesystem.write".to_string(),
            action: PermissionAction::Ask,
            patterns: None,
        },
        PermissionRule {
            tool: "filesystem.edit".to_string(),
            action: PermissionAction::Ask,
            patterns: None,
        },
    ]
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            rules: default_rules(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionManager {
    config: PermissionsConfig,
    config_path: PathBuf,
    audit_conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub session_id: Option<String>,
    pub tool: String,
    pub args: Value,
    pub decision: String,
    pub result: Option<String>,
    pub timestamp: String,
}

impl PermissionManager {
    pub fn new(config: &Config) -> Result<Self> {
        std::fs::create_dir_all(&config.general.data_dir).map_err(JarvisError::Io)?;

        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("jarvis");
        std::fs::create_dir_all(&config_dir).map_err(JarvisError::Io)?;

        let config_path = config_dir.join("permissions.toml");
        let permissions_config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).map_err(JarvisError::Io)?;
            toml::from_str(&content).unwrap_or_default()
        } else {
            let default_cfg = PermissionsConfig::default();
            let toml_str = toml::to_string_pretty(&default_cfg).unwrap_or_default();
            let _ = std::fs::write(&config_path, toml_str);
            default_cfg
        };

        let db_path = config.general.data_dir.join("audit.db");
        let conn = Connection::open(&db_path).map_err(|e| JarvisError::Memory(e.to_string()))?;
        let mgr = Self {
            config: permissions_config,
            config_path,
            audit_conn: Arc::new(Mutex::new(conn)),
        };
        mgr.init_audit_schema()?;
        Ok(mgr)
    }

    fn init_audit_schema(&self) -> Result<()> {
        let conn = self.audit_conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tool_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                decision TEXT NOT NULL,
                result TEXT,
                timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_audit_tool ON tool_audit(tool);
            CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON tool_audit(timestamp);
            ",
        )
        .map_err(|e| JarvisError::Memory(e.to_string()))?;
        Ok(())
    }

    pub fn check_permission(&self, tool: &str, _args: &Value) -> PermissionAction {
        // Check for exact match first
        for rule in &self.config.rules {
            if rule.tool == tool {
                return rule.action.clone();
            }
        }
        // Default: ask for unknown tools (safe by default)
        PermissionAction::Ask
    }

    pub fn log_audit(
        &self,
        session_id: Option<&str>,
        tool: &str,
        args: &Value,
        decision: &str,
        result: Option<&str>,
    ) -> Result<()> {
        let conn = self.audit_conn.lock().unwrap();
        let args_str = serde_json::to_string(args).unwrap_or_default();
        conn.execute(
            "INSERT INTO tool_audit (session_id, tool, args, decision, result) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, tool, args_str, decision, result],
        )
        .map_err(|e| JarvisError::Memory(e.to_string()))?;
        Ok(())
    }

    pub fn get_audit_log(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.audit_conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, tool, args, decision, result, timestamp FROM tool_audit ORDER BY timestamp DESC LIMIT ?1",
            )
            .map_err(|e| JarvisError::Memory(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let args_str: String = row.get(3)?;
                let args: Value = serde_json::from_str(&args_str).unwrap_or(Value::Null);
                Ok(AuditEntry {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    tool: row.get(2)?,
                    args,
                    decision: row.get(4)?,
                    result: row.get(5)?,
                    timestamp: row.get(6)?,
                })
            })
            .map_err(|e| JarvisError::Memory(e.to_string()))?;
        let mut entries = Vec::new();
        for e in rows {
            entries.push(e.map_err(|e| JarvisError::Memory(e.to_string()))?);
        }
        Ok(entries)
    }

    pub fn get_config_path(&self) -> &PathBuf {
        &self.config_path
    }
}
