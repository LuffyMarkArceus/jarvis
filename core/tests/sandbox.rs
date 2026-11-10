//! `shell.run` inside the OS sandbox, with real bubblewrap. Skipped, with a
//! message, where bwrap does not run (CI may lack it).

use jarvis_core::config::Config;
use jarvis_core::sandbox::{self, Sandbox};
use jarvis_core::tools::shell::RunShellTool;
use jarvis_core::tools::Tool;
use serde_json::{json, Value};
use std::path::PathBuf;

struct Dirs {
    root: PathBuf,
    work: PathBuf,
    data: PathBuf,
    outside: PathBuf,
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn dirs() -> Dirs {
    let root = std::env::temp_dir().join(format!("jarvis-sbx-{}", uuid::Uuid::new_v4()));
    let work = root.join("work");
    let data = work.join("data");
    let outside = root.join("outside");
    for d in [&work, &data, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(data.join("permissions.toml"), "[[rules]]").unwrap();
    Dirs {
        root,
        work,
        data,
        outside,
    }
}

fn tool(d: &Dirs) -> RunShellTool {
    let mut config = Config::default();
    config.general.data_dir = d.data.clone();
    config.memory.db_path = d.data.join("memory.db");
    config.tools.work_dir = Some(d.work.clone());
    RunShellTool {
        work_dir: Some(d.work.clone()),
        sandbox: Some(Sandbox::for_config(&config)),
        key_vars: vec!["JARVIS_SBX_TEST_KEY".into()],
    }
}

fn skip() -> bool {
    if !sandbox::available() {
        eprintln!("bwrap does not run here; sandbox test skipped");
        return true;
    }
    false
}

async fn run(tool: &RunShellTool, args: Value) -> Value {
    tool.execute(args).await.unwrap()
}

#[tokio::test]
async fn writes_land_only_in_the_workspace() {
    if skip() {
        return;
    }
    let d = dirs();
    let t = tool(&d);
    let inside = run(&t, json!({ "command": "echo hi > made.txt" })).await;
    assert_eq!(inside["status"], 0, "{}", inside);
    assert_eq!(inside["sandboxed"], true);
    assert!(d.work.join("made.txt").exists());

    // Not under /tmp: the sandbox has its own, empty /tmp, where the folder
    // would not exist at all. Home is read-only inside.
    let target = dirs::home_dir()
        .unwrap()
        .join(format!(".jarvis-sbx-{}", uuid::Uuid::new_v4()));
    let out = run(
        &t,
        json!({ "command": format!("echo hi > {}", target.display()) }),
    )
    .await;
    assert_ne!(out["status"], 0, "{}", out);
    assert!(!target.exists());
    assert!(
        out["hint"]
            .as_str()
            .unwrap_or("")
            .contains("outside the sandbox"),
        "{}",
        out
    );
}

#[tokio::test]
async fn jarvis_files_are_hidden_even_inside_the_workspace() {
    if skip() {
        return;
    }
    let d = dirs();
    let t = tool(&d);
    let out = run(
        &t,
        json!({ "command": "cat data/permissions.toml; ls data" }),
    )
    .await;
    assert!(
        !out["stdout"].as_str().unwrap().contains("rules"),
        "{}",
        out
    );
    assert!(
        !out["stdout"].as_str().unwrap().contains("permissions"),
        "{}",
        out
    );
    let out = run(&t, json!({ "command": "echo x > data/permissions.toml" })).await;
    // The tmpfs over it is writable, but thrown away: the real file is untouched.
    let _ = out;
    assert_eq!(
        std::fs::read_to_string(d.data.join("permissions.toml")).unwrap(),
        "[[rules]]"
    );
}

#[tokio::test]
async fn there_is_no_network() {
    if skip() {
        return;
    }
    let d = dirs();
    let out = run(
        &tool(&d),
        json!({ "command": "getent hosts example.com || cat /proc/net/dev | grep -c :" }),
    )
    .await;
    // No address resolves, and only the loopback interface exists.
    assert_eq!(out["stdout"].as_str().unwrap().trim(), "1", "{}", out);
}

#[tokio::test]
async fn keys_are_not_in_the_environment_inside_or_outside() {
    if skip() {
        return;
    }
    let d = dirs();
    // Names no other test reads.
    std::env::set_var("JARVIS_SBX_TEST_KEY", "sekret");
    std::env::set_var("JARVIS_SBX_TEST_API_KEY", "sekret");
    let t = tool(&d);
    for outside in [false, true] {
        let out = run(&t, json!({ "command": "env", "outside_sandbox": outside })).await;
        let env = out["stdout"].as_str().unwrap();
        assert!(!env.contains("sekret"), "outside={}: key visible", outside);
        assert!(env.contains("PATH="), "{}", env);
    }
}

#[tokio::test]
async fn an_unrelated_failure_gets_no_hint() {
    if skip() {
        return;
    }
    let d = dirs();
    let out = run(&tool(&d), json!({ "command": "ls /no/such/place" })).await;
    assert_ne!(out["status"], 0);
    assert!(out.get("hint").is_none(), "{}", out);
}

#[tokio::test]
async fn outside_the_sandbox_writes_anywhere() {
    let d = dirs();
    let target = d.outside.join("y.txt");
    let out = run(
        &tool(&d),
        json!({ "command": format!("echo hi > {}", target.display()), "outside_sandbox": true }),
    )
    .await;
    assert_eq!(out["status"], 0, "{}", out);
    assert_eq!(out["sandboxed"], false);
    assert!(target.exists());
}

#[tokio::test]
async fn without_a_sandbox_a_command_runs_outside() {
    let d = dirs();
    let mut t = tool(&d);
    t.sandbox = None;
    let out = run(&t, json!({ "command": "true" })).await;
    assert_eq!(out["sandboxed"], false, "{}", out);
}

#[tokio::test]
async fn the_session_bus_and_agent_sockets_are_out_of_reach() {
    // `busctl --user call org.freedesktop.systemd1 ... StartTransientUnit`
    // would start a program outside the sandbox, unasked.
    if skip() {
        return;
    }
    let d = dirs();
    std::env::set_var("JARVIS_SBX_PROBE", "1");
    let out = run(
        &tool(&d),
        json!({ "command": "ls -A /run/user | wc -l; test -e /run/dbus/system_bus_socket && echo SYSBUS; env | grep -E '^(DBUS_SESSION_BUS_ADDRESS|SSH_AUTH_SOCK|XDG_RUNTIME_DIR)=' ; true" }),
    )
    .await;
    let stdout = out["stdout"].as_str().unwrap();
    assert_eq!(stdout.trim(), "0", "{}", out);
}
