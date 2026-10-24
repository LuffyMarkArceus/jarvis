//! The daemon connection.
//!
//! One background task owns the socket. It reads wire messages and forwards
//! them to the UI, and writes the UI's requests and confirmation answers on
//! the same connection. A confirmation prompt arrives on that connection and
//! the daemon blocks until it is answered, so the two directions have to be
//! driven by the same task rather than a request made per call.

use jarvis_common::ipc::{
    Cancel, ConfirmationReply, ConfirmationRequest, Message, Request, Response,
};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Something the daemon said that the UI has to react to.
///
/// The `Response` for a cancelled turn carries `ok: true` with
/// `{"cancelled": true, ...}` rather than an error, so the UI can keep
/// partial text in place.
#[derive(Debug)]
pub enum ClientEvent {
    /// The socket connected; the daemon is reachable.
    Connected,
    Response(Response),
    /// A tool needs approval before the turn can continue.
    Confirmation(ConfirmationRequest),
    /// Incremental assistant text while the turn is still running.
    Chunk {
        id: Uuid,
        text: String,
    },
    /// The connection is gone, with a human-readable reason.
    Disconnected(String),
}

enum Command {
    Request(Request),
    Confirm { id: Uuid, approved: bool },
    Cancel { id: Uuid },
}

/// A handle to the background connection task.
pub struct Client {
    commands: mpsc::UnboundedSender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl Client {
    /// Connect to `socket` and start forwarding events to `events`.
    ///
    /// Failure to connect is reported as a [`ClientEvent::Disconnected`]
    /// rather than an error, so the UI can show it without special casing
    /// the startup path.
    pub fn spawn(socket: PathBuf, events: mpsc::UnboundedSender<ClientEvent>) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(socket, command_rx, events));
        Self { commands, task }
    }

    pub fn request(&self, request: Request) {
        let _ = self.commands.send(Command::Request(request));
    }

    pub fn confirm(&self, id: Uuid, approved: bool) {
        let _ = self.commands.send(Command::Confirm { id, approved });
    }

    /// Ask the daemon to stop the request `id`. No response is sent.
    pub fn cancel(&self, id: Uuid) {
        let _ = self.commands.send(Command::Cancel { id });
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // A replaced client must not keep a socket and a task alive.
        self.task.abort();
    }
}

async fn run(
    socket: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<ClientEvent>,
) {
    let stream = match UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(e) => {
            let _ = events.send(ClientEvent::Disconnected(connect_message(&socket, &e)));
            return;
        }
    };

    if events.send(ClientEvent::Connected).is_err() {
        return;
    }

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        tokio::select! {
            read = reader.read_line(&mut line) => {
                match read {
                    Ok(0) => {
                        let _ = events.send(ClientEvent::Disconnected(
                            "the daemon closed the connection".to_string(),
                        ));
                        return;
                    }
                    Ok(_) => match Message::decode(&line) {
                        Ok(Message::Response(response)) => {
                            let _ = events.send(ClientEvent::Response(response));
                        }
                        Ok(Message::ConfirmationRequest(request)) => {
                            let _ = events.send(ClientEvent::Confirmation(request));
                        }
                        Ok(Message::Chunk(chunk)) => {
                            let _ = events.send(ClientEvent::Chunk { id: chunk.id, text: chunk.text });
                        }
                        Ok(Message::Cancel(_)) | Ok(Message::Request(_)) | Ok(Message::ConfirmationReply(_)) => {}
                        Err(e) => {
                            let _ = events.send(ClientEvent::Disconnected(format!(
                                "unreadable message from the daemon: {e}"
                            )));
                            return;
                        }
                    },
                    Err(e) => {
                        let _ = events.send(ClientEvent::Disconnected(format!(
                            "lost the daemon connection: {e}"
                        )));
                        return;
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { return };
                let message = match command {
                    Command::Request(request) => Message::Request(request),
                    Command::Confirm { id, approved } => {
                        Message::ConfirmationReply(ConfirmationReply { id, approved })
                    }
                    Command::Cancel { id } => Message::Cancel(Cancel { id }),
                };
                if let Err(e) = write_message(&mut writer, &message).await {
                    let _ = events.send(ClientEvent::Disconnected(format!(
                        "could not talk to the daemon: {e}"
                    )));
                    return;
                }
            }
        }
    }
}

async fn write_message(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    message: &Message,
) -> std::io::Result<()> {
    writer
        .write_all(format!("{}\n", message.encode()).as_bytes())
        .await?;
    writer.flush().await
}

fn connect_message(socket: &Path, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "no daemon is listening on {}. Start it with `jarvis daemon start`.",
            socket.display()
        )
    } else {
        format!("could not connect to {}: {error}", socket.display())
    }
}
