use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub model: ModelConfig,
    pub memory: MemoryConfig,
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub name: String,
    pub log_level: String,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub base_url: String,
    pub default_model: String,
    pub routing_model: String,
    pub embedding_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub db_path: PathBuf,
    pub vector_dim: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub require_confirmation: bool,
    pub allowed_shell_commands: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("jarvis");
        Self {
            general: GeneralConfig {
                name: "Jarvis".to_string(),
                log_level: "info".to_string(),
                data_dir: data_dir.clone(),
            },
            model: ModelConfig {
                provider: "ollama".to_string(),
                base_url: "http://127.0.0.1:11434".to_string(),
                default_model: "llama3.2:1b".to_string(),
                routing_model: "llama3.2:1b".to_string(),
                embedding_model: "nomic-embed-text".to_string(),
            },
            memory: MemoryConfig {
                db_path: data_dir.join("memory.db"),
                vector_dim: 768,
            },
            tools: ToolsConfig {
                require_confirmation: true,
                allowed_shell_commands: vec![],
            },
        }
    }
}
