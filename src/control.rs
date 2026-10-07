//! Control socket: `cosmicbar toggle network` from a compositor keybind.
//!
//! waybar could only be poked with `pkill -SIGRTMIN+N`, which refreshes a
//! module but cannot open anything. A line-oriented unix socket lets a niri
//! bind drive the bar's popups directly.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const MAX_COMMAND_BYTES: usize = 4096;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 16;

use crate::bar::Message;
use crate::modules::ModuleId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Open the module's popup, or close it if that popup is already open.
    Toggle(ModuleId),
    /// Close whatever popup is open.
    Close,
    /// Re-read the config file.
    Reload,
    /// Apply the named brightness mode, or step to the next one.
    BrightnessMode(Option<String>),
}

impl Command {
    pub fn parse(line: &str) -> anyhow::Result<Self> {
        let mut words = line.split_whitespace();
        let command = match words.next() {
            Some("toggle") => {
                let name = words.next().context("toggle needs a module name")?;
                let module = ModuleId::parse_declared(name)
                    .with_context(|| format!("unknown module `{name}`"))?;
                Self::Toggle(module)
            }
            Some("close") => Self::Close,
            Some("reload") => Self::Reload,
            Some("brightness-mode") => {
                // A mode's name may have spaces in it.
                let name = words.collect::<Vec<_>>().join(" ");
                return Ok(Self::BrightnessMode((!name.is_empty()).then_some(name)));
            }
            Some(other) => anyhow::bail!("unknown command `{other}`"),
            None => anyhow::bail!("empty command"),
        };
        anyhow::ensure!(words.next().is_none(), "unexpected command argument");
        Ok(command)
    }
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    dir.join(format!("cosmicbar-{display}.sock"))
}

/// Send one command to a running bar.
pub fn send(line: &str) -> anyhow::Result<()> {
    Command::parse(line)?;
    anyhow::ensure!(line.len() < MAX_COMMAND_BYTES, "command is too long");
    anyhow::ensure!(!line.contains(['\n', '\r']), "command must be one line");
    let path = socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("no bar listening on {}", path.display()))?;
    stream.set_write_timeout(Some(COMMAND_TIMEOUT))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

async fn read_command(stream: tokio::net::UnixStream) -> anyhow::Result<Command> {
    let mut reader = BufReader::new(stream.take(MAX_COMMAND_BYTES as u64));
    let mut line = Vec::new();
    tokio::time::timeout(COMMAND_TIMEOUT, reader.read_until(b'\n', &mut line))
        .await
        .context("control command timed out")??;
    anyhow::ensure!(
        line.len() < MAX_COMMAND_BYTES || line.ends_with(b"\n"),
        "command is too long"
    );
    Command::parse(std::str::from_utf8(&line)?.trim())
}

async fn serve(
    listener: tokio::net::UnixListener,
    sender: cosmic::iced::futures::channel::mpsc::Sender<Message>,
) {
    // Read connections independently: a client waiting to write must neither
    // lose its command nor hold every keybind behind it. A JoinSet bounds and
    // cancels the outstanding readers when this subscription is dropped.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        log::error!("accepting control connection: {error}");
                        return;
                    }
                };
                let mut sender = sender.clone();
                connections.spawn(async move {
                    match read_command(stream).await {
                        Ok(command) => { let _ = sender.send(Message::Control(command)).await; }
                        Err(error) => log::warn!("control socket: {error:#}"),
                    }
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// Accept control connections for as long as the bar runs.
pub fn subscription() -> Subscription<Message> {
    Subscription::run(|| {
        cosmic::iced::stream::channel(4, async move |sender| {
            let path = socket_path();
            // A socket left behind by a crashed bar would block binding.
            if std::fs::metadata(&path).is_ok()
                && std::os::unix::net::UnixStream::connect(&path).is_err()
            {
                let _ = std::fs::remove_file(&path);
            }
            let listener = match tokio::net::UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(error) => {
                    log::error!("control socket {}: {error}", path.display());
                    return;
                }
            };
            log::info!("control socket at {}", path.display());
            serve(listener, sender).await;
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn rejects_invalid_cli_commands_and_extra_arguments() {
        for line in [
            "",
            "wat",
            "toggle",
            "toggle missing",
            "toggle network extra",
            "close extra",
            "reload extra",
        ] {
            assert!(Command::parse(line).is_err(), "{line}");
        }
        assert_eq!(
            Command::parse("brightness-mode late evening").unwrap(),
            Command::BrightnessMode(Some("late evening".into()))
        );
    }

    #[tokio::test]
    async fn reads_commands_written_after_accept_and_in_fragments() {
        let (reader, mut writer) = tokio::net::UnixStream::pair().unwrap();
        let received = tokio::spawn(read_command(reader));
        tokio::task::yield_now().await;
        assert!(!received.is_finished());
        writer.write_all(b"toggle net").await.unwrap();
        tokio::task::yield_now().await;
        assert!(!received.is_finished());
        writer.write_all(b"work").await.unwrap();
        tokio::task::yield_now().await;
        writer.write_all(b"\n").await.unwrap();
        assert_eq!(
            received.await.unwrap().unwrap(),
            Command::Toggle(ModuleId::Network)
        );
    }

    #[tokio::test]
    async fn rejects_oversized_commands_before_newline() {
        let (reader, mut writer) = tokio::net::UnixStream::pair().unwrap();
        let received = tokio::spawn(read_command(reader));
        writer
            .write_all(&vec![b'a'; MAX_COMMAND_BYTES])
            .await
            .unwrap();
        assert!(
            received
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("too long")
        );
    }

    #[tokio::test]
    async fn waiting_client_does_not_block_another_command() {
        let path = std::env::temp_dir().join(format!(
            "cosmicbar-control-test-{}.sock",
            std::process::id()
        ));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (sender, mut messages) = cosmic::iced::futures::channel::mpsc::channel(4);
        let server = tokio::spawn(serve(listener, sender));
        let _waiting = tokio::net::UnixStream::connect(&path).await.unwrap();
        let mut writer = tokio::net::UnixStream::connect(&path).await.unwrap();
        writer.write_all(b"close\n").await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), messages.next())
                .await
                .unwrap(),
            Some(Message::Control(Command::Close))
        ));
        server.abort();
        let _ = server.await;
        std::fs::remove_file(path).unwrap();
    }
}
