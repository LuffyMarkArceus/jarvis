//! The OS sandbox `shell.run` commands run in (bubblewrap).
//!
//! A command reads the whole filesystem except a hidden list, writes only
//! the workspace and a private `/tmp`, and has no network. The hidden list
//! protects Jarvis from what it runs: its permission rules, audit log, undo
//! copies, keys and socket. See docs/design/sandbox.md.

use crate::config::Config;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What a sandboxed command may touch.
#[derive(Debug, Clone, PartialEq)]
pub struct Sandbox {
    pub writable: Vec<PathBuf>,
    pub hidden: Vec<PathBuf>,
    pub network: bool,
}

impl Sandbox {
    /// The sandbox `config` describes, with `[tools] work_dir` writable and
    /// Jarvis's own files hidden.
    pub fn for_config(config: &Config) -> Sandbox {
        let mut writable = config.sandbox.writable.clone();
        writable.extend(config.tools.work_dir.clone());

        let mut hidden = Vec::new();
        if let Some(home) = dirs::home_dir() {
            hidden.push(home.join(".ssh"));
            hidden.push(home.join(".gnupg"));
        }
        // The session and system buses, gpg-agent, Wayland and systemd's
        // private socket: a read-only mount does not stop a connect(), and
        // `systemd-run --user` over the bus starts a program outside.
        hidden.push(PathBuf::from("/run/user"));
        hidden.push(PathBuf::from("/run/dbus"));
        // Config, permission rules and key files.
        if let Some(dir) = dirs::config_dir() {
            hidden.push(dir.join("jarvis"));
        }
        // Memory, audit log, undo copies and the daemon socket.
        hidden.push(config.general.data_dir.clone());
        hidden.extend(config.memory.db_path.parent().map(Path::to_path_buf));
        hidden.extend(config.undo.dir.clone());
        hidden.extend(config.sandbox.hidden.iter().cloned());
        hidden.dedup();

        Sandbox {
            writable,
            hidden,
            network: config.sandbox.network,
        }
    }

    /// `bwrap` arguments up to, not including, the command.
    pub fn bwrap_args(&self, work_dir: Option<&Path>, background: bool) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        let mut push =
            |items: &[&std::ffi::OsStr]| args.extend(items.iter().map(|i| i.to_os_string()));
        push(&["--ro-bind".as_ref(), "/".as_ref(), "/".as_ref()]);
        push(&["--dev".as_ref(), "/dev".as_ref()]);
        push(&["--proc".as_ref(), "/proc".as_ref()]);
        push(&["--tmpfs".as_ref(), "/tmp".as_ref()]);
        for dir in self.writable.iter().filter(|p| p.exists()) {
            push(&["--bind".as_ref(), dir.as_os_str(), dir.as_os_str()]);
        }
        // After the binds: in bwrap the later mount wins, so a hidden folder
        // inside the workspace stays hidden.
        for path in self.hidden.iter().filter(|p| p.exists()) {
            if path.is_dir() {
                push(&["--tmpfs".as_ref(), path.as_os_str()]);
            } else {
                push(&["--ro-bind".as_ref(), "/dev/null".as_ref(), path.as_os_str()]);
            }
        }
        if !self.network {
            push(&["--unshare-net".as_ref()]);
        }
        push(&["--unshare-pid".as_ref(), "--new-session".as_ref()]);
        // A background app outlives the call that started it.
        if !background {
            push(&["--die-with-parent".as_ref()]);
        }
        if let Some(dir) = work_dir {
            push(&["--chdir".as_ref(), dir.as_os_str()]);
        }
        args
    }
}

/// Whether `bwrap` runs here. Checked once.
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // Installed is not enough: user namespaces can be switched off.
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Whether commands run sandboxed under `config`.
pub fn ready(config: &Config) -> bool {
    config.sandbox.enabled && available()
}

/// The variables holding Jarvis's model keys.
pub fn key_vars(config: &Config) -> Vec<String> {
    let mut vars: Vec<String> = config.model.api_key_env.iter().cloned().collect();
    if let Some(remote) = &config.remote {
        vars.extend(remote.model.api_key_env.iter().cloned());
    }
    vars
}

/// Whether a command must not see the variable `name`.
///
/// By whole words of the name, so `AWS_SECRET_ACCESS_KEY` is a secret and
/// `TOKENIZERS_PARALLELISM` is not.
pub fn is_secret_var(name: &str, key_vars: &[String]) -> bool {
    const WORDS: &[&str] = &[
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "TOKEN",
        "CREDENTIAL",
        "CREDENTIALS",
    ];
    let upper = name.to_ascii_uppercase();
    let parts: Vec<&str> = upper.split('_').collect();
    key_vars.iter().any(|k| k == name)
        || parts.iter().any(|p| WORDS.contains(p))
        || parts.last() == Some(&"KEY")
        || parts.last() == Some(&"DSN")
        || upper == "DATABASE_URL"
        || upper == "PGPASSWORD"
}

/// Addresses of what is hidden inside the sandbox; a command run there
/// does not get them.
pub const SESSION_VARS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "XDG_RUNTIME_DIR",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn position(args: &[String], pair: &[&str]) -> Option<usize> {
        args.windows(pair.len())
            .position(|w| w.iter().zip(pair).all(|(a, b)| a == b))
    }

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-sandbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn args_bind_writable_before_hiding() {
        // An eval workspace contains the data folder: hidden must still win,
        // and in bwrap the later mount wins.
        let work = temp();
        let data = work.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let sandbox = Sandbox {
            writable: vec![work.clone()],
            hidden: vec![data.clone()],
            network: false,
        };
        let args = s(&sandbox.bwrap_args(Some(&work), false));
        let w = work.display().to_string();
        let d = data.display().to_string();
        let bind = position(&args, &["--bind", &w, &w]).expect("workspace bound");
        let hide = position(&args, &["--tmpfs", &d]).expect("data hidden");
        assert!(bind < hide, "{:?}", args);
        assert!(position(&args, &["--ro-bind", "/", "/"]).unwrap() < bind);
        assert!(position(&args, &["--chdir", &w]).is_some(), "{:?}", args);
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn background_does_not_die_with_parent() {
        let sandbox = Sandbox {
            writable: vec![],
            hidden: vec![],
            network: false,
        };
        let fg = s(&sandbox.bwrap_args(None, false));
        let bg = s(&sandbox.bwrap_args(None, true));
        assert!(fg.contains(&"--die-with-parent".to_string()), "{:?}", fg);
        assert!(!bg.contains(&"--die-with-parent".to_string()), "{:?}", bg);
        for args in [&fg, &bg] {
            assert!(args.contains(&"--new-session".to_string()));
            assert!(args.contains(&"--unshare-pid".to_string()));
            assert!(position(args, &["--tmpfs", "/tmp"]).is_some());
        }
    }

    #[test]
    fn no_network_unless_configured() {
        let mut sandbox = Sandbox {
            writable: vec![],
            hidden: vec![],
            network: false,
        };
        assert!(s(&sandbox.bwrap_args(None, false)).contains(&"--unshare-net".to_string()));
        sandbox.network = true;
        assert!(!s(&sandbox.bwrap_args(None, false)).contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn hidden_file_is_dev_null_and_folder_is_tmpfs() {
        let dir = temp();
        let file = dir.join("key.env");
        std::fs::write(&file, "x").unwrap();
        let sandbox = Sandbox {
            writable: vec![],
            hidden: vec![dir.clone(), file.clone()],
            network: false,
        };
        let args = s(&sandbox.bwrap_args(None, false));
        let f = file.display().to_string();
        let d = dir.display().to_string();
        assert!(
            position(&args, &["--ro-bind", "/dev/null", &f]).is_some(),
            "{:?}",
            args
        );
        assert!(position(&args, &["--tmpfs", &d]).is_some(), "{:?}", args);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_paths_are_skipped() {
        let gone = std::env::temp_dir().join(format!("jarvis-gone-{}", uuid::Uuid::new_v4()));
        let sandbox = Sandbox {
            writable: vec![gone.clone()],
            hidden: vec![gone.join("x")],
            network: false,
        };
        let args = s(&sandbox.bwrap_args(None, false));
        let g = gone.display().to_string();
        assert!(args.iter().all(|a| !a.starts_with(&g)), "{:?}", args);
    }

    #[test]
    fn for_config_hides_the_data_dir_and_config() {
        let mut config = Config::default();
        config.general.data_dir = PathBuf::from("/srv/jarvis-data");
        config.memory.db_path = PathBuf::from("/srv/jarvis-mem/memory.db");
        config.undo.dir = Some(PathBuf::from("/srv/jarvis-undo"));
        config.tools.work_dir = Some(PathBuf::from("/srv/work"));
        config.sandbox.writable = vec![PathBuf::from("/srv/projects")];
        config.sandbox.hidden = vec![PathBuf::from("/srv/secrets")];
        config.sandbox.network = true;
        let sandbox = Sandbox::for_config(&config);
        for hidden in [
            "/srv/jarvis-data",
            "/srv/jarvis-mem",
            "/srv/jarvis-undo",
            "/srv/secrets",
        ] {
            assert!(
                sandbox.hidden.contains(&PathBuf::from(hidden)),
                "{}: {:?}",
                hidden,
                sandbox.hidden
            );
        }
        let home = dirs::home_dir().unwrap();
        assert!(sandbox.hidden.contains(&home.join(".ssh")));
        assert!(sandbox.hidden.contains(&home.join(".gnupg")));
        // The session bus and its siblings (gpg-agent, Wayland, systemd's
        // private socket) are ways out of the sandbox: a read-only mount does
        // not stop a connect().
        assert!(sandbox.hidden.contains(&PathBuf::from("/run/user")));
        assert!(sandbox.hidden.contains(&PathBuf::from("/run/dbus")));
        assert!(sandbox
            .hidden
            .contains(&dirs::config_dir().unwrap().join("jarvis")));
        assert_eq!(
            sandbox.writable,
            vec![PathBuf::from("/srv/projects"), PathBuf::from("/srv/work")]
        );
        assert!(sandbox.network);
    }

    #[test]
    fn secret_vars() {
        let mut config = Config::default();
        config.model.api_key_env = Some("MY_KEY".into());
        let keys = key_vars(&config);
        assert_eq!(keys, vec!["MY_KEY".to_string()]);
        for name in [
            "OPENROUTER_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET",
            "DB_PASSWORD",
            "MY_KEY",
        ] {
            assert!(is_secret_var(name, &keys), "{}", name);
        }
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "GCP_PRIVATE_KEY",
            "STRIPE_KEY",
            "DOCKER_CREDENTIALS",
            "DATABASE_URL",
            "SENTRY_DSN",
            "PGPASSWORD",
        ] {
            assert!(is_secret_var(name, &keys), "{}", name);
        }
        for name in [
            "PATH",
            "HOME",
            "TOKENIZERS_PARALLELISM",
            "MY_KEYBOARD",
            "KEYMAP",
        ] {
            assert!(!is_secret_var(name, &keys), "{}", name);
        }
    }
}
