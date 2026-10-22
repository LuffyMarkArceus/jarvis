use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub model: ModelConfig,
    pub memory: MemoryConfig,
    pub tools: ToolsConfig,
    #[serde(default)]
    pub context: ContextConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub max_context_chars: usize,
    pub max_history_messages: usize,
    pub max_history_fetch: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_context_chars: 2000,
            max_history_messages: 20,
            max_history_fetch: 50,
        }
    }
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
            context: ContextConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let mut config = Config::default();

        // Load from config file if exists
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("jarvis");
        let config_path = config_dir.join("config.toml");
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(file_config) = toml::from_str::<Config>(&content) {
                    config = file_config;
                    // Recompute db_path if using default data_dir structure
                    let data_dir = config.general.data_dir.clone();
                    if config.memory.db_path.to_string_lossy().is_empty() {
                        config.memory.db_path = data_dir.join("memory.db");
                    }
                }
            }
        }

        // Override with environment variables
        if let Ok(val) = std::env::var("JARVIS_MODEL") {
            if !val.is_empty() {
                config.model.default_model = val;
            }
        }
        if let Ok(val) = std::env::var("JARVIS_BASE_URL") {
            if !val.is_empty() {
                config.model.base_url = val;
            }
        }
        if let Ok(val) = std::env::var("JARVIS_MAX_CONTEXT_CHARS") {
            if let Ok(v) = val.parse::<usize>() {
                config.context.max_context_chars = v;
            }
        }
        if let Ok(val) = std::env::var("JARVIS_MAX_HISTORY_FETCH") {
            if let Ok(v) = val.parse::<usize>() {
                config.context.max_history_fetch = v;
            }
        }

        config
    }
}
