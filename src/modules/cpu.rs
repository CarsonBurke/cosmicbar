//! CPU: package temperature and total usage, with a per-core popup.
//!
//! Replaces waybar's `group/cpu` — the `temperature` module plus
//! `custom/cpu_usage` (`scripts/cpu_usage.sh`), which differenced `/proc/stat`
//! through a file in `/tmp` every 2s.
//!
//! This is one of the few sources the kernel genuinely cannot push: `/proc/stat`
//! is a monotonic jiffy counter that only means something when differenced, and
//! `/sys/class/hwmon/*/temp*_input` is an ordinary file with no poll/inotify
//! semantics (inotify never fires on sysfs attributes). So the subscription
//! samples on a 2s timer — waybar's usage interval — and does its file reads on
//! a blocking thread so a few thousand `/proc/<pid>/stat` opens never stall the
//! compositor frame loop.
//!
//! The hwmon device is resolved by name rather than by path: waybar hardcodes
//! `/sys/class/hwmon/hwmon4/temp1_input`, and hwmon numbering is discovery
//! order, so on this machine that number now belongs to the DIMM sensor
//! (`spd5118`) and waybar has been showing RAM temperature as CPU temperature.

use std::collections::HashMap;
use std::path::PathBuf;
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

/// waybar: `#temperature`/`#custom-cpu_usage` sit on `@temperature` = mantle.
pub const ISLAND: Island = Island::Start;

/// Sampling interval, matching waybar's `custom/cpu_usage`.
const POLL: Duration = Duration::from_secs(2);
/// First delta is taken over a short window so the bar is populated at startup
/// instead of showing nothing for two seconds.
const PRIME: Duration = Duration::from_millis(250);

/// waybar `cpu.jsonc`: `states.warning` / `states.critical`.
const WARNING: f32 = 75.0;
const CRITICAL: f32 = 90.0;
/// waybar `temperature.jsonc`: `critical-threshold`. It declares no warning
/// threshold; Tctl runs hot on Zen 5 by design, so warn only near the limit.
const TEMP_CRITICAL: f32 = 90.0;
const TEMP_WARNING: f32 = 85.0;

/// md-memory: waybar's cpu glyph.
const ICON: &str = "\u{f035b}";
/// md-alert_circle: waybar's `format-warning`/`format-critical` for cpu.
const ICON_ALERT: &str = "\u{f0028}";
/// md-thermometer_low / md-thermometer / md-thermometer_high: waybar's
/// `temperature.format-icons`, ramped across the critical threshold.
const TEMP_ICONS: [&str; 3] = ["\u{f10c3}", "\u{f050f}", "\u{f10c2}"];
/// md-alert: waybar's `temperature.format-critical`.
const TEMP_ALERT: &str = "\u{f0026}";
/// md-speedometer, for the clock row.
const ICON_CLOCK: &str = "\u{f04c5}";
/// md-pulse, for the load row.
const ICON_LOAD: &str = "\u{f0430}";

/// Height of a meter bar, and of the slimmer per-core meters.
const METER_HEIGHT: f32 = 7.0;
const CORE_METER_HEIGHT: f32 = 5.0;
/// Threads per popup column; 24 threads fit as two columns of twelve.
const CORE_COLUMNS: usize = 2;
/// Processes listed in the popup.
const TOP_PROCESSES: usize = 5;
/// The model name is elided rather than allowed to grow, because the total
/// usage sits beside it in the header and a marketing name long enough to push
/// that number off the card would hide the one reading the popup is about.
const MODEL_LIMIT: usize = 32;
/// Width of the label column in every detail row. Fixed rather than
/// shrink-to-fit: it is what puts the load, clock and temperature values in one
/// column instead of at three different indents.
const LABEL_WIDTH: f32 = 76.0;

#[derive(Debug, Clone)]
pub enum Event {
    Sample(Arc<Sample>),
}

/// One poll's worth of CPU state. Built on the sampling thread, then shared.
#[derive(Debug, Default)]
pub struct Sample {
    /// Aggregate busy percentage, computed exactly like `cpu_usage.sh`.
    total: f32,
    /// Per-thread busy percentage, in `/proc/stat` order. Free: it comes out of
    /// the same `/proc/stat` read as the aggregate.
    cores: Vec<f32>,
    /// hwmon readings, `Tctl` first.
    temps: Vec<(String, f32)>,
    /// Package temperature: `Tctl` when present.
    package_c: Option<f32>,
    /// Only sampled while the popup is on screen.
    detail: Option<Detail>,
}

/// What the bar cell draws, and nothing else. Mirrors `view`: the temperature
/// and usage as they are formatted (whole degrees, whole percent) plus the
/// glyph and colour tier each is in. Two samples with the same key paint the
/// same cell, so the second one is not worth a frame.
type BarKey = (Option<(i32, &'static str, u8)>, i32, u8);

impl Sample {
    fn bar_key(&self) -> BarKey {
        let temp = self.package_c.map(|temp| {
            let (icon, tier) = temp_visual(temp);
            (temp.round() as i32, icon, tier)
        });
        (temp, self.total.round() as i32, usage_tier(self.total))
    }
}

/// The part of a sample only the popup shows, and which costs real syscalls:
/// a 50 KiB `/proc/cpuinfo` parse and a `/proc/<pid>/stat` read per process.
#[derive(Debug)]
struct Detail {
    load: [f64; 3],
    /// `/proc/loadavg`'s runnable/total entity count, verbatim.
    entities: String,
    mhz_avg: f32,
    mhz_max: f32,
    /// Shared with the sampler: the model string is read once per process.
    model: Arc<str>,
    top: Vec<TopProcess>,
}

#[derive(Debug)]
struct TopProcess {
    pid: u32,
    name: String,
    /// Percentage of one core, the same convention `top` prints.
    share: f32,
}

#[derive(Debug, Default)]
pub struct State {
    sample: Option<Arc<Sample>>,
}

/// One poller either way; `open` is part of the subscription's identity, so
/// opening the popup restarts the stream with the expensive reads switched on
/// and closing it switches them back off.
fn stream(open: &bool) -> impl Stream<Item = Message> + use<> {
    let detailed = *open;
    cosmic::iced::stream::channel(4, async move |mut sender| {
        let mut sampler = Sampler::default();
        let mut delay = PRIME;
        // What the last sample sent to the bar drew, while the popup is shut.
        let mut drawn = None;
        loop {
            // Blocking file reads, up to ~2k `/proc/<pid>/stat` opens while the
            // popup is open: not work for the executor driving the surface.
            let sampled = tokio::task::spawn_blocking(move || {
                let mut sampler = sampler;
                let sample = sampler.sample(detailed);
                (sampler, sample)
            })
            .await;
            match sampled {
                Ok((next, sample)) => {
                    sampler = next;
                    if let Some(sample) = sample {
                        // With the popup shut the cell is the whole module, and
                        // at rest most polls read back the same two numbers in
                        // the same colour: sending one costs a frame and draws
                        // the identical pixels, so skip it.
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
                }
                // The sampling thread died with its counters; start a fresh
                // baseline rather than killing the module for good.
                Err(error) => {
                    log::debug!("cpu sampler failed: {error}");
                    sampler = Sampler::default();
                }
            }
            tokio::time::sleep(delay).await;
            delay = POLL;
        }
    })
}

impl State {
    /// `open` selects the sampling depth, so the per-process scan only runs
    /// while someone is looking at it.
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
        let palette = &ctx.palette;
        let mut row = widget::Row::new().spacing(8).align_y(Alignment::Center);

        if let Some(temp) = sample.package_c {
            let (icon, color) = temp_state(temp, palette);
            row = row.push(crate::theme::label_fixed(
                icon,
                format!("{temp:.0}°C"),
                "100°C",
                ctx.font_size,
                cosmic::theme::Text::Color(color),
            ));
        }

        let (icon, color) = usage_state(sample.total, palette);
        row = row.push(crate::theme::label_fixed(
            icon,
            format!("{:.0}%", sample.total),
            "100%",
            ctx.font_size,
            cosmic::theme::Text::Color(color),
        ));

        Some(row.into())
    }

    /// The data arrives on this module's own timer, so the bar clock does not
    /// need to be sped up for it.
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
        let (_, usage_color) = usage_state(sample.total, palette);

        let model = sample
            .detail
            .as_ref()
            .map(|detail| &*detail.model)
            .unwrap_or("cpu");
        let mut card = Card::new().block(
            popup::column()
                .push(popup::split(
                    popup::title(elide(model, MODEL_LIMIT), ctx),
                    [popup::title(format!("{:.0}%", sample.total), ctx)
                        .class(cosmic::theme::Text::Color(usage_color))
                        .into()],
                ))
                .push(meter(sample.total / 100.0, palette, METER_HEIGHT)),
        );

        if !sample.cores.is_empty() {
            card = card.block(
                popup::column()
                    .push(popup::section("cores", ctx))
                    .push(core_grid(&sample.cores, ctx)),
            );
        }

        // Present from the first detailed poll, a quarter second after the
        // popup opens.
        if let Some(detail) = &sample.detail {
            let mut block = popup::column()
                .push(popup::section("activity", ctx))
                .push(field(
                    ICON_LOAD,
                    "load",
                    format!(
                        "{:.2} {:.2} {:.2}   {} runnable",
                        detail.load[0], detail.load[1], detail.load[2], detail.entities
                    ),
                    palette.muted(),
                    ctx,
                ));
            if detail.mhz_max > 0.0 {
                block = block.push(field(
                    ICON_CLOCK,
                    "clock",
                    format!(
                        "{:.2} GHz avg   {:.2} GHz peak",
                        detail.mhz_avg / 1000.0,
                        detail.mhz_max / 1000.0
                    ),
                    palette.muted(),
                    ctx,
                ));
            }
            card = card.block(block);
        }

        if !sample.temps.is_empty() {
            let mut block = popup::column().push(popup::section("temperatures", ctx));
            for (label, value) in &sample.temps {
                let (icon, color) = temp_state(*value, palette);
                block = block.push(field(
                    icon,
                    label.as_str(),
                    format!("{value:.1}°C"),
                    color,
                    ctx,
                ));
            }
            card = card.block(block);
        }

        if let Some(detail) = &sample.detail
            && !detail.top.is_empty()
        {
            let mut block = popup::column().push(popup::section("processes", ctx));
            for process in &detail.top {
                block = block.push(popup::split(
                    popup::item(process.name.as_str(), ctx),
                    [
                        popup::detail(format!("{}", process.pid), ctx)
                            .class(cosmic::theme::Text::Color(palette.overlay0))
                            .into(),
                        popup::detail(format!("{:>5.1}%", process.share), ctx)
                            .class(cosmic::theme::Text::Color(palette.blue))
                            .into(),
                    ],
                ));
            }
            card = card.block(block);
        }

        Some(card.build())
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Cpu(event))
}

/// Which of waybar's three cpu states a reading is in: plain below 75%,
/// warning from 75%, critical from 90%. Split out from the colour so a sample
/// can be compared for "draws the same" without a palette in hand.
fn usage_tier(usage: f32) -> u8 {
    (usage >= WARNING) as u8 + (usage >= CRITICAL) as u8
}

fn usage_state(usage: f32, palette: &Palette) -> (&'static str, Color) {
    match usage_tier(usage) {
        0 => (ICON, palette.fg()),
        1 => (ICON_ALERT, palette.yellow),
        _ => (ICON_ALERT, palette.red),
    }
}

/// waybar ramps `format-icons` across `0..critical-threshold` and swaps in
/// `format-critical` at the threshold: the glyph and the colour step
/// separately, so both belong in the visual identity of a reading.
fn temp_visual(temp: f32) -> (&'static str, u8) {
    if temp >= TEMP_CRITICAL {
        return (TEMP_ALERT, 2);
    }
    let step = TEMP_CRITICAL / TEMP_ICONS.len() as f32;
    let index = ((temp / step) as usize).min(TEMP_ICONS.len() - 1);
    (TEMP_ICONS[index], (temp >= TEMP_WARNING) as u8)
}

fn temp_state(temp: f32, palette: &Palette) -> (&'static str, Color) {
    let (icon, tier) = temp_visual(temp);
    let color = match tier {
        0 => palette.fg(),
        1 => palette.yellow,
        _ => palette.red,
    };
    (icon, color)
}

/// Colour a meter by the same thresholds as the bar text, so a hot core is
/// visible without reading the number.
fn meter_color(fraction: f32, palette: &Palette) -> Color {
    let percent = fraction * 100.0;
    if percent >= CRITICAL {
        palette.red
    } else if percent >= WARNING {
        palette.yellow
    } else {
        palette.blue
    }
}

/// A filled bar built from two rounded rectangles: `progress_bar::linear` has
/// no per-value colour, and the thresholds are the point of the meter.
fn meter<'a>(fraction: f32, palette: &Palette, height: f32) -> Element<'a, Message> {
    let fraction = fraction.clamp(0.0, 1.0);
    let filled = (fraction * 1000.0).round() as u16;
    let mut row = widget::Row::new().width(Length::Fill);
    if filled > 0 {
        row = row.push(segment(
            meter_color(fraction, palette),
            Length::FillPortion(filled),
            height,
        ));
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

/// 24 threads in two columns: one row per thread would be taller than the
/// popup, and a single wide row per thread wastes the width.
fn core_grid<'a>(cores: &[f32], ctx: &Ctx) -> Element<'a, Message> {
    // `chunks(0)` panics, and a `/proc/stat` with only the aggregate line
    // leaves this empty.
    let per_column = cores.len().div_ceil(CORE_COLUMNS).max(1);
    let mut columns = widget::Row::new()
        .spacing(popup::ROW_GAP)
        .width(Length::Fill);
    for (index, chunk) in cores.chunks(per_column).enumerate() {
        let start = index * per_column;
        // The threads of one column are the lines of a single reading rather
        // than a list of separate items, so they sit at the tighter gap.
        let mut column = popup::lines().width(Length::FillPortion(1));
        for (offset, &value) in chunk.iter().enumerate() {
            column = column.push(
                widget::Row::new()
                    .push(popup::section(format!("{:>2}", start + offset), ctx))
                    .push(
                        meter(value / 100.0, &ctx.palette, CORE_METER_HEIGHT)
                            .apply(widget::container)
                            .width(Length::Fill),
                    )
                    .push(popup::detail(format!("{value:>3.0}"), ctx))
                    .align_y(Alignment::Center)
                    .spacing(popup::GAP),
            );
        }
        columns = columns.push(column);
    }
    columns.into()
}

/// One detail row: its glyph, its label in the column every other label in the
/// card shares, and its value. The glyph takes the value's colour because what
/// it reports is the state of that reading.
fn field<'a>(
    icon: &'a str,
    label: &'a str,
    value: String,
    color: Color,
    ctx: &Ctx,
) -> Element<'a, Message> {
    widget::Row::new()
        .push(
            crate::theme::icon_text(icon)
                .size(ctx.small())
                .class(cosmic::theme::Text::Color(color)),
        )
        .push(popup::section(label, ctx).width(Length::Fixed(LABEL_WIDTH)))
        .push(
            popup::detail(value, ctx)
                .class(cosmic::theme::Text::Color(color))
                .width(Length::Fill),
        )
        .align_y(Alignment::Center)
        .spacing(popup::ROW_GAP)
        .into()
}

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Jiffy counters for one `/proc/stat` cpu line.
#[derive(Debug, Clone, Copy, Default)]
struct Times {
    total: u64,
    idle: u64,
}

/// Carries the previous poll's counters; owned by the subscription task.
#[derive(Debug, Default)]
struct Sampler {
    /// `None` until the first poll; then the resolved `k10temp` sensors, which
    /// is empty when the device is absent so the lookup is not retried.
    sensors: Option<Vec<(String, PathBuf)>>,
    /// Aggregate line first, then one entry per thread.
    prev: Vec<Times>,
    /// pid -> utime+stime jiffies at the previous poll.
    prev_procs: HashMap<u32, u64>,
    /// Owner of this bar; only its processes are listed.
    uid: Option<u32>,
    /// Model name, resolved on the first detailed poll and then shared.
    model: Option<Arc<str>>,
}

impl Sampler {
    /// `None` on the very first call: a jiffy counter has no meaning until
    /// there is something to subtract it from. `detailed` adds the popup-only
    /// reads, which are the expensive ones.
    fn sample(&mut self, detailed: bool) -> Option<Sample> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let now: Vec<Times> = stat
            .lines()
            .take_while(|line| line.starts_with("cpu"))
            .filter_map(times)
            .collect();
        if now.is_empty() {
            return None;
        }
        if self.prev.len() != now.len() {
            // First poll, or a cpu was hotplugged; wait for a matching pair.
            self.prev = now;
            if detailed {
                self.prev_procs = read_process_times(self.uid());
            }
            return None;
        }

        let busy: Vec<f32> = now
            .iter()
            .zip(&self.prev)
            .map(|(now, before)| percent(*now, *before))
            .collect();
        let total_delta = now[0].total.saturating_sub(self.prev[0].total);
        let threads = now.len() - 1;
        self.prev = now;

        let temps = self.read_temps();
        let package_c = temps
            .iter()
            .find(|(label, _)| label == "Tctl")
            .or_else(|| temps.first())
            .map(|(_, value)| *value);

        let detail = detailed.then(|| {
            let (mhz_avg, mhz_max) = read_clocks();
            let (load, entities) = read_loadavg();
            Detail {
                load,
                entities,
                mhz_avg,
                mhz_max,
                model: self.model(),
                top: self.read_top(total_delta, threads),
            }
        });

        Some(Sample {
            total: busy[0],
            cores: busy[1..].to_vec(),
            temps,
            package_c,
            detail,
        })
    }

    /// The model name cannot change while the process lives.
    fn model(&mut self) -> Arc<str> {
        self.model.get_or_insert_with(read_model).clone()
    }

    fn uid(&mut self) -> u32 {
        *self.uid.get_or_insert_with(|| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata("/proc/self")
                .map(|meta| meta.uid())
                .unwrap_or(0)
        })
    }

    /// hwmon paths never change while the device is bound, so resolve once.
    fn read_temps(&mut self) -> Vec<(String, f32)> {
        let sensors = self.sensors.get_or_insert_with(resolve_sensors);
        sensors
            .iter()
            .filter_map(|(label, path)| {
                let raw = std::fs::read_to_string(path).ok()?;
                let milli: f32 = raw.trim().parse().ok()?;
                Some((label.clone(), milli / 1000.0))
            })
            .collect()
    }

    /// Top processes by CPU time consumed since the previous poll. `total_delta`
    /// spans every thread, so scaling by the thread count converts a share of
    /// the machine into the share-of-one-core number `top` reports.
    fn read_top(&mut self, total_delta: u64, threads: usize) -> Vec<TopProcess> {
        let uid = self.uid();
        let now = read_process_times(uid);
        if total_delta == 0 {
            self.prev_procs = now;
            return Vec::new();
        }

        let scale = 100.0 * threads.max(1) as f32 / total_delta as f32;
        let mut ranked: Vec<(u32, f32)> = now
            .iter()
            .filter_map(|(pid, ticks)| {
                let before = self.prev_procs.get(pid)?;
                let delta = ticks.saturating_sub(*before);
                (delta > 0).then_some((*pid, delta as f32 * scale))
            })
            .collect();
        self.prev_procs = now;
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(TOP_PROCESSES);

        ranked
            .into_iter()
            .map(|(pid, share)| TopProcess {
                pid,
                // Only for the few winners: one read per process would double
                // the syscalls for a list that shows five names.
                name: std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map(|name| name.trim().to_string())
                    .unwrap_or_else(|_| format!("[{pid}]")),
                share,
            })
            .collect()
    }
}

/// `cpu_usage.sh` summed the first eight fields for the total and
/// `idle + iowait` for the idle part; guest time is already counted inside
/// user time, so the later fields must stay out of the sum.
fn times(line: &str) -> Option<Times> {
    let mut fields = line.split_whitespace();
    fields.next()?;
    let values: Vec<u64> = fields
        .take(8)
        .map(|field| field.parse().unwrap_or(0))
        .collect();
    if values.len() < 5 {
        return None;
    }
    Some(Times {
        total: values.iter().sum(),
        idle: values[3] + values[4],
    })
}

fn percent(now: Times, before: Times) -> f32 {
    let total = now.total.saturating_sub(before.total);
    if total == 0 {
        return 0.0;
    }
    let idle = now.idle.saturating_sub(before.idle);
    (total.saturating_sub(idle) as f32 * 100.0 / total as f32).clamp(0.0, 100.0)
}

/// Every `k10temp` sensor that carries a label, `Tctl` (the package control
/// temperature the firmware fan curve uses) first, then the per-CCD dies.
fn resolve_sensors() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let device = entry.path();
        let name = std::fs::read_to_string(device.join("name")).unwrap_or_default();
        if name.trim() != "k10temp" {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&device) else {
            continue;
        };
        let mut sensors: Vec<(String, PathBuf)> = files
            .flatten()
            .filter_map(|file| {
                let path = file.path();
                let stem = path.file_name()?.to_str()?.strip_suffix("_label")?;
                let label = std::fs::read_to_string(&path).ok()?.trim().to_string();
                let input = device.join(format!("{stem}_input"));
                input.exists().then_some((label, input))
            })
            .collect();
        sensors.sort_by(|a, b| (a.0 != "Tctl", &a.0).cmp(&(b.0 != "Tctl", &b.0)));
        return sensors;
    }
    Vec::new()
}

/// Current per-thread clocks: average and peak MHz across `/proc/cpuinfo`.
fn read_clocks() -> (f32, f32) {
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return (0.0, 0.0);
    };
    let mut sum = 0.0f32;
    let mut count = 0u32;
    let mut max = 0.0f32;
    for line in text.lines() {
        let Some(value) = line.strip_prefix("cpu MHz") else {
            continue;
        };
        let Some((_, value)) = value.split_once(':') else {
            continue;
        };
        if let Ok(mhz) = value.trim().parse::<f32>() {
            sum += mhz;
            count += 1;
            max = max.max(mhz);
        }
    }
    let avg = if count == 0 { 0.0 } else { sum / count as f32 };
    (avg, max)
}

/// The `model name` of the first thread, which is the package's marketing name.
/// Fixed for the life of the machine, so the caller reads it once.
fn read_model() -> Arc<str> {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    text.lines()
        .filter_map(|line| line.strip_prefix("model name"))
        .filter_map(|rest| rest.split_once(':'))
        .map(|(_, value)| value.trim().trim_end_matches(" Processor"))
        .next()
        .unwrap_or("cpu")
        .into()
}

fn read_loadavg() -> ([f64; 3], String) {
    let Ok(text) = std::fs::read_to_string("/proc/loadavg") else {
        return ([0.0; 3], String::new());
    };
    let mut fields = text.split_whitespace();
    let mut load = [0.0f64; 3];
    for slot in &mut load {
        *slot = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0.0);
    }
    (load, fields.next().unwrap_or_default().to_string())
}

/// utime+stime jiffies for every process owned by `uid`.
fn read_process_times(uid: u32) -> HashMap<u32, u64> {
    use std::os::unix::fs::MetadataExt;

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashMap::new();
    };
    let mut times = HashMap::with_capacity(512);
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // Owner of /proc/<pid> is the process's real uid; other users'
        // processes are not this bar's business.
        if entry.metadata().ok().map(|meta| meta.uid()) != Some(uid) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The comm field is parenthesised and may contain spaces and ')', so
        // the numeric fields start after the *last* ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        // `rest` starts at `state` (field 3), so utime (14) and stime (15) are
        // the twelfth and thirteenth tokens. Taken straight off the iterator:
        // collecting ~40 fields per process would allocate for every pid.
        let mut fields = rest.split_whitespace().skip(11);
        if let (Some(utime), Some(stime)) = (fields.next(), fields.next())
            && let (Ok(utime), Ok(stime)) = (utime.parse::<u64>(), stime.parse::<u64>())
        {
            times.insert(pid, utime + stime);
        }
    }
    times
}
