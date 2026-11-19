//! "open X": X is launched only if it is an installed app, as the app's own
//! command. Real use: "open vscode" ran `vscode` (no such program) and
//! "open the episode 001 of BleaCH on my DATA drive" ran `episode 001`.

mod common;

use common::{answer, Script};
use jarvis_core::config::Config;
use jarvis_core::orchestrator::{ConfirmationHandler, Orchestrator};
use jarvis_core::permissions::PermissionManager;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct Yes(Mutex<Vec<Value>>);

#[async_trait::async_trait]
impl ConfirmationHandler for Yes {
    async fn confirm(&self, _tool: &str, args: &Value) -> bool {
        self.0.lock().unwrap().push(args.clone());
        // No: nothing is really launched by the test.
        false
    }
}

struct Setup {
    dir: PathBuf,
    jarvis: Orchestrator,
    conv: std::sync::Arc<jarvis_core::conversation::Conversation>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup() -> Setup {
    let dir = std::env::temp_dir().join(format!("jarvis-apps-{}", uuid::Uuid::new_v4()));
    let apps = dir.join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("code.desktop"),
        "[Desktop Entry]\nName=Visual Studio Code\nExec=code %F\nType=Application\nKeywords=vscode;\n",
    )
    .unwrap();
    let mut config = Config::default();
    config.general.data_dir = dir.clone();
    config.memory.db_path = dir.join("memory.db");
    config.memory.recall_enabled = false;
    config.tools.app_dirs = Some(vec![apps]);
    let permissions =
        PermissionManager::with_paths(dir.join("permissions.toml"), dir.join("audit.db")).unwrap();
    let jarvis = Orchestrator::with_parts(
        config,
        Arc::new(Script::new(vec![answer("I looked for it.")])),
        permissions,
    );
    let conv = jarvis.new_conversation();
    Setup { dir, conv, jarvis }
}

#[tokio::test]
async fn an_installed_app_is_launched_as_its_own_command() {
    let s = setup();
    let owner = Yes(Mutex::new(Vec::new()));
    let report = s
        .jarvis
        .run_turn(&s.conv, "open vscode for me", Some(&owner), None, None)
        .await
        .unwrap();
    assert_eq!(report.route, "Tool(shell.run)");
    assert_eq!(
        report.calls[0].args["command"], "code",
        "{:?}",
        report.calls
    );
    assert_eq!(
        owner.0.lock().unwrap()[0]["command"],
        "code",
        "the prompt shows the real command"
    );
}

#[tokio::test]
async fn something_that_is_not_an_app_goes_to_the_model() {
    let s = setup();
    let report = s
        .jarvis
        .run_turn(
            &s.conv,
            "Can you open the episode 001 of BleaCH on my DATA drive?",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(report.route, "Model", "{:?}", report.calls);
    assert!(
        report
            .calls
            .iter()
            .all(|c| c.args["command"] != "episode 001"),
        "{:?}",
        report.calls
    );
    assert!(
        report.offered.iter().any(|t| t == "shell.run"),
        "the model can find and open the file: {:?}",
        report.offered
    );
}
