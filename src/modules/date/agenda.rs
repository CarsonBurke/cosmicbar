//! Read GNOME Calendar's selected Evolution Data Server calendars.
//!
//! A popup-scoped native Rust worker keeps calendar views alive. libecal
//! expands recurrences, including detached exceptions and calendar timezones.
//! Dropping the stream kills the worker, including any blocked native call.

use std::{
    process::Stdio,
    time::{Duration, Instant},
};

use cosmic::iced::futures::{SinkExt, Stream, channel::mpsc::Sender};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Clone, Deserialize)]
pub(super) struct AgendaEvent {
    pub title: String,
    pub time: String,
}

pub(super) struct Snapshot {
    pub date: jiff::civil::Date,
    pub events: Result<Vec<AgendaEvent>, String>,
}

#[derive(Deserialize)]
struct Reply {
    date: String,
    #[serde(flatten)]
    result: ReplyResult,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReplyResult {
    Events(Vec<AgendaEvent>),
    Error(String),
}

pub(super) fn watch(date: jiff::civil::Date) -> impl Stream<Item = Snapshot> {
    cosmic::iced::stream::channel(1, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            let Err(error) = session(date, &mut sender).await else {
                return;
            };
            if sender
                .send(Snapshot {
                    date,
                    events: Err(error),
                })
                .await
                .is_err()
            {
                return;
            }
            if started.elapsed() >= Duration::from_secs(60) {
                attempt = 0;
            }
            let delay = [1, 2, 5, 10, 30][attempt.min(4)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

async fn session(date: jiff::civil::Date, sender: &mut Sender<Snapshot>) -> Result<(), String> {
    let helper = std::env::current_exe()
        .map_err(|_| "Could not locate the calendar worker".to_owned())?
        .with_file_name("cosmicbar-calendar");
    let mut child = tokio::process::Command::new(helper)
        .arg(date.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Native diagnostics may contain event contents or account credentials.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            "Calendar unavailable: install cosmicbar-calendar and Evolution Data Server".to_owned()
        })?;
    let stdout = child.stdout.take().expect("piped calendar helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    loop {
        // Idle views have no heartbeat. The helper bounds individual native
        // operations and view initialization, not all calendars' combined
        // startup: several healthy sources can legitimately take longer.
        let line = lines
            .next_line()
            .await
            .map_err(|_| "Could not read the calendar helper response".to_owned())?
            .ok_or_else(|| {
                "Calendar connection ended; reconnecting to Evolution Data Server".to_owned()
            })?;
        let reply: Reply = serde_json::from_str(&line)
            .map_err(|_| "Calendar helper returned an invalid response".to_owned())?;
        let date = reply
            .date
            .parse()
            .map_err(|_| "Calendar helper returned an invalid date".to_owned())?;
        let events = match reply.result {
            ReplyResult::Events(events) => events,
            ReplyResult::Error(error) => return Err(error),
        };
        if sender
            .send(Snapshot {
                date,
                events: Ok(events),
            })
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}
