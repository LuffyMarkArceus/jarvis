use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub mcp: Option<McpConfig>,
}

/// Default data directory, shared by the config sections that need it.
fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("jarvis")
}

/// Every section has defaults, so a config file only needs the settings it
/// actually wants to change. Requiring every field meant one missing key
/// silently discarded the whole file - which is how a real config ended up
/// ignored for weeks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub name: String,
    pub log_level: String,
    pub data_dir: PathBuf,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            name: "Jarvis".to_string(),
            log_level: "info".to_string(),
            data_dir: default_data_dir(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "ollama".to_string(),
            base_url: "http://127.0.0.1:11434".to_string(),
            default_model: "llama3.2:1b".to_string(),
            routing_model: "llama3.2:1b".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            native_tool_calling: true,
            temperature: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub db_path: PathBuf,
    pub vector_dim: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            db_path: default_data_dir().join("memory.db"),
            vector_dim: 768,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Offer this server's tools on every model turn.
    ///
    /// Off by default. A server with a large tool surface should be selected
    /// by intent (see [`crate::planner`]) rather than always present, because
    /// every offered tool costs prompt budget and attention on small models.
    #[serde(default)]
    pub always_offer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub require_confirmation: bool,
    pub allowed_shell_commands: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            require_confirmation: true,
            allowed_shell_commands: Vec::new(),
        }
    }
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

        config.resolve_sidecar_paths();
        config
    }

    /// Turn `sidecars/...` arguments into absolute paths.
    ///
    /// A config that points at a script in `/tmp` works exactly once. This
    /// makes the shipped sidecars addressable from a checkout, an installed
    /// tree, or an explicit override, without the user hand-writing an
    /// absolute path.
    fn resolve_sidecar_paths(&mut self) {
        let Some(mcp) = self.mcp.as_mut() else {
            return;
        };
        let root = sidecar_dir();
        for server in &mut mcp.servers {
            for arg in &mut server.args {
                if let Some(resolved) = expand_sidecar_arg(arg, root.as_deref()) {
                    *arg = resolved;
                }
            }
        }
    }
}

/// The directory holding the sidecars shipped with this build.
///
/// Checked in order: `JARVIS_SIDECAR_DIR`, an ancestor of the executable
/// (a `target/debug` binary), an ancestor of the working directory (running
/// from a checkout), and finally the data directory of an installed tree.
pub fn sidecar_dir() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("JARVIS_SIDECAR_DIR") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }

    let looks_right = |dir: &std::path::Path| dir.join("fetch/server.py").is_file();

    let from_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .and_then(|dir| find_ancestor(&dir, looks_right));
    if from_exe.is_some() {
        return from_exe;
    }

    if let Ok(cwd) = std::env::current_dir() {
        if let Some(found) = find_ancestor(&cwd, looks_right) {
            return Some(found);
        }
    }

    let installed = default_data_dir().join("sidecars");
    if looks_right(&installed) {
        return Some(installed);
    }

    None
}

fn find_ancestor(
    start: &std::path::Path,
    predicate: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join("sidecars");
        if predicate(&candidate) {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// Expand one MCP `args` entry.
///
/// Supports `${SIDECAR_DIR}` and a bare relative `sidecars/...` path, which
/// is what the shipped example config uses.
fn expand_sidecar_arg(arg: &str, root: Option<&std::path::Path>) -> Option<String> {
    let root = root?.to_string_lossy();

    if let Some(rest) = arg.strip_prefix("${SIDECAR_DIR}") {
        return Some(format!("{}{}", root.trim_end_matches('/'), rest));
    }
    if arg == "sidecars" {
        return Some(root.to_string());
    }
    if let Some(rest) = arg.strip_prefix("sidecars/") {
        return Some(format!("{}/{}", root.trim_end_matches('/'), rest));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_config_keeps_every_other_default() {
        // No [memory] section, and no most fields: this used to fail to
        // parse and silently discard the user's whole file.
        let text = r#"
            [general]
            name = "Jarvis"

            [model]
            default_model = "tiny"

            [context]
            max_prompt_tokens = 900
        "#;
        let config: Config = toml::from_str(text).expect("partial config should parse");

        assert_eq!(config.model.default_model, "tiny");
        assert_eq!(config.model.base_url, "http://127.0.0.1:11434");
        assert!(config.model.native_tool_calling);
        assert_eq!(config.memory.vector_dim, 768);
        assert!(config.memory.db_path.ends_with("memory.db"));
        assert!(config.tools.require_confirmation);
        assert_eq!(config.context.max_prompt_tokens, 900);
        assert_eq!(config.context.num_ctx, 8192);
    }

    #[test]
    fn an_unknown_key_is_ignored_rather_than_discarding_the_file() {
        let text = r#"
            [context]
            max_context_chars = 2000

            [model]
            default_model = "tiny"
        "#;
        let config: Config = toml::from_str(text).expect("unknown keys should not fail");
        assert_eq!(config.model.default_model, "tiny");
    }

    #[test]
    fn the_prompt_budget_stays_below_the_provider_limit() {
        let default = ContextConfig::default();
        assert!(default.prompt_budget() < default.num_ctx / 2);

        // A budget larger than the provider allows is clamped, not honoured.
        let greedy = ContextConfig {
            num_ctx: 4096,
            max_prompt_tokens: 4000,
            ..ContextConfig::default()
        };
        assert_eq!(greedy.prompt_budget(), 4096 / 2 - 4096 / 2 / 8);
    }
    #[test]
    fn sidecar_placeholder_is_expanded() {
        let root = std::path::Path::new("/opt/jarvis/sidecars");
        assert_eq!(
            expand_sidecar_arg("${SIDECAR_DIR}/fetch/server.py", Some(root)).as_deref(),
            Some("/opt/jarvis/sidecars/fetch/server.py")
        );
    }

    #[test]
    fn relative_sidecar_path_is_expanded() {
        let root = std::path::Path::new("/opt/jarvis/sidecars");
        assert_eq!(
            expand_sidecar_arg("sidecars/fetch/server.py", Some(root)).as_deref(),
            Some("/opt/jarvis/sidecars/fetch/server.py")
        );
        assert_eq!(
            expand_sidecar_arg("sidecars", Some(root)).as_deref(),
            Some("/opt/jarvis/sidecars")
        );
    }

    #[test]
    fn ordinary_arguments_are_untouched() {
        let root = std::path::Path::new("/opt/jarvis/sidecars");
        assert_eq!(expand_sidecar_arg("--flag", Some(root)), None);
        assert_eq!(expand_sidecar_arg("/usr/bin/python3", Some(root)), None);
    }

    #[test]
    fn without_a_root_nothing_is_guessed() {
        assert_eq!(expand_sidecar_arg("sidecars/fetch/server.py", None), None);
    }

    #[test]
    fn mcp_servers_opt_into_always_offer() {
        let text = r#"
            [mcp]
            [[mcp.servers]]
            name = "notes"
            command = "notes-mcp"
            always_offer = true

            [[mcp.servers]]
            name = "filesystem"
            command = "fs-mcp"
        "#;
        let config: Config = toml::from_str(text).expect("valid");
        let servers = config.mcp.expect("mcp").servers;
        assert!(servers[0].always_offer);
        assert!(!servers[1].always_offer);
    }
}
