//! Extension protocol: a bar cell and its popup, driven by another process.
//!
//! An extension is a long-lived program the bar spawns once. It writes one JSON
//! object per line on stdout — a *frame*, everything the bar should draw for it
//! right now — and reads one JSON object per line on stdin: whether its popup is
//! open, and which of its buttons was pressed.
//!
//! The shape is deliberately push-only. Nothing here has an interval, because a
//! bar that polls is the thing this program exists to replace: an extension
//! sends a frame when its own source told it something changed, and the bar
//! draws exactly what it was last sent.
//!
//! ```text
//! bar  -> {"popup":true}                    the popup just opened
//! ext  -> {"cell":{"glyph":"","text":"2 running"},"popup":[...]}
//! bar  -> {"action":"cancel:2910"}          a popup button was pressed
//! ext  -> {"cell":...,"popup":[...]}        the answer is the next frame
//! ```
//!
//! A frame's `header` is the one part of a popup that does not scroll: what the
//! popup is and the verb that acts on all of it — a queue and its pause, an
//! adapter and its switch — belong there rather than at the bottom of a list
//! that walks away from them.
//!
//! The protocol is a drawing contract, not a widget toolkit: colours are palette
//! roles rather than hex, so an extension inherits the bar's theme, and the only
//! interactive element is a labelled button on a popup row. `contrib/extensions`
//! holds a working example.

use std::sync::Arc;
use std::time::Duration;

use cosmic::iced::futures::{SinkExt, Stream};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as Process;
use tokio::sync::mpsc;

/// Ladder for an extension that exits: a program that cannot start must not be
/// respawned in a loop, but one that crashed should come back.
const RESTART_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 15, 60];
/// A run this long was healthy, so the next failure starts the ladder over.
const STABLE_RUN: Duration = Duration::from_secs(60);
/// A frame longer than this is a runaway writer, not a bar cell.
const MAX_FRAME_BYTES: usize = 1 << 18;
/// Commands buffered for an extension that is not reading its stdin. Deeper than
/// any burst of clicks a pointer can produce, shallow enough that a wedged
/// extension is noticed instead of growing a queue.
const COMMAND_QUEUE: usize = 16;

/// One frame: everything the bar draws for this extension until the next one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    /// The bar cell. `null` hides the module: an extension with nothing to say
    /// takes no bar space, the way the built-in modules do.
    #[serde(default)]
    pub cell: Option<Cell>,
    /// The popup's header: its title, the state under it, and the action that
    /// applies to the whole thing. Drawn above the list and pinned there, so a
    /// hundred rows cannot scroll it away.
    #[serde(default)]
    pub header: Option<Row>,
    /// Popup body, top to bottom. Empty means the cell is not clickable.
    #[serde(default)]
    pub popup: Vec<Item>,
}

/// The bar cell: a nerd-font glyph, text beside it, and one colour for both.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    #[serde(default)]
    pub glyph: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub color: Role,
}

/// One entry in a popup, in the order the extension listed them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Item {
    /// A line of text on its own.
    Text(Line),
    /// Lines stacked on the left, with an optional button on the right: a job
    /// with a cancel, a device with a connect, a footer with a toggle.
    Row(Row),
    /// A hairline between sections.
    Divider,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub text: String,
    #[serde(default)]
    pub color: Role,
    /// Draw at the popup's secondary size, for detail under a title.
    #[serde(default)]
    pub small: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    #[serde(default)]
    pub lines: Vec<Line>,
    #[serde(default)]
    pub action: Option<Action>,
}

/// A button on a popup row. Pressing it sends `id` back to the extension, which
/// answers with the next frame — the bar never guesses what a press did.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub label: String,
    /// Paint it as a destructive action: cancel, disconnect, kill.
    #[serde(default)]
    pub danger: bool,
    /// A disabled button still reads as an affordance that is spoken for — a
    /// cancel already requested — instead of vanishing.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// Palette roles, so an extension colours by meaning and follows the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Fg,
    Muted,
    /// Faintest readable text: a command line under a job name.
    Faint,
    Accent,
    Green,
    Yellow,
    Peach,
    Red,
}

impl Role {
    pub fn color(self, palette: &crate::theme::Palette) -> cosmic::iced::Color {
        match self {
            Role::Fg => palette.fg(),
            Role::Muted => palette.muted(),
            Role::Faint => palette.overlay0,
            Role::Accent => palette.accent(),
            Role::Green => palette.green,
            Role::Yellow => palette.yellow,
            Role::Peach => palette.peach,
            Role::Red => palette.red,
        }
    }
}

/// What the bar tells an extension. One line of JSON each, same as frames.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum Command {
    /// The popup opened or closed. An extension that has expensive detail —
    /// a process list, a device scan — reads this and gathers it only while the
    /// popup is on screen, exactly as the built-in modules do.
    Popup { popup: bool },
    /// A popup button was pressed.
    Action { action: String },
}

/// What a running extension hands the bar.
#[derive(Debug, Clone)]
pub enum Event {
    /// The process is up; this is the pipe to its stdin.
    Started(mpsc::Sender<Command>),
    Frame(Arc<Frame>),
    /// The process exited or could not be started; the ladder is retrying.
    Stopped,
}

/// Run one extension, restarting it when it exits.
///
/// The command is the identity of this subscription: editing it in the config
/// stops the old program and starts the new one.
pub fn stream(command: Arc<[String]>) -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(8, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = std::time::Instant::now();
            match run(&command, &mut sender).await {
                // The bar dropped the subscription.
                Ok(false) => return,
                Ok(true) => log::debug!("extension {:?} exited", command.first()),
                Err(error) => log::warn!("extension {:?}: {error}", command.first()),
            }
            if sender.send(Event::Stopped).await.is_err() {
                return;
            }
            if started.elapsed() >= STABLE_RUN {
                attempt = 0;
            }
            let delay = RESTART_BACKOFF_SECS[attempt.min(RESTART_BACKOFF_SECS.len() - 1)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

/// One run of the program. `Ok(false)` means the bar is gone and the stream
/// should end; `Ok(true)` means the program exited and the caller should retry.
async fn run(
    command: &[String],
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Event>,
) -> anyhow::Result<bool> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty command"))?;
    let mut child = Process::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr is the extension's log; it belongs in the bar's journal.
        .stderr(std::process::Stdio::inherit())
        // A bar that goes away must not leave its extensions running.
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");

    let (commands, mut inbox) = mpsc::channel(COMMAND_QUEUE);
    if sender.send(Event::Started(commands)).await.is_err() {
        return Ok(false);
    }
    // Writing to the child is its own task: an extension that stops reading
    // must not block the bar's frames behind a full pipe.
    let writer = tokio::spawn(async move {
        while let Some(command) = inbox.recv().await {
            let Ok(mut line) = serde_json::to_vec(&command) else {
                continue;
            };
            line.push(b'\n');
            if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                return;
            }
        }
    });

    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut last: Option<Arc<Frame>> = None;
    let alive = loop {
        line.clear();
        // Capped while the line is being read, not once it is already resident:
        // a writer that never sends a newline must not grow the bar's heap.
        let read = (&mut reader)
            .take(MAX_FRAME_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            // Clean EOF, or a pipe that broke: the process is done either way.
            Ok(0) => break true,
            Ok(_) => {}
            Err(error) => {
                log::debug!("extension {program}: {error}");
                break true;
            }
        }
        if !line.ends_with(b"\n") {
            // The cap, or EOF mid-line. The tail of an over-long frame is not a
            // frame either, and resynchronising to the writer's garbage is worth
            // less than the restart ladder.
            log::warn!("extension {program}: frame over {MAX_FRAME_BYTES} bytes");
            break true;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Frame>(&line) {
            // A malformed frame is the extension's bug: keep the last good one
            // on screen and say so, rather than tearing the module down.
            Err(error) => log::warn!("extension {program}: {error}"),
            Ok(frame) => {
                // A frame identical to the one on screen is not a repaint. An
                // extension that re-emits its whole state per source event is
                // well within the protocol; it must not cost the bar a layout
                // and a redraw apiece.
                if last.as_deref() == Some(&frame) {
                    continue;
                }
                let frame = Arc::new(frame);
                last = Some(frame.clone());
                if sender.send(Event::Frame(frame)).await.is_err() {
                    break false;
                }
            }
        }
    };

    writer.abort();
    // The child is killed on drop, but reaping it here keeps a restart loop from
    // leaving zombies behind between attempts.
    let _ = child.kill().await;
    Ok(alive)
}
