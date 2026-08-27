//! Memory: used/total and percentage, with a breakdown popup.
//!
//! Replaces waybar's `memory` module, whose whole tooltip was one line of pango
//! (`Memory Used: {used:0.1f} GB / {total:0.1f} GB`). `/proc/meminfo` has no
//! push interface — it is a synthesised text file, and inotify never fires on
//! procfs — so this module polls on a 2s timer inside its own subscription
//! (waybar polled at 10s), on a blocking thread because the popup's
//! process list opens `/proc/<pid>/statm` for every process.

use std::sync::Arc;
use std::time::Duration;

use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Color, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card};
use crate::theme::{Island, Palette};

/// waybar: `#memory` sits on `@cpu` = surface0.
pub const ISLAND: Island = Island::Start;

const POLL: Duration = Duration::from_secs(2);

/// waybar `memory.jsonc`: `states.warning` / `states.critical`.
const WARNING: f32 = 75.0;
const CRITICAL: f32 = 90.0;

/// md-chip: waybar's memory glyph.
const ICON: &str = "\u{f061a}";
/// md-alert_box: waybar's `format-warning`/`format-critical` for memory.
const ICON_ALERT: &str = "\u{f0027}";
/// md-swap_horizontal, for the swap row.
const ICON_SWAP: &str = "\u{f04e1}";
/// md-database, for the page-cache row.
const ICON_CACHE: &str = "\u{f01bc}";
/// md-checkbox_blank_circle_outline, for the available row.
const ICON_FREE: &str = "\u{f0130}";

/// Height of the usage meter, and of the slimmer bar under each process.
const METER_HEIGHT: f32 = 7.0;
const PROCESS_METER_HEIGHT: f32 = 3.0;
/// Width of the label column in every detail row. Fixed rather than
/// shrink-to-fit: it is what puts the available, cache and swap values in one
/// column instead of at three different indents.
const LABEL_WIDTH: f32 = 76.0;
/// Processes listed in the popup.
const TOP_PROCESSES: usize = 5;
/// x86-64 base page size; `/proc/<pid>/statm` counts pages, and this kernel
/// has no huge base pages.
const PAGE_BYTES: u64 = 4096;
const KIB: f64 = 1024.0;

#[derive(Debug, Clone)]
pub enum Event {
    Sample(Arc<Sample>),
}

/// One poll of `/proc/meminfo`, in kibibytes as the file reports them.
#[derive(Debug, Default)]
pub struct Sample {
    total_kib: u64,
    available_kib: u64,
    cached_kib: u64,
    swap_total_kib: u64,
    swap_free_kib: u64,
    /// Only gathered while the popup is on screen; empty otherwise.
    top: Vec<TopProcess>,
}

impl Sample {
    /// waybar computes `used = MemTotal - MemAvailable`, and its percentage
    /// from that; anything else disagrees with the bar it replaces.
    fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    fn percent(&self) -> f32 {
        if self.total_kib == 0 {
            return 0.0;
        }
        self.used_kib() as f32 * 100.0 / self.total_kib as f32
    }

    fn swap_used_kib(&self) -> u64 {
        self.swap_total_kib.saturating_sub(self.swap_free_kib)
    }
}

/// What the bar cell draws: used and total as they are formatted (a tenth of a
/// GiB, whole GiB), the whole-percent reading, and the colour tier. Two samples
/// that agree here paint the same cell.
type BarKey = (i64, i64, i32, u8);

impl Sample {
    fn bar_key(&self) -> BarKey {
        let percent = self.percent();
        (
            (gib(self.used_kib()) * 10.0).round() as i64,
            gib(self.total_kib).round() as i64,
            percent.round() as i32,
            tier(percent),
        )
    }
}

#[derive(Debug)]
struct TopProcess {
    pid: u32,
    name: String,
    rss_bytes: u64,
}

#[derive(Debug, Default)]
pub struct State {
    sample: Option<Arc<Sample>>,
}

/// `open` is part of the subscription's identity, so opening the popup
/// restarts this stream with the per-process scan switched on.
fn stream(open: &bool) -> impl Stream<Item = Message> + use<> {
    let detailed = *open;
    cosmic::iced::stream::channel(4, async move |mut sender| {
        // What the last sample sent to the bar drew, while the popup is shut.
        let mut drawn = None;
        loop {
            // The meminfo read is trivial, the per-process scan is not; both go
            // to a blocking thread so the surface's executor keeps running.
            match tokio::task::spawn_blocking(move || sample(detailed)).await {
                Ok(Some(sample)) => {
                    // Used memory holds still for minutes at a time on an idle
                    // desktop: a poll that reads back the same cell is not worth
                    // waking the bar for.
                    let key = sample.bar_key();
                    if detailed || drawn != Some(key) {
                        drawn = Some(key);
                        if sender
                            .send(event_message(Event::Sample(Arc::new(sample))))
                            .await
                            .is_err()
                        {
                            // The bar dropped the subscription.
                            return;
                        }
                    }
                }
                // Unreadable /proc/meminfo, or a sampling thread that died:
                // leave the last value on screen and try again next tick rather
                // than ending the subscription.
                Ok(None) => log::debug!("/proc/meminfo unreadable"),
                Err(error) => log::debug!("memory sampler failed: {error}"),
            }
            tokio::time::sleep(POLL).await;
        }
    })
}

impl State {
    /// `open` selects the sampling depth: the RSS ranking is popup-only.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        Subscription::run_with(open, stream)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Sample(sample) => self.sample = Some(sample),
        }
        Task::none()
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let sample = self.sample.as_ref()?;
        let percent = sample.percent();
        let (icon, color) = state(percent, &ctx.palette);
        let total = gib(sample.total_kib);
        Some(crate::theme::label_fixed(
            icon,
            format!("{:.1}/{total:.0}G {percent:.0}%", gib(sample.used_kib())),
            // The widest reading this machine can produce: its own total, in
            // both halves. A field measured from the data cannot drift.
            &format!("{total:.1}/{total:.0}G 100%"),
            ctx.font_size,
            cosmic::theme::Text::Color(color),
        ))
    }

    /// Data arrives on this module's own timer; the bar clock stays slow.
    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.sample.is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let sample = self.sample.as_ref()?;
        let palette = &ctx.palette;
        let percent = sample.percent();
        let (_, color) = state(percent, palette);

        let swap = if sample.swap_total_kib == 0 {
            "none".to_string()
        } else {
            format!(
                "{:.1} / {:.1} GiB",
                gib(sample.swap_used_kib()),
                gib(sample.swap_total_kib)
            )
        };
        // Any swap in use on a 96 GiB box is worth noticing.
        let swap_color = if sample.swap_used_kib() > 0 {
            palette.peach
        } else {
            palette.muted()
        };

        let mut card = Card::new()
            .block(
                popup::column()
                    .push(popup::split(
                        popup::title(
                            format!(
                                "{:.1} GiB of {:.1} GiB",
                                gib(sample.used_kib()),
                                gib(sample.total_kib)
                            ),
                            ctx,
                        ),
                        [popup::title(format!("{percent:.0}%"), ctx)
                            .class(cosmic::theme::Text::Color(color))
                            .into()],
                    ))
                    .push(meter(percent / 100.0, palette, METER_HEIGHT)),
            )
            .block(
                popup::column()
                    .push(popup::section("breakdown", ctx))
                    .push(row(
                        ICON_FREE,
                        "available",
                        format!("{:.1} GiB", gib(sample.available_kib)),
                        palette.green,
                        ctx,
                    ))
                    .push(row(
                        ICON_CACHE,
                        "page cache",
                        format!("{:.1} GiB", gib(sample.cached_kib)),
                        palette.blue,
                        ctx,
                    ))
                    .push(row(ICON_SWAP, "swap", swap, swap_color, ctx)),
            );

        if !sample.top.is_empty() {
            // Scaled against the biggest listed process, not total RAM: on
            // 96 GiB every bar would otherwise be a sliver.
            let largest = sample
                .top
                .iter()
                .map(|process| process.rss_bytes)
                .max()
                .unwrap_or(1)
                .max(1);
            let mut block = popup::column().push(popup::section("processes", ctx));
            for process in &sample.top {
                block = block.push(
                    popup::lines()
                        .push(popup::split(
                            popup::item(process.name.as_str(), ctx),
                            [
                                popup::detail(format!("{}", process.pid), ctx)
                                    .class(cosmic::theme::Text::Color(palette.overlay0))
                                    .into(),
                                popup::detail(
                                    format!(
                                        "{:>5.0} MiB",
                                        process.rss_bytes as f64 / (KIB * KIB)
                                    ),
                                    ctx,
                                )
                                .class(cosmic::theme::Text::Color(palette.mauve))
                                .into(),
                            ],
                        ))
                        .push(bar(
                            process.rss_bytes as f32 / largest as f32,
                            palette.mauve,
                            palette,
                            PROCESS_METER_HEIGHT,
                        )),
                );
            }
            card = card.block(block);
        }

        Some(card.build())
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Memory(event))
}

/// Which of waybar's three memory states a reading is in: plain below 75%,
/// warning glyph and colour from 75%, critical colour from 90%. Split out from
/// the colour so a sample can be compared for "draws the same" without a
/// palette in hand.
fn tier(percent: f32) -> u8 {
    (percent >= WARNING) as u8 + (percent >= CRITICAL) as u8
}

fn state(percent: f32, palette: &Palette) -> (&'static str, Color) {
    match tier(percent) {
        0 => (ICON, palette.fg()),
        1 => (ICON_ALERT, palette.yellow),
        _ => (ICON_ALERT, palette.red),
    }
}

fn gib(kib: u64) -> f64 {
    kib as f64 / (KIB * KIB)
}

/// One detail row: its glyph, its label in the column every other label in the
/// card shares, and its value. The glyph takes the value's colour because what
/// it reports is the state of that reading.
fn row<'a>(
    icon: &'a str,
    label: &'a str,
    value: String,
    value_color: Color,
    ctx: &Ctx,
) -> Element<'a, Message> {
    widget::Row::new()
        .push(
            crate::theme::icon_text(icon)
                .size(ctx.small())
                .class(cosmic::theme::Text::Color(value_color)),
        )
        .push(popup::section(label, ctx).width(Length::Fixed(LABEL_WIDTH)))
        .push(
            popup::detail(value, ctx)
                .class(cosmic::theme::Text::Color(value_color))
                .width(Length::Fill),
        )
        .align_y(Alignment::Center)
        .spacing(popup::ROW_GAP)
        .into()
}

/// The usage meter, coloured by the same thresholds as the bar text.
fn meter<'a>(fraction: f32, palette: &Palette, height: f32) -> Element<'a, Message> {
    let percent = fraction * 100.0;
    let color = if percent >= CRITICAL {
        palette.red
    } else if percent >= WARNING {
        palette.yellow
    } else {
        palette.mauve
    };
    bar(fraction, color, palette, height)
}

/// A filled bar built from two rounded rectangles; `progress_bar::linear` has
/// no per-value colour and the thresholds are the point of the meter.
fn bar<'a>(fraction: f32, color: Color, palette: &Palette, height: f32) -> Element<'a, Message> {
    let fraction = fraction.clamp(0.0, 1.0);
    let filled = (fraction * 1000.0).round() as u16;
    let mut row = widget::Row::new().width(Length::Fill);
    if filled > 0 {
        row = row.push(segment(color, Length::FillPortion(filled), height));
    }
    if filled < 1000 {
        row = row.push(segment(
            palette.surface1,
            Length::FillPortion(1000 - filled),
            height,
        ));
    }
    row.height(Length::Fixed(height)).into()
}

fn segment<'a>(color: Color, width: Length, height: f32) -> Element<'a, Message> {
    widget::space::horizontal()
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .apply(widget::container)
        .width(width)
        .height(Length::Fixed(height))
        .class(cosmic::theme::Container::custom(move |_theme| {
            widget::container::Style {
                background: Some(cosmic::iced::Background::Color(color)),
                border: cosmic::iced::Border {
                    radius: (height / 2.0).into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into()
}

/// `None` only when `/proc/meminfo` is unreadable, which keeps the last good
/// value on screen instead of showing zeroes. `detailed` adds the popup-only
/// per-process scan.
fn sample(detailed: bool) -> Option<Sample> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut sample = Sample::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // Every field used here is reported in kB (= KiB, despite the unit).
        let Some(kib) = value
            .split_whitespace()
            .next()
            .and_then(|number| number.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "MemTotal" => sample.total_kib = kib,
            "MemAvailable" => sample.available_kib = kib,
            "Cached" => sample.cached_kib = kib,
            "SwapTotal" => sample.swap_total_kib = kib,
            "SwapFree" => sample.swap_free_kib = kib,
            _ => continue,
        }
    }
    if sample.total_kib == 0 {
        return None;
    }
    if detailed {
        sample.top = top_processes();
    }
    Some(sample)
}

/// The largest resident processes owned by this user.
fn top_processes() -> Vec<TopProcess> {
    use std::os::unix::fs::MetadataExt;

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let uid = *UID;

    let mut ranked: Vec<(u32, u64)> = Vec::with_capacity(512);
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // Owner of /proc/<pid> is the process's real uid.
        if entry.metadata().ok().map(|meta| meta.uid()) != Some(uid) {
            continue;
        }
        // statm: size resident shared text lib data dirty, in pages.
        let Ok(statm) = std::fs::read_to_string(entry.path().join("statm")) else {
            continue;
        };
        let Some(resident) = statm
            .split_whitespace()
            .nth(1)
            .and_then(|pages| pages.parse::<u64>().ok())
        else {
            continue;
        };
        ranked.push((pid, resident * PAGE_BYTES));
    }

    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    ranked.truncate(TOP_PROCESSES);
    ranked
        .into_iter()
        .map(|(pid, rss_bytes)| TopProcess {
            pid,
            // Read only for the few winners: one extra open per process would
            // double the syscalls for a list that shows five names.
            name: std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|name| name.trim().to_string())
                .unwrap_or_else(|_| format!("[{pid}]")),
            rss_bytes,
        })
        .collect()
}

/// Owner of this bar. `/proc/self`'s owner is this process's real uid, and it
/// cannot change, so it is resolved once for the life of the bar.
static UID: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid())
        .unwrap_or(0)
});
