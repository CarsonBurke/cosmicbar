//! Pending package updates on Arch/CachyOS.
//!
//! `checkupdates` (pacman-contrib) is the only correct way to ask this without
//! root: it syncs a *private* copy of the repository databases into
//! `$CHECKUPDATES_DB` and diffs that against the installed set, so the system
//! sync database is never touched and a later `pacman -Syu` is not turned into
//! a partial upgrade. Without it the module falls back to `pacman -Qu`, which
//! answers from whatever the last real `-Sy` left behind. AUR updates come
//! from an installed helper, the same list and order `system-update.sh` used.
//!
//! There is no push interface for this. Two timers, for two different costs:
//!
//! - The full check goes over the network, so it runs every 30 minutes.
//! - A local `pacman -Syu` should not leave a stale count on the bar for half
//!   an hour, so the loop also stats `/var/lib/pacman/local` every 30 seconds.
//!   pacman rewrites that directory on every install, removal and upgrade, so
//!   its mtime moves exactly when the local half of the answer changed. One
//!   `stat(2)` twice a minute is cheaper than pulling in an inotify crate and
//!   holding a watch descriptor, and unlike a coalesced inotify queue it
//!   cannot miss an event after a queue overflow.
//!
//! Neither ever runs on the UI thread: both live in the subscription task, and
//! the popup's "check now" goes through a `Task::future`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Subscription};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

/// waybar painted `#custom-system_update` `@tray`, which is `@mantle`.
pub const ISLAND: Island = Island::Join;

/// nf-md-tray_arrow_down: the glyph waybar used for "updates available".
const ICON: &str = "\u{f0120}";
/// nf-md-server_remove: the glyph waybar used for "cannot fetch updates".
const ICON_OFFLINE: &str = "\u{f0491}";

/// Above this many updates the count takes waybar's warning colour, above the
/// second its critical colour. waybar's own CSS set `warning` to `@yellow` and
/// `critical` to `@red`; the thresholds are this module's.
const WARN_AT: usize = 25;
const CRITICAL_AT: usize = 150;

/// Full check interval. The network sync is the expensive part.
const CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// How often the local database directory is stat'd for a local upgrade.
const WATCH_INTERVAL: Duration = Duration::from_secs(30);
/// Directory pacman rewrites on every install, removal and upgrade.
const LOCAL_DB: &str = "/var/lib/pacman/local";
/// A hung mirror must not hold the check open; `system-update.sh` used 10s.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// AUR helpers, in the order `system-update.sh` probed them.
const HELPERS: [&str; 5] = ["aura", "paru", "pikaur", "trizen", "yay"];
/// Rows the popup will render; a 900 package rebuild does not need a widget
/// each.
const LIST_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct Update {
    name: String,
    from: String,
    to: String,
}

#[derive(Clone, Default)]
pub struct Report {
    repo: Vec<Update>,
    aur: Vec<Update>,
    /// Which AUR helper answered, when one is installed.
    helper: Option<&'static str>,
    /// Wall clock of the check, for "last checked" in the popup.
    checked_ms: i64,
    /// Set when the check itself failed; waybar showed this as "Cannot fetch
    /// updates. Right-click to retry."
    error: Option<String>,
}

/// Counts, not the package lists: the bar logs every message it handles, and a
/// full Arch update is hundreds of entries long.
impl std::fmt::Debug for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Report")
            .field("repo", &self.repo.len())
            .field("aur", &self.aur.len())
            .field("helper", &self.helper)
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Checked(Arc<Report>),
    /// A check started, either on a timer or from the popup button.
    Checking,
    /// Popup: run a check now.
    CheckNow,
    /// Popup: open the upgrade in a terminal. The terminal comes from `Ctx`,
    /// which only the view side has, so the event carries it.
    Upgrade { terminal: String },
    /// The upgrade terminal could not be started.
    SpawnFailed(String),
}

#[derive(Debug, Default)]
pub struct State {
    report: Option<Arc<Report>>,
    checking: bool,
    error: Option<String>,
}

impl State {
    /// The timers live here rather than in the bar's tick so a slow mirror can
    /// never stall a frame. Popup state changes nothing about what has to be
    /// checked, so `open` is unused.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::run(stream).map(event_message)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Checked(report) => {
                self.checking = false;
                self.report = Some(report);
                Task::none()
            }
            Event::Checking => {
                self.checking = true;
                Task::none()
            }
            Event::CheckNow => {
                if self.checking {
                    return Task::none();
                }
                self.checking = true;
                Task::future(async move {
                    cosmic::Action::App(event_message(Event::Checked(Arc::new(check().await))))
                })
            }
            Event::Upgrade { terminal } => {
                let helper = self.report.as_ref().and_then(|report| report.helper);
                Self::upgrade_task(terminal, helper)
            }
            Event::SpawnFailed(error) => {
                self.error = Some(error);
                Task::none()
            }
        }
    }

    /// Open the upgrade in a terminal, the way `system-update.sh` did when
    /// waybar ran it under kitty.
    fn upgrade_task(terminal: String, helper: Option<&'static str>) -> Task<Message> {
        // `system-update.sh` ran `sudo pacman -Syu`, then the helper, then a
        // notification and a keypress so the log stays readable. Reproducing
        // that needs one command line for the terminal to run; it is a launch,
        // not a data source.
        let mut script = String::from("sudo pacman -Syu");
        if let Some(helper) = helper {
            script.push_str("; ");
            script.push_str(helper);
            script.push_str(" -Syu");
        }
        script.push_str(
            "; notify-send 'Update Complete' -i package-install; \
             printf '\\nPress enter to exit...'; read -r _",
        );
        Task::future(async move {
            let spawned = tokio::process::Command::new(&terminal)
                .arg("-e")
                .arg("sh")
                .arg("-c")
                .arg(&script)
                .status()
                .await;
            cosmic::Action::App(event_message(match spawned {
                // The terminal closed, so the upgrade is over either way:
                // re-check instead of leaving the old count on the bar.
                Ok(_) => Event::CheckNow,
                Err(error) => Event::SpawnFailed(format!("{terminal}: {error}")),
            }))
        })
    }

    /// `None` hides the module: a fully updated system has nothing to say. A
    /// failed check stays visible, because that is the state you want to see.
    ///
    /// The cell is the glyph alone; how many packages are waiting is the
    /// popup's business, and the colour already says how far behind you are.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let report = self.report.as_ref()?;
        let palette = ctx.palette;

        if report.error.is_some() {
            return Some(
                crate::theme::glyph_only(ICON_OFFLINE, ctx.font_size)
                    .class(cosmic::theme::Text::Color(palette.red))
                    .align_y(Alignment::Center)
                    .into(),
            );
        }

        let total = report.total();
        if total == 0 {
            return None;
        }
        let color = match total {
            _ if self.checking => palette.muted(),
            total if total >= CRITICAL_AT => palette.red,
            total if total >= WARN_AT => palette.yellow,
            _ => palette.fg(),
        };
        Some(
            crate::theme::glyph_only(ICON, ctx.font_size)
                .class(cosmic::theme::Text::Color(color))
                .align_y(Alignment::Center)
                .into(),
        )
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.report.is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let report = self.report.as_ref()?;
        let palette = ctx.palette;

        // Which AUR helper answered is not news every time the popup opens, so
        // the header is the two counts and nothing else.
        let headline = match &report.error {
            Some(error) => format!("check failed: {error}"),
            None if report.helper.is_some() => {
                format!("{} official · {} aur", report.repo.len(), report.aur.len())
            }
            None => format!("{} official", report.repo.len()),
        };
        let mut card = Card::new().block(popup::split(
            popup::title(headline, ctx).class(cosmic::theme::Text::Color(
                if report.error.is_some() {
                    palette.red
                } else {
                    palette.accent()
                },
            )),
            [],
        ));

        let mut list = popup::column();
        let mut shown = 0usize;
        for (label, updates) in [("", &report.repo), ("aur", &report.aur)] {
            if updates.is_empty() {
                continue;
            }
            if !label.is_empty() {
                list = list.push(popup::section(label, ctx));
            }
            for update in updates.iter().take(LIST_LIMIT.saturating_sub(shown)) {
                shown += 1;
                list = list.push(popup::split(
                    popup::item(update.name.as_str(), ctx),
                    [popup::detail(format!("{} → {}", update.from, update.to), ctx).into()],
                ));
            }
        }
        let hidden = report.total().saturating_sub(shown);
        if hidden > 0 {
            list = list.push(popup::detail(format!("… {hidden} more"), ctx));
        }
        if shown > 0 {
            card = card.list(list);
        }

        let checked = if report.checked_ms == 0 {
            "never checked".to_owned()
        } else {
            format!(
                "checked {}",
                crate::bar::local(report.checked_ms).strftime("%H:%M")
            )
        };
        card = card.block(popup::split(
            popup::detail(
                if self.checking {
                    "checking…".to_owned()
                } else {
                    checked
                },
                ctx,
            ),
            [
                popup::chip(
                    "check now",
                    Chip::Plain,
                    ctx,
                    (!self.checking).then(|| event_message(Event::CheckNow)),
                ),
                popup::chip(
                    "update",
                    Chip::Accent,
                    ctx,
                    (report.total() > 0).then(|| {
                        event_message(Event::Upgrade {
                            terminal: ctx.terminal.clone(),
                        })
                    }),
                ),
            ],
        ));

        Some(
            card.maybe(self.error.as_ref().map(|error| {
                popup::detail(error.as_str(), ctx).class(cosmic::theme::Text::Color(palette.red))
            }))
            .build(),
        )
    }

    /// Nothing here changes per second.
    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }
}

impl Report {
    fn total(&self) -> usize {
        self.repo.len() + self.aur.len()
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Updates(event))
}

fn stream() -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(4, async move |mut sender| {
        let mut last_seen = local_db_mtime();
        loop {
            if sender.send(Event::Checking).await.is_err() {
                return;
            }
            let report = check().await;
            if sender.send(Event::Checked(Arc::new(report))).await.is_err() {
                return;
            }

            // Wait out the full interval, but wake early when the local
            // database moved: an upgrade just happened.
            let deadline = tokio::time::Instant::now() + CHECK_INTERVAL;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep(WATCH_INTERVAL.min(deadline - now)).await;
                let mtime = local_db_mtime();
                if mtime != last_seen {
                    last_seen = mtime;
                    break;
                }
            }
        }
    })
}

fn local_db_mtime() -> Option<SystemTime> {
    std::fs::metadata(LOCAL_DB)
        .and_then(|meta| meta.modified())
        .ok()
}

/// One full check: repository updates, plus AUR updates when a helper exists.
async fn check() -> Report {
    let checked_ms = jiff::Timestamp::now().as_millisecond();
    let helper = HELPERS
        .into_iter()
        .find(|helper| which(helper).is_some());

    let repo = match repo_updates().await {
        Ok(repo) => repo,
        Err(error) => {
            return Report {
                helper,
                checked_ms,
                error: Some(error),
                ..Report::default()
            };
        }
    };

    let mut report = Report {
        repo,
        helper,
        checked_ms,
        ..Report::default()
    };
    if let Some(helper) = helper {
        match run(helper, &["-Qua"]).await {
            // A helper with nothing to report exits non-zero and says nothing;
            // that is not a failure.
            Ok(output) => report.aur = parse(&output),
            Err(error) => log::debug!("{helper} -Qua: {error}"),
        }
    }
    report
}

async fn repo_updates() -> Result<Vec<Update>, String> {
    if which("checkupdates").is_some() {
        return run("checkupdates", &[]).await.map(|output| parse(&output));
    }
    // No pacman-contrib: fall back to the plain query, which reports against
    // whatever the last real `pacman -Sy` left in the sync database.
    run("pacman", &["-Qu"]).await.map(|output| parse(&output))
}

/// Run one command and take stdout. `checkupdates` exits 2 with no output when
/// there is nothing to do, and `pacman -Qu` exits 1 in the same case, so an
/// empty stdout is success no matter what the status was.
async fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The child dies with the future if the timeout fires.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;

    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("{program} timed out"))?
        .map_err(|error| format!("{program}: {error}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() && stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            return Err(format!("{program}: {stderr}"));
        }
    }
    Ok(stdout)
}

/// `pkgname 1.0-1 -> 1.1-1`, the format shared by `checkupdates`, `pacman -Qu`
/// and every AUR helper's `-Qua`.
fn parse(output: &str) -> Vec<Update> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let name = *fields.first()?;
            let from = fields.get(1).copied().unwrap_or_default();
            // Field 2 is the arrow, whatever a helper spells it as.
            let to = fields.get(3).copied().unwrap_or(from);
            Some(Update {
                name: name.to_owned(),
                from: from.to_owned(),
                to: to.to_owned(),
            })
        })
        .collect()
}

/// `PATH` lookup, so an absent helper costs nothing but a few `stat` calls.
fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| Path::new(candidate).is_file())
}
