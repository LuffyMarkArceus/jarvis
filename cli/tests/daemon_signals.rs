//! The daemon and the signals a desktop sends it. Real use: closing the
//! terminal a daemon was started from could take it down (SIGHUP), and
//! `jarvis daemon stop` (SIGTERM) left its socket and PID file behind.
//!
//! Each test runs the real binary in its own data and config folders, with a
//! model address nothing listens on, so nothing loads on the owner's GPU.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Home {
    root: PathBuf,
}

impl Drop for Home {
    fn drop(&mut self) {
        // A failed test must not leave its daemon running.
        if let Ok(pid) = std::fs::read_to_string(self.data().join("jarvis.pid")) {
            let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Home {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "jarvis-signals-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos() ^ rand_suffix()
        ));
        std::fs::create_dir_all(root.join("config/jarvis")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(
            root.join("config/jarvis/config.toml"),
            "[model]\nprovider = \"ollama\"\nbase_url = \"http://127.0.0.1:9\"\n",
        )
        .unwrap();
        Home { root }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/jarvis")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_jarvis-cli"));
        c.args(args)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("HOME", &self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn wait_for(mut what: impl FnMut() -> bool, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if what() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn signal(pid: u32, name: &str) {
    let ok = Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -{name} {pid}");
}

fn running(child: &mut Child) -> bool {
    child.try_wait().unwrap().is_none()
}

fn sock(home: &Home) -> PathBuf {
    home.data().join("jarvis.sock")
}

#[test]
fn a_hangup_does_not_stop_the_daemon_and_a_terminate_stops_it_cleanly() {
    let home = Home::new();
    let mut child = home.command(&["daemon", "run"]).spawn().unwrap();
    assert!(
        wait_for(|| sock(&home).exists(), 20),
        "the daemon never listened"
    );
    let pid_file = home.data().join("jarvis.pid");
    assert!(wait_for(|| pid_file.exists(), 5));

    // Closing the terminal it came from.
    signal(child.id(), "HUP");
    std::thread::sleep(Duration::from_millis(800));
    assert!(running(&mut child), "SIGHUP stopped the daemon");
    assert!(sock(&home).exists());

    // `jarvis daemon stop`, or the system shutting down.
    signal(child.id(), "TERM");
    assert!(
        wait_for(|| !running(&mut child), 10),
        "SIGTERM did not stop it"
    );
    assert!(!sock(&home).exists(), "socket left behind");
    assert!(!pid_file.exists(), "PID file left behind");
}

/// The process group and standard input of `pid`.
fn group_and_stdin(pid: &str) -> (String, PathBuf) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // pid (comm) state ppid pgrp ...; comm may contain spaces.
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    let pgrp = after.split_whitespace().nth(2).unwrap().to_string();
    let stdin = std::fs::read_link(format!("/proc/{pid}/fd/0")).unwrap();
    (pgrp, stdin)
}

#[test]
fn daemon_start_leaves_the_terminal_behind() {
    let home = Home::new();
    let status = home.command(&["daemon", "start"]).status().unwrap();
    assert!(status.success());
    let pid_file = home.data().join("jarvis.pid");
    assert!(wait_for(|| pid_file.exists(), 10));
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();

    let (pgrp, stdin) = group_and_stdin(&pid);
    let ours = group_and_stdin(&std::process::id().to_string()).0;
    assert_ne!(
        pgrp, ours,
        "the daemon is in the starting terminal's process group"
    );
    assert_eq!(pgrp, pid, "the daemon leads its own process group");
    assert_eq!(
        stdin,
        Path::new("/dev/null"),
        "the daemon reads the terminal"
    );

    let pid: u32 = pid.parse().unwrap();
    signal(pid, "TERM");
    assert!(wait_for(|| !sock(&home).exists(), 10), "it did not stop");
}
