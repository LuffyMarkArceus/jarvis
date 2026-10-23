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
    /// Use the provider's native function-calling interface when available.
    ///
    /// When true, tool schemas are sent with the request and structured
    /// `tool_calls` are expected back. When false, the orchestrator falls
    /// back to parsing tool calls out of the model's text.
    #[serde(default = "default_true")]
    pub native_tool_calling: bool,
    /// Sampling temperature.
    ///
    /// Defaults to 0 (greedy). An agent that answers differently for the
    /// same question is impossible to test and impossible to trust, and
    /// small models drift into refusals or fabrications at higher
    /// temperatures.
    #[serde(default)]
    pub temperature: f32,
}

fn default_true() -> bool {
    true
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
#[serde(default)]
pub struct ContextConfig {
    /// Context window requested from the provider. Ollama truncates the
    /// prompt at half of this, so raising it is the only way to allow a
    /// larger prompt.
    pub num_ctx: usize,
    /// Token budget for one request: system prompt, tool schemas, history
    /// and the current request together.
    pub max_prompt_tokens: usize,
    /// Past messages kept word-for-word. Older messages are condensed.
    pub max_history_messages: usize,
    /// Past messages read from memory at all.
    pub max_history_fetch: usize,
    /// Characters kept per older message when condensing.
    pub summary_message_chars: usize,
    /// Characters of a tool result allowed back into the prompt.
    pub max_tool_result_chars: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            num_ctx: 8192,
            max_prompt_tokens: 2800,
            max_history_messages: 12,
            max_history_fetch: 50,
            summary_message_chars: 160,
            max_tool_result_chars: 1200,
        }
    }
}

impl ContextConfig {
    /// Tokens we allow ourselves to send in one request.
    ///
    /// Measured on Ollama 0.40: the prompt is truncated at `num_ctx / 2`
    /// (2048 with the default 4096 window), and the truncation removes the
    /// *front* of the prompt, where the system prompt lives. Staying well
    /// below that limit is what keeps the honesty rules in front of the
    /// model. The eighth of headroom absorbs the error in estimating tokens
    /// as `chars / 4`.
    pub fn prompt_budget(&self) -> usize {
        let hard_limit = self.num_ctx / 2;
        self.max_prompt_tokens
            .min(hard_limit.saturating_sub(hard_limit / 8))
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
                native_tool_calling: true,
                temperature: 0.0,
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
            match std::fs::read_to_string(&config_path) {
                Ok(content) => match toml::from_str::<Config>(&content) {
                    Ok(file_config) => {
                        config = file_config;
                        // Recompute db_path if using default data_dir structure
                        let data_dir = config.general.data_dir.clone();
                        if config.memory.db_path.to_string_lossy().is_empty() {
                            config.memory.db_path = data_dir.join("memory.db");
                        }
                    }
                    // Silently falling back to defaults hides real mistakes,
                    // such as a renamed field, behind behaviour that looks
                    // like the user's settings were respected when they were
                    // not.
                    Err(e) => tracing::warn!(
                        "Ignoring unreadable config at {}: {}",
                        config_path.display(),
                        e
                    ),
                },
                Err(e) => {
                    tracing::warn!("Could not read config at {}: {}", config_path.display(), e)
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
        if let Ok(val) = std::env::var("JARVIS_MAX_PROMPT_TOKENS") {
            if let Ok(v) = val.parse::<usize>() {
                config.context.max_prompt_tokens = v;
            }
        }
        if let Ok(val) = std::env::var("JARVIS_MAX_HISTORY_FETCH") {
            if let Ok(v) = val.parse::<usize>() {
                config.context.max_history_fetch = v;
            }
        }
        if let Ok(val) = std::env::var("JARVIS_TEMPERATURE") {
            if let Ok(v) = val.parse::<f32>() {
                config.model.temperature = v;
            }
        }

        config
    }
}
