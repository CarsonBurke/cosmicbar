//! GPU: temperature and utilization from NVML, with a full telemetry popup.
//!
//! Replaces waybar's `group/gpu` (`custom/gpu_temp` + `custom/gpu_usage`), which
//! shelled out to `scripts/gpu.sh` — an `nvidia-smi` fork per 5s interval, its
//! output cached through a file in `$XDG_RUNTIME_DIR` because two modules needed
//! the same two numbers. This talks to NVML directly: no process spawn, no cache
//! file, and the popup gets the telemetry `nvidia-smi` was already reading.
//!
//! NVML has no event for utilization or temperature (its event set covers
//! Xid errors and clock-change reasons only), so this polls on a 2s timer —
//! faster than waybar's 5s because there is no longer a fork behind it. The
//! calls are driver ioctls that can block for milliseconds, so they run on a
//! blocking thread.
//!
//! `Nvml` is not `Default`-constructible and holds a `dlopen`ed handle, so it is
//! created lazily inside the subscription task and never touches `State`: the
//! module's state is only the last snapshot it was sent. A machine with no
//! NVIDIA driver therefore produces no snapshot, and `view` hides the module
//! entirely rather than showing zeroes.

use std::sync::Arc;
use std::time::Duration;

use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Color, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::error::NvmlError;

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card};
use crate::theme::{Island, Palette};

/// waybar: `#custom-gpu_temp`/`#custom-gpu_usage` sit on `@memory` = base.
pub const ISLAND: Island = Island::Start;

/// Sampling interval. waybar used 5s because every tick forked `nvidia-smi`.
const POLL: Duration = Duration::from_secs(2);
/// Ladder for a driver that is absent, unloaded or still initialising. It ends
/// at five minutes: a machine with no NVIDIA card must not be probed forever,
/// but a driver loaded later still gets picked up.
const RETRY_BACKOFF_SECS: [u64; 5] = [2, 5, 15, 60, 300];
/// A session this long was healthy, so the next failure starts the ladder over.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// md-expansion_card: waybar's glyph for both gpu modules.
const ICON: &str = "\u{f08ae}";
/// md-alert: the temperature critical glyph, as waybar's `temperature` uses.
const ICON_ALERT: &str = "\u{f0026}";
/// md-database, for the VRAM row.
const ICON_VRAM: &str = "\u{f01bc}";
/// md-flash, for the power row.
const ICON_POWER: &str = "\u{f0241}";
/// md-speedometer, for the clock rows.
const ICON_CLOCK: &str = "\u{f04c5}";
/// md-fan, for the fan row.
const ICON_FAN: &str = "\u{f0210}";
/// md-memory, for the memory-bus utilization row.
const ICON_BUS: &str = "\u{f035b}";

const METER_HEIGHT: f32 = 7.0;
/// Processes listed in the popup, largest VRAM first.
const MAX_PROCESSES: usize = 6;
/// Fallbacks for a device that will not report its own thresholds.
const TEMP_CRITICAL: u32 = 90;
const TEMP_WARNING_MARGIN: u32 = 10;
const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;
/// The device name is elided rather than allowed to grow, because the
/// temperature sits beside it in the header and a name long enough to push that
/// reading off the card would hide what the popup is really reporting.
const NAME_LIMIT: usize = 26;
/// Width of the label column in every detail row. Fixed rather than
/// shrink-to-fit: it is what puts the gauge, clock and fan values in one column
/// instead of at a different indent per label.
const LABEL_WIDTH: f32 = 76.0;

#[derive(Debug, Clone)]
pub enum Event {
    Sample(Arc<Sample>),
}

/// What the bar needs every tick. Every field here comes from a call that a
/// working device always answers.
#[derive(Debug)]
pub struct Sample {
    /// Model name and throttling point: fixed for the life of the device, so
    /// they are read once per NVML session and carried, not re-queried.
    device: DeviceInfo,
    temp_c: u32,
    /// Percent of the sample period with at least one kernel resident.
    gpu_percent: u32,
    vram_used: u64,
    vram_total: u64,
    /// Only sampled while the popup is on screen.
    detail: Option<Detail>,
}

/// Immutable device identity.
#[derive(Debug, Clone)]
struct DeviceInfo {
    name: Arc<str>,
    /// Temperature at which the hardware starts throttling itself, when the
    /// device reports it; that is the critical threshold.
    slowdown_c: Option<u32>,
}

/// Popup-only telemetry: a dozen extra ioctls that nothing in the bar shows.
#[derive(Debug, Default)]
struct Detail {
    /// Percent of the period the memory bus was being read or written.
    memory_percent: u32,
    power_w: Option<f32>,
    power_limit_w: Option<f32>,
    sm_mhz: Option<u32>,
    sm_max_mhz: Option<u32>,
    mem_mhz: Option<u32>,
    mem_max_mhz: Option<u32>,
    fan_percent: Option<u32>,
    processes: Vec<GpuProcess>,
}

#[derive(Debug)]
struct GpuProcess {
    pid: u32,
    name: String,
    /// `None` when the driver will not attribute memory to the process.
    vram_bytes: Option<u64>,
}

#[derive(Debug, Default)]
pub struct State {
    sample: Option<Arc<Sample>>,
}

/// `open` is part of the subscription's identity, so opening the popup restarts
/// this stream with the extra telemetry switched on.
fn stream(open: &bool) -> impl Stream<Item = Message> + use<> {
    let detailed = *open;
    cosmic::iced::stream::channel(4, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = std::time::Instant::now();
            // dlopen plus nvmlInit: blocking, and it fails on a machine with no
            // driver, which is exactly the case that must stay quiet.
            match tokio::task::spawn_blocking(Nvml::init).await {
                Ok(Ok(nvml)) => {
                    if session(&mut sender, nvml, detailed).await.is_none() {
                        // The bar dropped the subscription.
                        return;
                    }
                }
                Ok(Err(error)) => log::debug!("nvml unavailable: {error}"),
                // The blocking pool is gone; so is the app.
                Err(_) => return,
            }
            if started.elapsed() >= STABLE_SESSION {
                attempt = 0;
            }
            let delay = RETRY_BACKOFF_SECS[attempt.min(RETRY_BACKOFF_SECS.len() - 1)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

/// Polls one NVML handle until the device stops answering. `None` means the bar
/// dropped the subscription and the stream must end; `Some(())` means
/// re-initialise after the caller's backoff.
async fn session(
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Message>,
    nvml: Nvml,
    detailed: bool,
) -> Option<()> {
    let Ok((mut nvml, device)) = tokio::task::spawn_blocking(move || {
        let device = device_info(&nvml);
        (nvml, device)
    })
    .await
    else {
        // The blocking task died with the handle; re-initialise.
        return Some(());
    };
    let Ok(device) = device else {
        // No device behind a working NVML: nothing to poll.
        return Some(());
    };

    // What the last sample sent to the bar drew, while the popup is shut. The
    // device's throttling point is fixed for the session, so the two integers
    // the cell prints are its whole visual identity.
    let mut drawn = None;
    loop {
        let info = device.clone();
        let sampled = tokio::task::spawn_blocking(move || {
            let sampled = read_sample(&nvml, info, detailed);
            (nvml, sampled)
        })
        .await;
        let Ok((returned, sampled)) = sampled else {
            // The handle went down with the blocking task; the ladder retries
            // instead of leaving the module dead for the session.
            return Some(());
        };
        nvml = returned;

        match sampled {
            Ok(sample) => {
                // An idle GPU reports the same temperature and 0% for minutes:
                // that is a frame the bar does not need.
                let key = (sample.temp_c, sample.gpu_percent);
                if detailed || drawn != Some(key) {
                    drawn = Some(key);
                    sender
                        .send(event_message(Event::Sample(Arc::new(sample))))
                        .await
                        // Only here does `None` mean the bar is gone.
                        .ok()?;
                }
            }
            Err(error) => {
                // Driver unloaded or the device fell off the bus: drop the
                // handle and let the caller's backoff re-initialise.
                log::debug!("nvml sample failed: {error}");
                return Some(());
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The identity of GPU 0, read once per session.
fn device_info(nvml: &Nvml) -> Result<DeviceInfo, NvmlError> {
    let device = nvml.device_by_index(0)?;
    Ok(DeviceInfo {
        name: device
            .name()
            .unwrap_or_else(|_| "GPU".into())
            .into_boxed_str()
            .into(),
        slowdown_c: device
            .temperature_threshold(TemperatureThreshold::Slowdown)
            .ok(),
    })
}

/// The bar's numbers come from calls a live device always answers, so a failure
/// here means the device is gone. Everything in `Detail` is optional: an
/// unsupported query hides its row instead of dropping the sample.
fn read_sample(nvml: &Nvml, device_info: DeviceInfo, detailed: bool) -> Result<Sample, NvmlError> {
    let device = nvml.device_by_index(0)?;
    let temp_c = device.temperature(TemperatureSensor::Gpu)?;
    let utilization = device.utilization_rates()?;
    let memory = device.memory_info()?;

    let detail = detailed.then(|| Detail {
        memory_percent: utilization.memory,
        // NVML reports power in milliwatts.
        power_w: device.power_usage().ok().map(|mw| mw as f32 / 1000.0),
        power_limit_w: device
            .enforced_power_limit()
            .or_else(|_| device.power_management_limit())
            .ok()
            .map(|mw| mw as f32 / 1000.0),
        sm_mhz: device.clock_info(Clock::SM).ok(),
        sm_max_mhz: device.max_clock_info(Clock::SM).ok(),
        mem_mhz: device.clock_info(Clock::Memory).ok(),
        mem_max_mhz: device.max_clock_info(Clock::Memory).ok(),
        fan_percent: device.fan_speed(0).ok(),
        processes: read_processes(&device),
    });

    Ok(Sample {
        device: device_info,
        temp_c,
        gpu_percent: utilization.gpu,
        vram_used: memory.used,
        vram_total: memory.total,
        detail,
    })
}

/// Everything holding VRAM, biggest first. `nvidia-smi`'s process table lists
/// compute (CUDA) and graphics (GL/Vulkan) contexts side by side, and on a
/// Wayland desktop it is the graphics contexts that account for the resident
/// VRAM, so listing only compute processes would contradict the vram row.
fn read_processes(device: &nvml_wrapper::Device<'_>) -> Vec<GpuProcess> {
    let compute = device.running_compute_processes().unwrap_or_default();
    let graphics = device.running_graphics_processes().unwrap_or_default();
    let mut processes: Vec<GpuProcess> = Vec::with_capacity(compute.len() + graphics.len());

    for process in compute.into_iter().chain(graphics) {
        let vram_bytes = match process.used_gpu_memory {
            UsedGpuMemory::Used(bytes) => Some(bytes),
            UsedGpuMemory::Unavailable => None,
        };
        // A process with both a compute and a graphics context appears in both
        // lists; keep the larger attribution rather than double-counting it.
        if let Some(existing) = processes.iter_mut().find(|held| held.pid == process.pid) {
            existing.vram_bytes = existing.vram_bytes.max(vram_bytes);
            continue;
        }
        processes.push(GpuProcess {
            pid: process.pid,
            // NVML only knows pids; the name comes from procfs, and is missing
            // for another user's process.
            name: std::fs::read_to_string(format!("/proc/{}/comm", process.pid))
                .map(|name| name.trim().to_string())
                .unwrap_or_else(|_| format!("[{}]", process.pid)),
            vram_bytes,
        });
    }

    processes.sort_unstable_by(|a, b| b.vram_bytes.cmp(&a.vram_bytes));
    // A busy desktop can hold a dozen contexts; the popup shows the ones that
    // matter and stays inside its height budget.
    processes.truncate(MAX_PROCESSES);
    processes
}

impl State {
    /// `open` selects the sampling depth: clocks, power, fan and the process
    /// list are a dozen extra ioctls that only the popup shows.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        Subscription::run_with(open, stream)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Sample(sample) => self.sample = Some(sample),
        }
        Task::none()
    }

    /// `None` hides the module: with no NVML there is no GPU to report, and an
    /// empty island would be worse than no island.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let sample = self.sample.as_ref()?;
        let (icon, color) = temp_state(sample.temp_c, sample.device.slowdown_c, &ctx.palette);
        Some(crate::theme::label_fixed(
            icon,
            format!("{}°C {}%", sample.temp_c, sample.gpu_percent),
            "100°C 100%",
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
        let (_, temp_color) = temp_state(sample.temp_c, sample.device.slowdown_c, palette);
        // Present from the first detailed poll, two seconds after the popup
        // opens at worst.
        let detail = sample.detail.as_ref();

        let mut usage = popup::column()
            .push(popup::section("usage", ctx))
            .push(row(
                ICON,
                "utilization",
                format!("{}%", sample.gpu_percent),
                palette.green,
                ctx,
            ))
            .push(bar(
                sample.gpu_percent as f32 / 100.0,
                palette.green,
                palette,
                METER_HEIGHT,
            ))
            .push(row(
                ICON_VRAM,
                "vram",
                format!(
                    "{:.1} / {:.1} GiB",
                    sample.vram_used as f64 / BYTES_PER_GIB,
                    sample.vram_total as f64 / BYTES_PER_GIB
                ),
                palette.mauve,
                ctx,
            ))
            .push(bar(
                fraction(sample.vram_used, sample.vram_total),
                palette.mauve,
                palette,
                METER_HEIGHT,
            ));
        if let Some(detail) = detail {
            if let (Some(power), Some(limit)) = (detail.power_w, detail.power_limit_w) {
                usage = usage
                    .push(row(
                        ICON_POWER,
                        "power",
                        format!("{power:.0} / {limit:.0} W"),
                        palette.peach,
                        ctx,
                    ))
                    .push(bar(power / limit, palette.peach, palette, METER_HEIGHT));
            } else if let Some(power) = detail.power_w {
                usage = usage.push(row(
                    ICON_POWER,
                    "power",
                    format!("{power:.0} W"),
                    palette.peach,
                    ctx,
                ));
            }
            // How busy the memory interface was is another share of the card
            // being used up, so it belongs with the gauges rather than with the
            // clocks it used to sit under.
            usage = usage.push(row(
                ICON_BUS,
                "memory bus",
                format!("{}%", detail.memory_percent),
                palette.teal,
                ctx,
            ));
        }

        let mut card = Card::new()
            .block(popup::split(
                popup::title(elide(&sample.device.name, NAME_LIMIT), ctx),
                [popup::title(
                    match sample.device.slowdown_c {
                        Some(limit) => format!("{}°C / {limit}°C", sample.temp_c),
                        None => format!("{}°C", sample.temp_c),
                    },
                    ctx,
                )
                .class(cosmic::theme::Text::Color(temp_color))
                .into()],
            ))
            .block(usage);

        if let Some(detail) = detail {
            if detail.sm_mhz.is_some() || detail.mem_mhz.is_some() {
                let mut clocks = popup::column().push(popup::section("clocks", ctx));
                if let Some(mhz) = detail.sm_mhz {
                    clocks = clocks.push(row(
                        ICON_CLOCK,
                        "sm clock",
                        clock(mhz, detail.sm_max_mhz),
                        palette.blue,
                        ctx,
                    ));
                }
                if let Some(mhz) = detail.mem_mhz {
                    clocks = clocks.push(row(
                        ICON_CLOCK,
                        "mem clock",
                        clock(mhz, detail.mem_max_mhz),
                        palette.blue,
                        ctx,
                    ));
                }
                card = card.block(clocks);
            }

            if let Some(fan) = detail.fan_percent {
                card = card.block(
                    popup::column()
                        .push(popup::section("cooling", ctx))
                        .push(row(
                            ICON_FAN,
                            "fan",
                            format!("{fan}%"),
                            if fan == 0 { palette.muted() } else { palette.teal },
                            ctx,
                        )),
                );
            }

            let mut processes = popup::column().push(popup::section("processes", ctx));
            if detail.processes.is_empty() {
                processes = processes.push(
                    popup::detail("no processes holding vram", ctx)
                        .class(cosmic::theme::Text::Color(palette.overlay0)),
                );
            }
            for process in &detail.processes {
                processes = processes.push(popup::split(
                    popup::item(process.name.as_str(), ctx),
                    [
                        popup::detail(format!("{}", process.pid), ctx)
                            .class(cosmic::theme::Text::Color(palette.overlay0))
                            .into(),
                        popup::detail(
                            match process.vram_bytes {
                                Some(bytes) => {
                                    format!("{:>5.0} MiB", bytes as f64 / BYTES_PER_MIB)
                                }
                                None => "—".to_string(),
                            },
                            ctx,
                        )
                        .class(cosmic::theme::Text::Color(palette.mauve))
                        .into(),
                    ],
                ));
            }
            card = card.block(processes);
        }

        Some(card.build())
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Gpu(event))
}

/// waybar's gpu modules had no states at all. The device knows its own
/// throttling point, so use that as critical rather than a guessed number, and
/// warn ten degrees below it.
fn temp_state(temp_c: u32, slowdown_c: Option<u32>, palette: &Palette) -> (&'static str, Color) {
    let critical = slowdown_c.unwrap_or(TEMP_CRITICAL);
    if temp_c >= critical {
        (ICON_ALERT, palette.red)
    } else if temp_c + TEMP_WARNING_MARGIN >= critical {
        (ICON, palette.yellow)
    } else {
        (ICON, palette.fg())
    }
}

fn clock(mhz: u32, max: Option<u32>) -> String {
    match max {
        Some(max) => format!("{mhz} / {max} MHz"),
        None => format!("{mhz} MHz"),
    }
}

fn fraction(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    used as f32 / total as f32
}

/// One detail row: its glyph, its label in the column every other label in the
/// card shares, and its value. The glyph takes the value's colour because it is
/// the same reading in another form — the gauge under the row is that colour
/// too.
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

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A filled bar built from two rounded rectangles; `progress_bar::linear` has
/// no per-value colour, and each gauge here wants its own.
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
