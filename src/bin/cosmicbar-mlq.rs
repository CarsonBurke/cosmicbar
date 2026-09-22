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
/// nf-md-close, nf-md-refresh, nf-md-play: a row's quiet verbs.
const ICON_CANCEL: &str = "\u{f0156}";
const ICON_RETRY: &str = "\u{f0450}";
const ICON_RELEASE: &str = "\u{f040a}";
const NAME_LIMIT: usize = 26;
/// A job's name in the popup. Longer than the cell's: the popup has the room,
/// and two sweeps differing only in their last token are why the name exists.
const ROW_NAME_LIMIT: usize = 44;
const REASON_LIMIT: usize = 48;
const MIN_TICK_MS: i64 = 50;
/// How long a cancel stays armed for the press that confirms it.
const ARM_WINDOW: Duration = Duration::from_secs(4);
/// A confirm sooner than this after arming is the same double-click that
/// armed it, not a second decision, and leaves the cancel armed.
const CONFIRM_DELAY: Duration = Duration::from_millis(400);
/// Finished jobs the popup keeps, newest first, and how far back it looks.
const RECENT: usize = 3;
const RECENT_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// Past this share of its time limit, a run's meter warns.
const LIMIT_WARNING: f64 = 0.8;

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

/// The daemon operation a popup button stands for, and its idempotency key:
/// `pause`, `unpause`, or a verb on one job, `cancel:12`.
fn operation(action: &str) -> Result<(Value, String)> {
    let (verb, job) = match action.split_once(':') {
        Some((verb, job)) => (verb, Some(job.trim().parse::<i64>()?)),
        None => (action, None),
    };
    let op = match (verb, job) {
        ("pause" | "unpause", None) => json!({"type": verb}),
        ("cancel", Some(job)) => json!({"type": "cancel", "job": job, "force": false}),
        ("retry" | "release", Some(job)) => json!({"type": verb, "job": job}),
        _ => bail!("unknown action `{action}`"),
    };
    let key = match job {
        Some(job) => format!("cosmicbar-mlq-{verb}-{job}-{}", request_id()),
        None => format!("cosmicbar-mlq-{verb}-{}", request_id()),
    };
    Ok((op, key))
}

async fn act(action: &str) -> Result<()> {
    let (op, key) = operation(action)?;
    request(op, key).await
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
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes}m"),
        // A time limit is set in whole hours more often than not.
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h{minutes:02}m"),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Epoch milliseconds, which mlqd sends as integers but JSON allows as floats.
fn millis(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|value| value as i64))
}

/// `14:02` today, `Mon 14:02` before: when a job finished, which does not
/// need a clock ticking the way `12m ago` would.
fn clock(ms: i64, now: i64) -> String {
    let zone = jiff::tz::TimeZone::system();
    let (Ok(at), Ok(today)) = (
        jiff::Timestamp::from_millisecond(ms),
        jiff::Timestamp::from_millisecond(now),
    ) else {
        return String::new();
    };
    let (at, today) = (at.to_zoned(zone.clone()), today.to_zoned(zone));
    match at.date() == today.date() {
        true => at.strftime("%H:%M").to_string(),
        false => at.strftime("%a %H:%M").to_string(),
    }
}

fn jobs(status: &Value) -> impl Iterator<Item = &Value> {
    status["jobs"].as_array().into_iter().flatten()
}

fn live(status: &Value) -> impl Iterator<Item = &Value> {
    jobs(status).filter(|job| job["finishedAt"].is_null())
}

fn running(status: &Value) -> impl Iterator<Item = &Value> {
    live(status).filter(|job| job["state"] == "running")
}

fn job_id(job: &Value) -> i64 {
    job["id"].as_i64().unwrap_or(-1)
}

fn name(job: &Value) -> &str {
    job["name"].as_str().unwrap_or("?")
}

fn priority(job: &Value) -> i64 {
    job["priority"].as_i64().unwrap_or(0)
}

/// Where a job stands in the popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    /// The scheduler is holding a slot open for it.
    Next,
    /// Ready, and waiting its turn.
    Ready,
    /// Waiting on another job first.
    After,
    Held,
}

/// What a queued job is waiting for, said the way a person would, from mlqd's
/// `eligibility` code (`waiting_for_higher_priority: job 9190 (pri 10)`).
/// Jobs are named rather than numbered: the other job is in the same list.
struct Wait {
    text: String,
    standing: Standing,
}

fn wait(job: &Value, names: &HashMap<i64, &str>) -> Wait {
    let eligibility = job["eligibility"]
        .as_str()
        .filter(|code| !code.is_empty())
        .or_else(|| job["state"].as_str())
        .unwrap_or("");
    let (code, detail) = eligibility.split_once(": ").unwrap_or((eligibility, ""));
    // `job 12 (pri 10)` names its job first; `jobs [3, 4]` lists them all.
    let ids: Vec<i64> = detail
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|digits| digits.parse().ok())
        .collect();
    let named = |id: &i64| {
        names
            .get(id)
            .map_or_else(|| format!("#{id}"), |name| elide(name, ROW_NAME_LIMIT))
    };
    let other = ids
        .first()
        .map(named)
        .unwrap_or_else(|| "another job".into());
    let (text, standing) = match code {
        "protected_drain" => ("next · when a slot frees".into(), Standing::Next),
        "supersedes_lower_priority" => (format!("next · ahead of {other}"), Standing::Next),
        // Behind the job above it in the list, which says so itself: naming
        // it again on every row under it is the whole list repeating one fact.
        "waiting_for_slot" | "waiting_for_higher_priority" => ("queued".into(), Standing::Ready),
        "backfill_window_open" | "backfill_eligible" => {
            (format!("may fit in before {other}"), Standing::Ready)
        }
        "waiting_for_retry_delay" => ("retrying shortly".into(), Standing::Ready),
        "paused" => ("paused".into(), Standing::Ready),
        "admission_blocked" => ("admission blocked".into(), Standing::Ready),
        "behind_backfill_cutoff" | "backfill_bypass_consumed" => {
            (format!("after {other}"), Standing::After)
        }
        "waiting_for_dependencies" => {
            let text = match ids.as_slice() {
                [] => "after its dependencies".into(),
                [one] => format!("after {}", named(one)),
                [one, rest @ ..] => format!("after {} +{}", named(one), rest.len()),
            };
            (text, Standing::After)
        }
        "held" => ("held".into(), Standing::Held),
        // A code this build does not know yet is still better read than hidden.
        other => (other.replace('_', " "), Standing::Ready),
    };
    Wait { text, standing }
}

/// A finished job's `stateReason`, shortened to what the state does not say.
fn outcome(reason: &str) -> Option<String> {
    if let Some(code) = reason.strip_prefix("command exited with code ") {
        return Some(elide(&format!("exit {code}"), REASON_LIMIT));
    }
    if let Some(signal) = reason.strip_prefix("command terminated by signal ") {
        return Some(elide(&format!("signal {signal}"), REASON_LIMIT));
    }
    if let Some(limit) = reason
        .strip_prefix("time limit of ")
        .and_then(|rest| rest.strip_suffix(" ms exceeded"))
        .and_then(|ms| ms.parse().ok())
    {
        return Some(format!("timed out at {}", duration(limit)));
    }
    match reason {
        "" | "cancelled by request" => None,
        "cancelled before start" => Some("before start".into()),
        "cancelled before launch" => Some("before launch".into()),
        reason => Some(elide(reason, REASON_LIMIT)),
    }
}

/// A popup row in the extension protocol's shape. The detail line stays muted
/// whatever it says, so the names are what the eye runs down.
fn row(title: String, detail: String, progress: Option<Value>, action: Option<Value>) -> Value {
    let mut row = json!({
        "lines": [
            {"text": title},
            {"text": detail, "color": "muted", "small": true},
        ],
    });
    if let Some(progress) = progress {
        row["progress"] = progress;
    }
    if let Some(action) = action {
        row["action"] = action;
    }
    json!({"row": row})
}

#[derive(Default)]
struct Extension {
    status: Option<Value>,
    connected: bool,
    error: Option<String>,
    popup_open: bool,
    started: HashMap<i64, i64>,
    /// The job whose cancel was pressed once, and when. The second press within
    /// [`ARM_WINDOW`] cancels it: a training run is one misclick from gone.
    armed: Option<(i64, Instant)>,
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
            self.started
                .entry(id)
                .or_insert_with(|| millis(&job["updatedAt"]).unwrap_or(now).min(now));
        }
        self.started.retain(|id, _| ids.contains(id));
    }

    fn is_armed(&self, job: i64) -> bool {
        self.armed
            .is_some_and(|(armed, at)| armed == job && at.elapsed() < ARM_WINDOW)
    }

    /// A popup button. A cancel only arms on its first press; everything else,
    /// and a cancel's second press, is for the daemon.
    fn press(&mut self, action: String) -> Option<String> {
        let cancel = action
            .strip_prefix("cancel:")
            .and_then(|job| job.trim().parse::<i64>().ok());
        match cancel {
            Some(job) if !self.is_armed(job) => {
                self.armed = Some((job, Instant::now()));
                None
            }
            Some(_)
                if self
                    .armed
                    .is_some_and(|(_, at)| at.elapsed() < CONFIRM_DELAY) =>
            {
                None
            }
            _ => {
                self.armed = None;
                Some(action)
            }
        }
    }

    /// Forget an arm that has run out, so the button settles back and the
    /// cadence stops waking for it.
    fn expire(&mut self) {
        if self.armed.is_some_and(|(_, at)| at.elapsed() >= ARM_WINDOW) {
            self.armed = None;
        }
    }

    fn cancel(&self, job: &Value) -> Value {
        let id = format!("cancel:{}", job_id(job));
        if job["cancelRequested"].as_bool().unwrap_or(false) {
            json!({"id": id, "glyph": ICON_CANCEL, "enabled": false})
        } else if self.is_armed(job_id(job)) {
            json!({"id": id, "label": "cancel?", "danger": true})
        } else {
            json!({"id": id, "glyph": ICON_CANCEL})
        }
    }

    fn cell(&self, status: &Value, now: i64) -> Value {
        let mut active = 0;
        let mut pending = 0;
        let mut headline = None;
        for job in live(status) {
            if matches!(job["state"].as_str(), Some("running" | "starting")) {
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
        if let Some(job) = headline {
            let mut text = elide(name(job), NAME_LIMIT);
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
        }
    }

    /// What the queue is doing, and the verb for all of it.
    fn header(&self, status: &Value, stuck: usize, running: usize, waiting: usize) -> Value {
        let paused = status["paused"].as_bool().unwrap_or(false);
        let mut title = Vec::new();
        if stuck > 0 {
            title.push(format!("{stuck} stuck"));
        }
        if running > 0 {
            title.push(format!("{running} running"));
        }
        if waiting > 0 {
            title.push(format!("{waiting} waiting"));
        }
        let title = match title.is_empty() {
            true => "queue empty".to_owned(),
            false => title.join(" · "),
        };
        let leases = status["activeLeases"].as_u64().unwrap_or(0);
        let (state, color) = if !self.connected {
            ("reconnecting to mlqd".to_owned(), "peach")
        } else if paused {
            ("paused · nothing new starts".to_owned(), "peach")
        } else if status["admissionBlocked"].as_bool().unwrap_or(false) {
            ("admission blocked".to_owned(), "peach")
        } else {
            let slots = match status.get("effectiveLimit").and_then(Value::as_u64) {
                Some(limit) => format!("{leases} of {limit} slots busy"),
                None if leases == 0 => "all slots free".to_owned(),
                None => format!("{leases} slots busy"),
            };
            (slots, "muted")
        };
        let (action, label) = match paused {
            true => ("unpause", "resume"),
            false => ("pause", "pause"),
        };
        json!({
            "lines": [
                {"text": title},
                {"text": state, "color": color, "small": true},
            ],
            "action": {"id": action, "label": label},
        })
    }

    fn running_row(&self, job: &Value, now: i64) -> Value {
        let elapsed = self.elapsed_ms(job, now);
        let limit = millis(&job["timeLimitMs"]).filter(|limit| *limit > 0);
        let mut detail = match (job["state"].as_str(), limit) {
            (Some("starting"), _) => "starting".to_owned(),
            (_, Some(limit)) => format!("{} of {}", duration(elapsed), duration(limit)),
            (_, None) => duration(elapsed),
        };
        if priority(job) != 0 {
            detail.push_str(&format!(" · priority {}", priority(job)));
        }
        if job["cancelRequested"].as_bool().unwrap_or(false) {
            detail.push_str(" · cancelling");
        }
        detail.push_str(&format!(" · #{}", job_id(job)));
        let progress = limit.map(|limit| {
            let share = elapsed as f64 / limit as f64;
            json!({
                "value": share.min(1.0),
                "color": if share >= LIMIT_WARNING { "peach" } else { "green" },
            })
        });
        row(
            elide(name(job), ROW_NAME_LIMIT),
            detail,
            progress,
            Some(self.cancel(job)),
        )
    }

    fn waiting_row(&self, job: &Value, wait: &Wait) -> Value {
        let mut detail = wait.text.clone();
        if priority(job) != 0 {
            detail.push_str(&format!(" · priority {}", priority(job)));
        }
        if job["cancelRequested"].as_bool().unwrap_or(false) {
            detail.push_str(" · cancelling");
        }
        detail.push_str(&format!(" · #{}", job_id(job)));
        let action = match wait.standing {
            Standing::Held => {
                json!({"id": format!("release:{}", job_id(job)), "glyph": ICON_RELEASE})
            }
            _ => self.cancel(job),
        };
        row(elide(name(job), ROW_NAME_LIMIT), detail, None, Some(action))
    }

    fn recent_row(job: &Value, now: i64) -> Value {
        let state = job["state"].as_str().unwrap_or("?");
        let word = match state {
            "succeeded" => "done",
            other => other,
        };
        let mut detail = vec![word.to_owned()];
        detail.extend(job["stateReason"].as_str().and_then(outcome));
        detail.extend(millis(&job["finishedAt"]).map(|at| clock(at, now)));
        detail.push(format!("#{}", job_id(job)));
        let retry = matches!(state, "failed" | "lost")
            .then(|| json!({"id": format!("retry:{}", job_id(job)), "glyph": ICON_RETRY}));
        row(
            elide(name(job), ROW_NAME_LIMIT),
            detail.join(" · "),
            None,
            retry,
        )
    }

    fn frame(&self) -> Value {
        let Some(status) = &self.status else {
            // A machine without mlqd should look like a machine with no queue.
            return json!({"cell": null, "popup": []});
        };
        let now = now_ms();
        let names: HashMap<i64, &str> = jobs(status).map(|job| (job_id(job), name(job))).collect();

        let mut attention = Vec::new();
        let mut active = Vec::new();
        let mut waiting = Vec::new();
        let mut finished = Vec::new();
        for job in jobs(status) {
            if let Some(at) = millis(&job["finishedAt"]) {
                if now - at <= RECENT_WINDOW_MS {
                    finished.push((at, job));
                }
                continue;
            }
            match job["state"].as_str() {
                Some("running" | "starting") => active.push(job),
                Some("needs_attention") => attention.push(job),
                _ => waiting.push((wait(job, &names), job)),
            }
        }
        // Longest-running first, the job the cell is already naming.
        active.sort_by_key(|job| std::cmp::Reverse(self.elapsed_ms(job, now)));
        // The order the scheduler will take them in, as far as the snapshot
        // says: the job it is draining for, then by priority and readiness,
        // then what waits on others, then what is held.
        waiting.sort_by_key(|(wait, job)| {
            (
                wait.standing,
                std::cmp::Reverse(priority(job)),
                job["readySequence"].as_i64().unwrap_or(i64::MAX),
                job_id(job),
            )
        });
        finished.sort_by_key(|(at, _)| std::cmp::Reverse(*at));

        let mut popup = Vec::new();
        if let Some(error) = &self.error {
            popup.push(json!({"text": {"text": error, "color": "red", "small": true}}));
        }
        if !attention.is_empty() {
            popup.push(json!({"section": "needs attention"}));
            for job in &attention {
                popup.push(row(
                    elide(name(job), ROW_NAME_LIMIT),
                    format!("run mlq recover · #{}", job_id(job)),
                    None,
                    Some(self.cancel(job)),
                ));
            }
        }
        if !active.is_empty() {
            popup.push(json!({"section": "running"}));
            popup.extend(active.iter().map(|job| self.running_row(job, now)));
        }
        if !waiting.is_empty() {
            popup.push(json!({"section": "up next"}));
            popup.extend(
                waiting
                    .iter()
                    .map(|(wait, job)| self.waiting_row(job, wait)),
            );
        }
        if !finished.is_empty() {
            popup.push(json!({"section": "recent"}));
            popup.extend(
                finished
                    .iter()
                    .take(RECENT)
                    .map(|(_, job)| Self::recent_row(job, now)),
            );
        }

        json!({
            "cell": self.cell(status, now),
            "header": self.header(status, attention.len(), active.len(), waiting.len()),
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

    /// When the next frame is due on its own: an elapsed time about to change
    /// its last digit, or an armed cancel about to settle back.
    fn cadence(&self) -> Option<Duration> {
        let clock = self.status.as_ref().and_then(|status| {
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
        });
        let disarm = self
            .armed
            .map(|(_, at)| ARM_WINDOW.saturating_sub(at.elapsed()));
        clock.into_iter().chain(disarm).min()
    }
}

enum Event {
    Status(Value),
    Disconnected,
    Popup(bool),
    Press(String),
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

fn commands(events: Sender<Event>) {
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
        if let Some(popup) = message.get("popup").and_then(Value::as_bool)
            && events.send(Event::Popup(popup)).is_err()
        {
            return;
        }
        if let Some(action) = message.get("action").and_then(Value::as_str)
            && events.send(Event::Press(action.to_owned())).is_err()
        {
            return;
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
    thread::spawn(move || commands(input_events));
    tokio::spawn(subscription(events));

    let mut extension = Extension::default();
    let mut stdout = io::stdout().lock();
    loop {
        extension.expire();
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
            Event::Popup(open) => {
                extension.popup_open = open;
                // An arm is a question asked in the popup; closing it is a no.
                if !open {
                    extension.armed = None;
                }
            }
            Event::Press(action) => {
                if let Some(action) = extension.press(action) {
                    // The worker outlives the loop; a closed inbox means exit.
                    let _ = actions.send(action);
                }
            }
            Event::ActionResult(error) => extension.error = error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting_for(eligibility: &str) -> Wait {
        let names = HashMap::from([(9190, "gate"), (9191, "ppo")]);
        wait(
            &json!({"state": "queued", "eligibility": eligibility}),
            &names,
        )
    }

    #[test]
    fn eligibility_reads_as_words_naming_the_other_job() {
        let cases = [
            (
                "protected_drain",
                "next · when a slot frees",
                Standing::Next,
            ),
            (
                "supersedes_lower_priority: job 9191 (pri 0)",
                "next · ahead of ppo",
                Standing::Next,
            ),
            (
                "waiting_for_higher_priority: job 9190 (pri 10)",
                "queued",
                Standing::Ready,
            ),
            (
                "backfill_window_open: job 9190",
                "may fit in before gate",
                Standing::Ready,
            ),
            (
                "behind_backfill_cutoff: job 9190",
                "after gate",
                Standing::After,
            ),
            (
                "waiting_for_dependencies: jobs [9190]",
                "after gate",
                Standing::After,
            ),
            (
                "waiting_for_dependencies: jobs [9191, 9190, 7]",
                "after ppo +2",
                Standing::After,
            ),
            (
                "waiting_for_dependencies: jobs [7]",
                "after #7",
                Standing::After,
            ),
            ("held", "held", Standing::Held),
            ("some_future_code", "some future code", Standing::Ready),
        ];
        for (eligibility, text, standing) in cases {
            let wait = waiting_for(eligibility);
            assert_eq!(
                (wait.text.as_str(), wait.standing),
                (text, standing),
                "{eligibility}"
            );
        }
    }

    #[test]
    fn a_missing_eligibility_falls_back_to_the_state() {
        let wait = wait(&json!({"state": "held"}), &HashMap::new());
        assert_eq!(wait.standing, Standing::Held);
    }

    #[test]
    fn actions_become_daemon_operations() {
        let (op, key) = operation("cancel:12").unwrap();
        assert_eq!(op, json!({"type": "cancel", "job": 12, "force": false}));
        assert!(key.starts_with("cosmicbar-mlq-cancel-12-"), "{key}");
        assert_eq!(
            operation("retry:3").unwrap().0,
            json!({"type": "retry", "job": 3})
        );
        assert_eq!(
            operation("release:4").unwrap().0,
            json!({"type": "release", "job": 4})
        );
        assert_eq!(operation("unpause").unwrap().0, json!({"type": "unpause"}));
        for bad in ["pause:1", "cancel", "cancel:x", "drop:1"] {
            assert!(operation(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_cancel_needs_a_second_press() {
        let mut extension = Extension::default();
        assert_eq!(extension.press("cancel:5".into()), None);
        assert!(extension.is_armed(5));
        assert_eq!(extension.cancel(&json!({"id": 5}))["label"], "cancel?");
        // Pressing another job's cancel moves the arm rather than firing.
        assert_eq!(extension.press("cancel:6".into()), None);
        assert!(!extension.is_armed(5));
        extension.armed = Instant::now().checked_sub(CONFIRM_DELAY).map(|at| (6, at));
        assert_eq!(
            extension.press("cancel:6".into()).as_deref(),
            Some("cancel:6")
        );
        assert!(extension.armed.is_none());
    }

    #[test]
    fn a_double_click_only_arms() {
        let mut extension = Extension::default();
        assert_eq!(extension.press("cancel:5".into()), None);
        assert_eq!(extension.press("cancel:5".into()), None);
        assert!(extension.is_armed(5));
    }

    #[test]
    fn any_other_press_disarms_and_goes_through() {
        let mut extension = Extension::default();
        extension.press("cancel:5".into());
        assert_eq!(extension.press("pause".into()).as_deref(), Some("pause"));
        assert_eq!(extension.press("cancel:5".into()), None);
    }

    #[test]
    fn an_arm_runs_out() {
        let mut extension = Extension {
            armed: Instant::now().checked_sub(ARM_WINDOW).map(|at| (5, at)),
            ..Extension::default()
        };
        assert!(!extension.is_armed(5));
        assert_eq!(
            extension.press("cancel:5".into()),
            None,
            "a stale arm re-arms"
        );
        extension.armed = extension
            .armed
            .map(|(job, _)| (job, Instant::now().checked_sub(ARM_WINDOW).unwrap()));
        extension.expire();
        assert!(extension.armed.is_none());
    }

    #[test]
    fn reasons_keep_what_the_state_does_not_say() {
        assert_eq!(
            outcome("command exited with code 137").as_deref(),
            Some("exit 137")
        );
        assert_eq!(outcome("cancelled by request"), None);
        assert_eq!(
            outcome("cancelled before start").as_deref(),
            Some("before start")
        );
        assert_eq!(outcome(""), None);
        assert_eq!(
            outcome("cancelled before launch").as_deref(),
            Some("before launch")
        );
        assert_eq!(
            outcome("command terminated by signal 9").as_deref(),
            Some("signal 9")
        );
        assert_eq!(
            outcome("time limit of 3600000 ms exceeded").as_deref(),
            Some("timed out at 1h")
        );
        assert_eq!(
            outcome("command exited with code 2 (result file missing)").as_deref(),
            Some("exit 2 (result file missing)")
        );
    }

    #[test]
    fn durations_drop_empty_minutes() {
        assert_eq!(duration(42_000), "42s");
        assert_eq!(duration(25 * 60_000), "25m");
        assert_eq!(duration(60 * 60_000), "1h");
        assert_eq!(duration(65 * 60_000), "1h05m");
    }

    #[test]
    fn the_popup_groups_jobs_in_scheduling_order() {
        let now = now_ms();
        let status = json!({
            "activeLeases": 1,
            "effectiveLimit": 2,
            "jobs": [
                {"id": 1, "name": "done", "state": "succeeded", "finishedAt": now - 1000},
                {"id": 2, "name": "ancient", "state": "failed", "finishedAt": now - RECENT_WINDOW_MS - 1},
                {"id": 3, "name": "held", "state": "held", "eligibility": "held"},
                {"id": 4, "name": "low", "state": "queued", "eligibility": "waiting_for_slot", "readySequence": 1},
                {"id": 5, "name": "high", "state": "queued", "priority": 5, "eligibility": "waiting_for_slot", "readySequence": 2},
                {"id": 6, "name": "train", "state": "running", "updatedAt": now - 60_000},
                {"id": 7, "name": "stuck", "state": "needs_attention"},
            ],
        });
        let mut extension = Extension {
            connected: true,
            ..Extension::default()
        };
        extension.note_starts(&status);
        extension.status = Some(status);
        let frame = extension.frame();
        let order: Vec<&str> = frame["popup"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                item["section"]
                    .as_str()
                    .or_else(|| item["row"]["lines"][0]["text"].as_str())
                    .unwrap()
            })
            .collect();
        assert_eq!(
            order,
            [
                "needs attention",
                "stuck",
                "running",
                "train",
                "up next",
                "high",
                "low",
                "held",
                "recent",
                "done",
            ]
        );
        assert_eq!(
            frame["header"]["lines"][0]["text"],
            "1 stuck · 1 running · 3 waiting"
        );
        assert_eq!(frame["header"]["lines"][1]["text"], "1 of 2 slots busy");
    }

    #[test]
    fn a_time_limit_draws_a_meter() {
        let now = now_ms();
        let job = json!({"id": 1, "name": "train", "state": "running", "timeLimitMs": 100_000});
        let mut extension = Extension::default();
        extension.started.insert(1, now - 90_000);
        let row = extension.running_row(&job, now);
        assert_eq!(row["row"]["lines"][1]["text"], "1m of 1m · #1");
        assert_eq!(row["row"]["progress"]["color"], "peach");
        assert!((row["row"]["progress"]["value"].as_f64().unwrap() - 0.9).abs() < 1e-9);
    }
}
