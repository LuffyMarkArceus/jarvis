//! Jarvis terminal UI.
//!
//! A thin client over the daemon's Unix socket: the TUI owns the screen and
//! the conversation, while the daemon owns the model, tools, permissions and
//! history.

mod app;
mod client;
mod ui;
mod wrap;

use anyhow::Result;
use app::{Action, App, DaemonState, Entry};
use client::{Client, ClientEvent};
use crossterm::event::{Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use jarvis_common::ipc::Request;
use jarvis_core::config::Config;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

const TICK: Duration = Duration::from_millis(120);

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load();
    let socket = config.general.data_dir.join("jarvis.sock");
    let mut app = App::new(config.model.default_model.clone());

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    spawn_input_thread(input_tx);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut client = if socket.exists() {
        Some(Client::spawn(socket.clone(), event_tx.clone()))
    } else {
        app.on_disconnected(offline_message(&socket));
        None
    };

    let mut ticker = tokio::time::interval(TICK);
    let mut actions: Vec<Action> = Vec::new();

    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            event = input_rx.recv() => {
                if let Some(event) = event {
                    actions = handle_input_event(&mut app, event);
                }
            }
            event = event_rx.recv() => {
                if let Some(event) = event {
                    actions = app.on_client_event(event);
                }
            }
            _ = ticker.tick() => {
                app.tick();
                actions.clear();
            }
        }

        perform(
            actions.drain(..),
            &mut app,
            &mut client,
            &socket,
            &event_tx,
            &config,
        );
    }

    Ok(())
}

fn handle_input_event(app: &mut App, event: Event) -> Vec<Action> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            app.handle_key(key)
        }
        Event::Paste(text) => {
            app.on_paste(&text);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// `/undo`: read and change the undo folder directly, like `jarvis undo`,
/// so it works whether or not the daemon is running.
fn undo(action: Action, app: &mut App, config: &Config) {
    use jarvis_core::undo::{self, UndoPlan, UndoStore};
    let store = match UndoStore::for_config(config) {
        Ok(store) => store,
        Err(e) => return app.note(Entry::error(format!("Undo is unavailable: {}", e))),
    };
    match action {
        Action::UndoList => match store.list() {
            Ok(changes) if changes.is_empty() => app.note(Entry::system("No changes to undo.")),
            Ok(changes) => {
                let lines: Vec<String> = changes.iter().take(20).map(undo::list_line).collect();
                app.note(Entry::system(lines.join("\n")));
            }
            Err(e) => app.note(Entry::error(e.to_string())),
        },
        Action::UndoPreview(id) => match store.plan(id.as_deref(), false) {
            Ok(UndoPlan::Refuse { reason, .. }) => app.note(Entry::system(format!(
                "Cannot undo: {} (`jarvis undo --force` in a terminal overrides)",
                reason
            ))),
            Ok(ref plan @ (UndoPlan::Restore(ref change) | UndoPlan::Remove(ref change))) => {
                app.offer_undo(change.id.clone(), undo::describe(plan))
            }
            Err(e) => app.note(Entry::system(format!("Nothing to undo: {}.", e))),
        },
        Action::UndoApply(id) => {
            let applied = store.plan(Some(&id), false).and_then(|plan| match plan {
                UndoPlan::Refuse { reason, .. } => {
                    Err(jarvis_core::error::JarvisError::Tool(reason))
                }
                plan => {
                    let audit = jarvis_core::permissions::PermissionManager::new(config)?;
                    undo::apply_recorded(&store, &plan, false, &audit)
                }
            });
            app.note(match applied {
                Ok(()) => Entry::system(
                    "Undone. /undo list shows it; the undo itself can be undone by its id.",
                ),
                Err(e) => Entry::error(format!("Not undone: {}", e)),
            });
        }
        _ => {}
    }
}

fn perform(
    actions: impl IntoIterator<Item = Action>,
    app: &mut App,
    client: &mut Option<Client>,
    socket: &Path,
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
    config: &Config,
) {
    for action in actions {
        match action {
            Action::Quit => app.should_quit = true,
            Action::UndoPreview(_) | Action::UndoList | Action::UndoApply(_) => {
                undo(action, app, config)
            }
            Action::Reconnect => {
                // Dropping the old client aborts its socket task.
                *client = None;
                app.state = DaemonState::Connecting;
                if socket.exists() {
                    *client = Some(Client::spawn(socket.to_path_buf(), event_tx.clone()));
                } else {
                    app.on_disconnected(offline_message(socket));
                }
            }
            Action::Confirm {
                id,
                approved,
                choice,
            } => match client.as_ref() {
                Some(client) => client.confirm(id, approved, choice),
                None => app.note(Entry::error(
                    "Cannot answer: the daemon is not connected.".to_string(),
                )),
            },
            Action::Cancel { id } => {
                if let Some(client) = client.as_ref() {
                    client.cancel(id);
                }
            }
            Action::Request {
                method,
                params,
                pending,
            } => {
                if app.pending.is_some() {
                    app.note(Entry::system("Still waiting for the daemon."));
                    continue;
                }
                let Some(client) = client.as_ref() else {
                    app.request_failed(offline_message(socket));
                    continue;
                };
                let request = Request::new(method, params);
                app.begin_request(request.id, pending);
                client.request(request);
            }
        }
    }
}

fn offline_message(socket: &Path) -> String {
    format!(
        "No daemon is listening on {}. Start it with `jarvis daemon start`, then press Ctrl+R.",
        socket.display()
    )
}

fn spawn_input_thread(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || loop {
        match crossterm::event::poll(Duration::from_millis(50)) {
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    crossterm::execute!(
        out,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    let terminal = Terminal::new(CrosstermBackend::new(out))?;
    Ok(terminal)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        stdout(),
        LeaveAlternateScreen,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show
    );
}

/// Restores the terminal on the way out, including an early `?` return.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::undo::UndoStore;

    /// A config whose data lives in a fresh folder, with one undoable change
    /// to `notes.md` ("mine" -> "jarvis").
    fn with_a_change(name: &str) -> (Config, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("jarvis-tui-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = Config::default();
        config.general.data_dir = dir.clone();
        let notes = dir.join("notes.md");
        std::fs::write(&notes, "mine").unwrap();
        let store = UndoStore::for_config(&config).unwrap();
        let pending = store
            .before_change(&notes, "filesystem.write", "s1")
            .unwrap();
        std::fs::write(&notes, "jarvis").unwrap();
        store.after_change(pending).unwrap();
        (config, dir)
    }

    fn typed(app: &mut App, text: &str, config: &Config) {
        app.input.set(text);
        for action in app.submit() {
            undo(action, app, config);
        }
    }

    fn last(app: &App) -> String {
        app.entries.last().unwrap().text.clone()
    }

    #[test]
    fn undo_previews_then_restores_exactly_what_it_showed() {
        let (config, dir) = with_a_change("flow");
        let mut app = App::new("test".into());

        typed(&mut app, "/undo", &config);
        assert!(last(&app).contains("Restore"), "{}", last(&app));
        assert!(last(&app).contains("/undo yes"), "{}", last(&app));
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "jarvis",
            "only shown so far"
        );

        typed(&mut app, "/undo yes", &config);
        assert!(last(&app).starts_with("Undone"), "{}", last(&app));
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "mine"
        );

        typed(&mut app, "/undo list", &config);
        assert!(last(&app).contains("undone"), "{}", last(&app));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_undo_says_why_and_changes_nothing() {
        let (config, dir) = with_a_change("refused");
        std::fs::write(dir.join("notes.md"), "my own edit").unwrap();
        let mut app = App::new("test".into());

        typed(&mut app, "/undo", &config);
        assert!(
            last(&app).contains("changed after Jarvis wrote it"),
            "{}",
            last(&app)
        );
        typed(&mut app, "/undo yes", &config);
        assert!(last(&app).contains("/undo first"), "{}", last(&app));
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "my own edit"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
