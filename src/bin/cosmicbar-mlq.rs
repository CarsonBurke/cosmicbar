//! Push-driven ML queue extension. See docs/extensions.md for the stdio protocol.
//!
//! A subscription reads daemon snapshots, a separate worker serializes mutations,
//! and stdin never waits for either. Only the main thread owns drawing state and
//! stdout, so an older frame cannot race a newer one onto the bar.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{self, UnboundedSender as Sender};
use tokio::time::timeout;

// Match mlqueue's protocol version and reject oversized prefixes before allocating.
const PROTOCOL_VERSION: u32 = 8;
const MAX_FRAME_BYTES: usize = 1 << 20;
const RECONNECT_BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
const STABLE_SESSION: Duration = Duration::from_secs(60);
const ICON: &str = "\u{f1049}";
const NAME_LIMIT: usize = 26;
const COMMAND_LIMIT: usize = 72;
const MIN_TICK_MS: i64 = 50;

fn socket_path() -> PathBuf {
    let nonempty_env = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(runtime) = nonempty_env("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("mlqueue/mlqd.sock");
    }
    let state = nonempty_env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(nonempty_env("HOME").unwrap_or_else(|| "~".into())).join(".local/state")
        });
    state.join("mlqueue/runtime/mlqd.sock")
}

fn request_id() -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    format!(
        "cosmicbar-mlq-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

async fn write_frame(socket: &mut UnixStream, body: &Value) -> Result<()> {
    let body = serde_json::to_vec(body)?;
    socket
        .write_all(&u32::try_from(body.len())?.to_be_bytes())
        .await?;
    socket.write_all(&body).await?;
    Ok(())
}

async fn read_exactly(socket: &mut UnixStream, bytes: &mut [u8], timed: bool) -> Result<bool> {
    let mut read = 0;
    while read < bytes.len() {
        let operation = socket.read(&mut bytes[read..]);
        let result = if timed {
            self::timeout(Duration::from_secs(5), operation).await?
        } else {
            operation.await
        };
        match result {
            Ok(0) if read == 0 => return Ok(false),
            Ok(0) => bail!("connection closed mid-frame"),
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

async fn read_frame(socket: &mut UnixStream, timed: bool) -> Result<Option<Value>> {
    let mut header = [0; 4];
    // Distinguish a clean close between frames from a truncated prefix/body.
    if !read_exactly(socket, &mut header, timed).await? {
        return Ok(None);
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("frame of {length} bytes: stream desync");
    }
    let mut body = vec![0; length];
    if !read_exactly(socket, &mut body, timed).await? {
        bail!("connection closed mid-frame");
    }
    let response: Value = serde_json::from_slice(&body)?;
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        bail!(
            "{}: {}",
            error["code"].as_str().unwrap_or("None"),
            error["message"].as_str().unwrap_or("None")
        );
    }
    Ok(Some(response))
}

async fn request(op: Value, idempotency_key: String) -> Result<()> {
    let wait = Duration::from_secs(5);
    let mut socket = timeout(wait, UnixStream::connect(socket_path())).await??;
    timeout(
        wait,
        write_frame(
            &mut socket,
            &json!({
                "protocol_version": PROTOCOL_VERSION,
                "request_id": request_id(),
                "idempotency_key": idempotency_key,
                "op": op,
            }),
        ),
    )
    .await??;
    if read_frame(&mut socket, true).await?.is_none() {
        bail!("mlqd closed the connection without replying");
    }
    Ok(())
}

async fn act(action: &str) -> Result<()> {
    match action {
        "pause" | "unpause" => {
            request(
                json!({"type": action}),
                format!("cosmicbar-mlq-{action}-{}", request_id()),
            )
            .await
        }
        _ if action.starts_with("cancel:") => {
            let job: i64 = action[7..].trim().parse()?;
            request(
                json!({"type": "cancel", "job": job, "force": false}),
                format!("cosmicbar-mlq-cancel-{job}-{}", request_id()),
            )
            .await
        }
        _ => bail!("unknown action `{action}`"),
    }
}

fn elide(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        None => text.to_owned(),
        Some(_) => text.chars().take(limit - 1).chain(['…']).collect(),
    }
}

fn duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn live(status: &Value) -> impl Iterator<Item = &Value> {
    status["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|job| job["finishedAt"].is_null())
}

fn running(status: &Value) -> impl Iterator<Item = &Value> {
    live(status).filter(|job| job["state"] == "running")
}

fn job_id(job: &Value) -> i64 {
    job["id"].as_i64().unwrap_or(-1)
}

fn short_command(job: &Value) -> String {
    let mut command = String::new();
    for (index, arg) in job["args"].as_array().into_iter().flatten().enumerate() {
        if index > 0 {
            command.push(' ');
        }
        command.push_str(arg.as_str().unwrap_or("").rsplit('/').next().unwrap_or(""));
    }
    elide(&command, COMMAND_LIMIT)
}

#[derive(Default)]
struct Extension {
    status: Option<Value>,
    connected: bool,
    error: Option<String>,
    popup_open: bool,
    started: HashMap<i64, i64>,
    last: String,
}

impl Extension {
    fn elapsed_ms(&self, job: &Value, now: i64) -> i64 {
        now.saturating_sub(*self.started.get(&job_id(job)).unwrap_or(&now))
            .max(0)
    }

    fn note_starts(&mut self, status: &Value) {
        let now = now_ms();
        let mut ids = HashSet::new();
        for job in running(status) {
            let id = job_id(job);
            ids.insert(id);
            // updatedAt changes on unrelated mutations too; read it only once
            // per run, otherwise changing priority resets the displayed clock.
            self.started.entry(id).or_insert_with(|| {
                job["updatedAt"]
                    .as_i64()
                    .or_else(|| job["updatedAt"].as_f64().map(|value| value as i64))
                    .unwrap_or(now)
                    .min(now)
            });
        }
        self.started.retain(|id, _| ids.contains(id));
    }

    fn frame(&self) -> Value {
        let Some(status) = &self.status else {
            // A machine without mlqd should look like a machine with no queue.
            return json!({"cell": null, "popup": []});
        };
        let now = now_ms();
        let mut active = 0;
        let mut pending = 0;
        let mut headline = None;
        for job in live(status) {
            if job["state"] == "running" {
                active += 1;
                // Keep the first job on a tie, as the original extension did.
                if headline.is_none_or(|old| self.elapsed_ms(job, now) > self.elapsed_ms(old, now))
                {
                    headline = Some(job);
                }
            } else {
                pending += 1;
            }
        }
        let paused = status["paused"].as_bool().unwrap_or(false);
        let cell = if let Some(job) = headline {
            let mut text = elide(job["name"].as_str().unwrap_or("?"), NAME_LIMIT);
            if active > 1 {
                text.push_str(&format!(" +{}", active - 1));
            }
            text.push_str(&format!(" · {}", duration(self.elapsed_ms(job, now))));
            json!({"glyph": ICON, "text": text, "color": if self.connected { "green" } else { "muted" }})
        } else if pending > 0 {
            json!({
                "glyph": ICON,
                "text": format!("{pending} {}", if paused { "paused" } else { "queued" }),
                "color": if paused { "peach" } else { "yellow" },
            })
        } else {
            Value::Null
        };
        let title = if active > 0 {
            format!("{active} running")
        } else if pending > 0 {
            format!("{pending} queued")
        } else {
            "queue empty".to_owned()
        };
        let leases = status["activeLeases"].as_u64().unwrap_or(0);
        let mut detail = match status
            .get("effectiveLimit")
            .filter(|limit| !limit.is_null())
        {
            Some(limit) => format!("{leases}/{limit} leases"),
            None => format!("{leases} leases"),
        };
        if paused {
            detail.push_str(" · paused");
        }
        if !self.connected {
            detail.push_str(" · reconnecting");
        }
        let mut popup = Vec::new();
        for job in live(status) {
            let is_running = job["state"] == "running";
            let state = if is_running {
                format!("running · {}", duration(self.elapsed_ms(job, now)))
            } else {
                job["eligibility"]
                    .as_str()
                    .filter(|state| !state.is_empty())
                    .or_else(|| job["state"].as_str())
                    .unwrap_or("?")
                    .to_owned()
            };
            let cancelling = job["cancelRequested"].as_bool().unwrap_or(false);
            popup.push(json!({"row": {
                "lines": [
                    {"text": format!("#{} {}", job["id"], job["name"].as_str().unwrap_or("?"))},
                    {"text": state, "color": if is_running { "green" } else { "muted" }, "small": true},
                    {"text": short_command(job), "color": "faint", "small": true},
                ],
                "action": {
                    "id": format!("cancel:{}", job["id"]),
                    "label": if cancelling { "cancelling" } else { "cancel" },
                    "danger": true,
                    "enabled": !cancelling,
                },
            }}));
        }
        if let Some(error) = &self.error {
            popup.push(json!({"text": {"text": error, "color": "red", "small": true}}));
        }
        let action = if paused { "unpause" } else { "pause" };
        json!({
            "cell": cell,
            "header": {
                "lines": [
                    {"text": title, "color": if paused { "peach" } else { "fg" }},
                    {"text": detail, "color": "muted", "small": true},
                ],
                "action": {"id": action, "label": action},
            },
            "popup": popup,
        })
    }

    fn emit(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        let frame = self.frame().to_string();
        if frame != self.last {
            stdout.write_all(frame.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            self.last = frame;
        }
        Ok(())
    }

    fn cadence(&self) -> Option<Duration> {
        let status = self.status.as_ref()?;
        let now = now_ms();
        let until_change = |elapsed: i64| {
            let step = if elapsed < 60_000 { 1000 } else { 60_000 };
            step - elapsed % step
        };
        let elapsed = running(status).map(|job| self.elapsed_ms(job, now));
        let wait = if self.popup_open {
            elapsed.map(until_change).min()?
        } else {
            until_change(elapsed.max()?)
        };
        Some(Duration::from_millis(wait.max(MIN_TICK_MS) as u64))
    }
}

enum Event {
    Status(Value),
    Disconnected,
    Popup(bool),
    ActionResult(Option<String>),
}

async fn subscription(events: Sender<Event>) {
    let mut attempt = 0usize;
    loop {
        let started = Instant::now();
        let session = async {
            let mut socket = UnixStream::connect(socket_path()).await?;
            write_frame(
                &mut socket,
                &json!({
                    "protocol_version": PROTOCOL_VERSION,
                    "request_id": request_id(),
                    "op": {"type": "subscribe"},
                }),
            )
            .await?;
            while let Some(mut response) = read_frame(&mut socket, false).await? {
                if response["reply"]["type"] == "status" {
                    events.send(Event::Status(response["reply"].take()))?;
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        if let Err(error) = session.await {
            eprintln!("mlqd subscription ended: {error}");
        }
        if events.send(Event::Disconnected).is_err() {
            return;
        }
        if started.elapsed() >= STABLE_SESSION {
            attempt = 0;
        }
        tokio::time::sleep(Duration::from_secs(
            RECONNECT_BACKOFF[attempt.min(RECONNECT_BACKOFF.len() - 1)],
        ))
        .await;
        attempt = attempt.saturating_add(1);
    }
}

fn commands(events: Sender<Event>, actions: Sender<String>) {
    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("reading command from the bar: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                eprintln!("bad command from the bar: {error}");
                continue;
            }
        };
        if let Some(popup) = message.get("popup").and_then(Value::as_bool) {
            if events.send(Event::Popup(popup)).is_err() {
                return;
            }
        }
        if let Some(action) = message.get("action").and_then(Value::as_str) {
            if actions.send(action.to_owned()).is_err() {
                return;
            }
        }
    }
    // EOF must stop even a blocked socket read, slow mutation, or stdout write.
    std::process::exit(0);
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let (events, mut inbox) = mpsc::unbounded_channel();
    let (actions, mut action_inbox) = mpsc::unbounded_channel::<String>();
    let action_events = events.clone();
    tokio::spawn(async move {
        while let Some(action) = action_inbox.recv().await {
            let error = act(&action).await.err().map(|error| error.to_string());
            if action_events.send(Event::ActionResult(error)).is_err() {
                return;
            }
        }
    });
    let input_events = events.clone();
    thread::spawn(move || commands(input_events, actions));
    tokio::spawn(subscription(events));

    let mut extension = Extension::default();
    let mut stdout = io::stdout().lock();
    loop {
        if let Err(error) = extension.emit(&mut stdout) {
            return if error.kind() == io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(error.into())
            };
        }
        let event = match extension.cadence() {
            Some(wait) => match timeout(wait, inbox.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => return Ok(()),
                Err(_) => continue,
            },
            None => match inbox.recv().await {
                Some(event) => event,
                None => return Ok(()),
            },
        };
        match event {
            Event::Status(status) => {
                extension.note_starts(&status);
                extension.status = Some(status);
                extension.connected = true;
                extension.error = None;
            }
            Event::Disconnected => extension.connected = false,
            Event::Popup(open) => extension.popup_open = open,
            Event::ActionResult(error) => extension.error = error,
        }
    }
}
