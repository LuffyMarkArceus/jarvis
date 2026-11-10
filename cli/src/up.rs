//! `jarvis up`: everything Jarvis needs, started in one command - Ollama,
//! the keys, the daemon - then the TUI.

use jarvis_core::config::Config;
use std::path::{Path, PathBuf};

/// `KEY=value` lines; `export `, comments, blank lines and quotes allowed.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            let value = value.trim();
            let value = ['"', '\'']
                .iter()
                .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
                .unwrap_or(value);
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// The key files to load: `[up] env_files`, or every `*.env` in `dir`.
pub fn env_files(config: &Config, dir: &Path) -> Vec<PathBuf> {
    if !config.up.env_files.is_empty() {
        return config.up.env_files.clone();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "env"))
        .collect();
    files.sort();
    files
}

/// `host:port` of a local Ollama that a tier uses, if any.
pub fn local_ollama(config: &Config) -> Option<String> {
    let mut tiers = vec![&config.model];
    tiers.extend(config.remote.as_ref().map(|r| &r.model));
    tiers.into_iter().find_map(|m| {
        if m.provider != "ollama" {
            return None;
        }
        let rest = m
            .base_url
            .split_once("://")
            .map_or(m.base_url.as_str(), |(_, r)| r);
        let authority = rest.split('/').next().unwrap_or_default();
        let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
        if !["127.0.0.1", "localhost", "[::1]"].contains(&host) {
            return None;
        }
        Some(if authority.contains(':') && !authority.ends_with(']') {
            authority.to_string()
        } else {
            format!("{}:11434", authority)
        })
    })
}

/// Whether something answers on `host:port`.
fn answers(host: &str) -> bool {
    use std::net::ToSocketAddrs;
    host.to_socket_addrs().into_iter().flatten().any(|a| {
        std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_millis(300)).is_ok()
    })
}

/// Start what is not running, then open the TUI.
pub async fn run(restart: bool, tui: bool) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let config = Config::load();

    // 1. Ollama, if a tier uses one on this machine.
    if let Some(host) = local_ollama(&config) {
        if answers(&host) {
            println!("Ollama    running on {}", host);
        } else {
            let log = config
                .up
                .ollama_log
                .clone()
                .unwrap_or_else(|| config.general.data_dir.join("ollama.log"));
            if let Some(parent) = log.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)?;
            let spawned = Command::new("ollama")
                .arg("serve")
                .envs(&config.up.ollama_env)
                .env("OLLAMA_HOST", &host)
                .stdin(Stdio::null())
                .stdout(out.try_clone()?)
                .stderr(out)
                // Its own process group: closing this terminal does not stop it.
                .process_group(0)
                .spawn();
            match spawned {
                Err(e) => println!(
                    "Ollama    could not start ({}): local models will not answer",
                    e
                ),
                Ok(_) => {
                    let mut up = false;
                    for _ in 0..40 {
                        if answers(&host) {
                            up = true;
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    if up {
                        println!("Ollama    started on {} (log {})", host, log.display());
                    } else {
                        println!("Ollama    not answering yet; see {}", log.display());
                    }
                }
            }
        }
    }

    // 2. Keys, for the daemon only. Names are shown, values never.
    let dir = dirs::config_dir().unwrap_or_default().join("jarvis");
    let mut env: Vec<(String, String)> = Vec::new();
    for file in env_files(&config, &dir) {
        match std::fs::read_to_string(&file) {
            Ok(text) => env.extend(parse_env(&text)),
            Err(e) => println!("Keys      cannot read {}: {}", file.display(), e),
        }
    }
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    println!(
        "Keys      {}",
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    );

    // 3. The daemon.
    let exe = std::env::current_exe()?;
    let running = crate::daemon_responds().await;
    if running && !restart {
        println!("Daemon    running (`jarvis up --restart` reloads the config and keys)");
    } else {
        if running {
            Command::new(&exe).args(["daemon", "stop"]).status()?;
        }
        let started = Command::new(&exe)
            .args(["daemon", "start"])
            .envs(env)
            .status()?;
        if !started.success() {
            anyhow::bail!("the daemon did not start");
        }
    }

    // 4. The TUI, in this terminal.
    if tui {
        let path = exe.with_file_name("jarvis-tui");
        if !path.exists() {
            anyhow::bail!(
                "{} not found; build it with: cargo build -p jarvis-tui",
                path.display()
            );
        }
        let err = Command::new(&path).exec();
        anyhow::bail!("could not open {}: {}", path.display(), err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::config::{ModelConfig, RemoteConfig};

    #[test]
    fn env_files_parse_with_or_without_export() {
        let text = "# a comment\n\nexport OPENROUTER_API_KEY=sk-or-1\nOPENCODE_API_KEY=\"sk-oc 2\"\n  B='x=y'\nnot a line\n";
        assert_eq!(
            parse_env(text),
            vec![
                ("OPENROUTER_API_KEY".to_string(), "sk-or-1".to_string()),
                ("OPENCODE_API_KEY".to_string(), "sk-oc 2".to_string()),
                ("B".to_string(), "x=y".to_string()),
            ]
        );
    }

    #[test]
    fn the_default_key_files_are_the_env_files_in_the_config_folder() {
        let dir = std::env::temp_dir().join(format!(
            "jarvis-up-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["openrouter.env", "opencode.env", "config.toml", "x.env.bak"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let mut config = Config::default();
        assert_eq!(
            env_files(&config, &dir),
            vec![dir.join("opencode.env"), dir.join("openrouter.env")]
        );
        config.up.env_files = vec![PathBuf::from("/elsewhere/k.env")];
        assert_eq!(
            env_files(&config, &dir),
            vec![PathBuf::from("/elsewhere/k.env")]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ollama_is_started_only_for_a_local_ollama_tier() {
        let mut config = Config::default();
        assert_eq!(local_ollama(&config).as_deref(), Some("127.0.0.1:11434"));
        config.model.base_url = "http://192.168.1.5:11434".into();
        assert_eq!(local_ollama(&config), None);
        config.model.provider = "openai".into();
        config.model.base_url = "https://openrouter.ai/api/v1".into();
        config.remote = Some(RemoteConfig {
            model: ModelConfig {
                provider: "ollama".into(),
                base_url: "http://localhost:11500/".into(),
                ..ModelConfig::default()
            },
            default: false,
        });
        assert_eq!(local_ollama(&config).as_deref(), Some("localhost:11500"));
    }
}
