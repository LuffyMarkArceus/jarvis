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

        perform(actions.drain(..), &mut app, &mut client, &socket, &event_tx);
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

fn perform(
    actions: impl IntoIterator<Item = Action>,
    app: &mut App,
    client: &mut Option<Client>,
    socket: &Path,
    event_tx: &mpsc::UnboundedSender<ClientEvent>,
) {
    for action in actions {
        match action {
            Action::Quit => app.should_quit = true,
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
            Action::Confirm { id, approved } => match client.as_ref() {
                Some(client) => client.confirm(id, approved),
                None => app.note(Entry::error(
                    "Cannot answer: the daemon is not connected.".to_string(),
                )),
            },
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
