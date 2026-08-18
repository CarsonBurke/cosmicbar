//! Control socket: `cosmicbar toggle network` from a compositor keybind.
//!
//! waybar could only be poked with `pkill -SIGRTMIN+N`, which refreshes a
//! module but cannot open anything. A line-oriented unix socket lets a niri
//! bind drive the bar's popups directly.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Context;
use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;

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
}

impl Command {
    pub fn parse(line: &str) -> anyhow::Result<Self> {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("toggle") => {
                let name = words.next().context("toggle needs a module name")?;
                let module = ModuleId::parse_declared(name)
                    .with_context(|| format!("unknown module `{name}`"))?;
                Ok(Self::Toggle(module))
            }
            Some("close") => Ok(Self::Close),
            Some("reload") => Ok(Self::Reload),
            Some(other) => anyhow::bail!("unknown command `{other}`"),
            None => anyhow::bail!("empty command"),
        }
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
    let path = socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("no bar listening on {}", path.display()))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// Accept control connections for as long as the bar runs.
pub fn subscription() -> Subscription<Message> {
    Subscription::run(|| {
        cosmic::iced::stream::channel(4, async move |mut sender| {
            let path = socket_path();
            // A socket left behind by a crashed bar would block binding.
            if std::fs::metadata(&path).is_ok() && std::os::unix::net::UnixStream::connect(&path).is_err() {
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
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let Ok(stream) = stream.into_std() else { continue };
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_err() {
                    continue;
                }
                match Command::parse(line.trim()) {
                    Ok(command) => {
                        if sender.send(Message::Control(command)).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => log::warn!("control socket: {error:#}"),
                }
            }
        })
    })
}
