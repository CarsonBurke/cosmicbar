//! Brightness, for internal panels *and* external monitors.
//!
//! Waybar needed two modules and two shell scripts for this: `backlight` +
//! `scripts/backlight.sh` (brightnessctl) for a laptop panel, and
//! `custom/brightness` + `scripts/brightness.sh` (ddcutil, a cache directory,
//! `pkill -RTMIN+5 waybar` and a zenity dialog per monitor) for external ones.
//! Here one module owns both backends, the popup has a real slider per display,
//! and scrolling the bar cell nudges the monitor that bar is drawn on.
//!
//! Backends, in order of preference:
//!
//! * `/sys/class/backlight/*` when a panel exists. Reads come from sysfs;
//!   writes go through logind's `Session.SetBrightness`, which is why this needs
//!   no root, no udev rule and no `brightnessctl` suid helper.
//! * DDC/CI over i2c otherwise, by running `ddcutil` as a child process — there
//!   is no Rust binding for it. Measured on this machine: `ddcutil detect
//!   --brief` ≈ 0.8 s, and each `getvcp`/`setvcp` ≈ 0.28 s. That is far too slow
//!   to touch from a view, so displays are detected once, values are cached and
//!   rendered from the cache, writes are coalesced per display (a slider drag
//!   issues one write at a time, always with the newest value), and live re-reads
//!   only happen while the popup is open.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, renderer};
use cosmic::iced::{Alignment, Length, Rectangle, Size, Subscription, Vector, mouse};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::theme::Island;

/// waybar: `@backlight` → base.
pub const ISLAND: Island = Island::Start;

/// Popup content width; see the note in `power.rs`.
const POPUP_WIDTH: f32 = 320.0;

/// Scroll step, matching `custom/brightness`'s `up 5` / `down 5`.
const STEP: u32 = 5;
/// Shortest gap between two accepted wheel notches.
const NUDGE_DEBOUNCE: Duration = Duration::from_millis(80);
/// Presets offered in the popup.
const PRESETS: [u32; 5] = [10, 25, 50, 75, 100];
/// A wedged i2c bus must not pin a task forever.
const DDC_TIMEOUT: Duration = Duration::from_secs(5);
/// Re-read interval while the popup is open, so a change made elsewhere shows up.
const REFRESH: Duration = Duration::from_secs(3);
/// Retry interval while no display has been found yet (monitor plugged in later).
const DETECT_RETRY: Duration = Duration::from_secs(60);
/// sysfs panels never go fully dark, mirroring `brightnessctl -n`.
const SYSFS_FLOOR: u32 = 1;

/// Where one display's brightness is read and written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Sink {
    /// `/sys/class/backlight/<name>`, `max` raw units.
    Backlight { name: String, max: u32 },
    /// External monitor: `bus` is the i2c bus number from `ddcutil detect`,
    /// which survives display renumbering, unlike `--display N`. `max` is the
    /// monitor's own maximum for VCP feature 0x10.
    Ddc { bus: u32, max: u32 },
}

#[derive(Debug, Clone)]
pub struct Found {
    sink: Sink,
    /// Monitor model, or the backlight device name.
    label: String,
    /// DRM connector (`DP-1`), when the backend reports it: lets each bar show
    /// and scroll the display it is actually drawn on.
    connector: Option<String>,
    percent: u32,
}

#[derive(Debug)]
struct Display {
    found: Found,
    /// Newest value the user asked for while a write was in flight.
    pending: Option<u32>,
    writing: bool,
}

/// What a click or a scroll applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    One(usize),
    All,
}

#[derive(Debug, Clone)]
pub enum Event {
    Detected(Arc<Vec<Found>>),
    /// Live values from the popup-only refresh.
    Refreshed(Arc<Vec<(Sink, u32)>>),
    Set(Target, u32),
    Nudge(Target, i32),
    Wrote {
        index: usize,
        percent: u32,
        result: Result<(), String>,
    },
}

#[derive(Debug, Default)]
pub struct State {
    displays: Vec<Display>,
    /// When the last wheel notch was accepted; see [`NUDGE_DEBOUNCE`].
    nudged_at: Option<Instant>,
    error: Option<String>,
}

impl State {
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        if self.displays.is_empty() {
            // Detection is the only thing worth doing until something is found.
            return Subscription::run(detect);
        }
        if !open {
            // Nobody is looking, and a DDC read costs a third of a second.
            return Subscription::none();
        }
        let sinks: Vec<Sink> = self
            .displays
            .iter()
            .map(|display| display.found.sink.clone())
            .collect();
        Subscription::run_with(sinks, |sinks| refresh(sinks.clone()))
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Detected(found) => {
                self.displays = found
                    .iter()
                    .cloned()
                    .map(|found| Display {
                        found,
                        pending: None,
                        writing: false,
                    })
                    .collect();
                Task::none()
            }
            Event::Refreshed(values) => {
                for (sink, percent) in values.iter() {
                    if let Some(display) = self
                        .displays
                        .iter_mut()
                        .find(|display| &display.found.sink == sink)
                    {
                        // Never fight the user: a display we are writing to
                        // keeps the value the pointer picked.
                        if !display.writing && display.pending.is_none() {
                            display.found.percent = *percent;
                        }
                    }
                }
                Task::none()
            }
            Event::Set(target, percent) => self.apply(target, |_| percent),
            Event::Nudge(target, steps) => {
                // niri/winit can deliver two wheel events for one physical
                // notch (`axis_value120` plus the legacy axis), which would
                // double every step. One step per debounce window is also all
                // the i2c bus can absorb: a DDC write takes ~0.3 s.
                let now = Instant::now();
                if steps == 0
                    || self
                        .nudged_at
                        .is_some_and(|last| now.duration_since(last) < NUDGE_DEBOUNCE)
                {
                    return Task::none();
                }
                self.nudged_at = Some(now);
                let delta = steps.signum().saturating_mul(STEP as i32);
                self.apply(target, |current| {
                    (current as i32 + delta).clamp(0, 100) as u32
                })
            }
            Event::Wrote {
                index,
                percent,
                result,
            } => {
                if let Err(error) = &result {
                    log::warn!("brightness write failed: {error}");
                }
                self.error = result.err();
                let Some(display) = self.displays.get_mut(index) else {
                    return Task::none();
                };
                display.writing = false;
                match display.pending.take() {
                    // The user moved on while that write was in flight.
                    Some(newest) if newest != percent => self.write(index, newest),
                    _ => Task::none(),
                }
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if self.displays.is_empty() {
            // No panel, no DDC monitor: nothing to show.
            return None;
        }
        let target = self.target(ctx);
        let percent = self.percent(target);
        let color = if self.error.is_some() {
            ctx.palette.red
        } else {
            ctx.palette.fg()
        };
        Some(
            crate::theme::label(
                glyph(percent),
                format!("{percent}%"),
                ctx.font_size,
                cosmic::theme::Text::Color(color),
            )
            // waybar bound the wheel to `brightness.sh up/down 5`. This cannot
            // be a `mouse_area`: that widget captures every left click over it,
            // which would eat the bar's popup toggle.
            .apply(|label| {
                Wheel::new(label, move |delta| {
                    event_message(Event::Nudge(target, steps(delta)))
                })
            })
            .into(),
        )
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        !self.displays.is_empty()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if self.displays.is_empty() {
            return None;
        }
        let mut body = widget::Column::new()
            .spacing(10)
            .width(Length::Fixed(POPUP_WIDTH));

        for (index, display) in self.displays.iter().enumerate() {
            let percent = display.found.percent;
            let mut heading = widget::Row::new()
                .push(crate::theme::text(glyph(percent)))
                .push(crate::theme::text(display.found.label.clone()).width(Length::Fill));
            if let Some(connector) = &display.found.connector {
                heading = heading.push(
                    crate::theme::text(connector.clone())
                        .size(ctx.small())
                        .class(cosmic::theme::Text::Color(ctx.palette.overlay0)),
                );
            }
            heading = heading.push(
                crate::theme::text(format!("{percent}%"))
                    .class(cosmic::theme::Text::Color(ctx.palette.accent())),
            );

            body = body.push(
                widget::Column::new()
                    .push(heading.spacing(8).align_y(Alignment::Center))
                    .push(
                        widget::slider(0..=100, percent, move |percent| {
                            event_message(Event::Set(Target::One(index), percent))
                        })
                        .step(1u32),
                    )
                    .spacing(4)
                    .width(Length::Fill),
            );
        }

        let mut presets = widget::Row::new()
            .push(
                crate::theme::text("all")
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(ctx.palette.muted())),
            )
            .spacing(6)
            .align_y(Alignment::Center);
        for preset in PRESETS {
            presets = presets.push(
                widget::button::custom(crate::theme::text(format!("{preset}%")))
                    .padding([4, 8])
                    .class(crate::theme::chip(ctx.palette))
                    .on_press(event_message(Event::Set(Target::All, preset))),
            );
        }
        body = body
            .push(widget::divider::horizontal::default())
            .push(presets);

        if let Some(error) = &self.error {
            body = body.push(
                crate::theme::text(error.clone())
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(ctx.palette.red)),
            );
        }

        Some(body.apply(widget::container).padding(12).into())
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// The display this bar cell speaks for: the one on this output when the
    /// connector is known, otherwise every display at once (which is what the
    /// waybar script's averaged readout and paired `up 1 5 && up 2 5` did).
    fn target(&self, ctx: &Ctx) -> Target {
        if let Some(output) = &ctx.output {
            if let Some(index) = self.displays.iter().position(|display| {
                display
                    .found
                    .connector
                    .as_deref()
                    .is_some_and(|connector| connector == output)
            }) {
                return Target::One(index);
            }
        }
        if self.displays.len() == 1 {
            Target::One(0)
        } else {
            Target::All
        }
    }

    /// The number the bar cell shows: one display's value, or the average when
    /// the cell speaks for all of them.
    fn percent(&self, target: Target) -> u32 {
        match target {
            Target::One(index) => self
                .displays
                .get(index)
                .map_or(0, |display| display.found.percent),
            Target::All if self.displays.is_empty() => 0,
            Target::All => {
                let total: u32 = self
                    .displays
                    .iter()
                    .map(|display| display.found.percent)
                    .sum();
                total / self.displays.len() as u32
            }
        }
    }

    /// Change one or every display, coalescing against writes in flight.
    fn apply(&mut self, target: Target, value: impl Fn(u32) -> u32) -> Task<Message> {
        let indices: Vec<usize> = match target {
            Target::One(index) => vec![index],
            Target::All => (0..self.displays.len()).collect(),
        };
        let mut tasks = Vec::with_capacity(indices.len());
        for index in indices {
            let Some(display) = self.displays.get_mut(index) else {
                continue;
            };
            let wanted = value(display.found.percent).min(100);
            if wanted == display.found.percent && display.pending.is_none() {
                continue;
            }
            // Optimistic: the slider and the bar follow the pointer, and the
            // hardware catches up a third of a second later.
            display.found.percent = wanted;
            if display.writing {
                display.pending = Some(wanted);
                continue;
            }
            display.writing = true;
            tasks.push(self.write(index, wanted));
        }
        Task::batch(tasks)
    }

    fn write(&mut self, index: usize, percent: u32) -> Task<Message> {
        let Some(display) = self.displays.get_mut(index) else {
            return Task::none();
        };
        display.writing = true;
        let sink = display.found.sink.clone();
        Task::future(async move {
            cosmic::Action::App(event_message(Event::Wrote {
                index,
                percent,
                result: set(&sink, percent)
                    .await
                    .map_err(|error| format!("{error:#}")),
            }))
        })
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Brightness(event))
}

/// waybar's script picked its icon from an average; the thresholds are the same,
/// with the two middle glyphs put back in ascending order (the script had
/// nf-md-brightness-4 above nf-md-brightness-5).
fn glyph(percent: u32) -> &'static str {
    match percent {
        75.. => "\u{f00e0}",
        50..75 => "\u{f00de}",
        25..50 => "\u{f00dd}",
        _ => "\u{f00dc}",
    }
}

fn steps(delta: mouse::ScrollDelta) -> i32 {
    match delta {
        mouse::ScrollDelta::Lines { y, .. } => y.round() as i32,
        // Touchpads and high-resolution wheels report pixels.
        mouse::ScrollDelta::Pixels { y, .. } => (y / 50.0).round() as i32,
    }
}

/// Probe for displays until something answers.
fn detect() -> impl Stream<Item = Message> {
    cosmic::iced::stream::channel(1, async move |mut sender| {
        loop {
            let found = match backlights().await {
                // A panel exists: DDC is not worth the seconds it costs.
                found if !found.is_empty() => found,
                _ => ddc_detect().await.unwrap_or_else(|error| {
                    log::debug!("ddcutil detect: {error:#}");
                    Vec::new()
                }),
            };
            if !found.is_empty() {
                let _ = sender
                    .send(event_message(Event::Detected(Arc::new(found))))
                    .await;
            }
            tokio::time::sleep(DETECT_RETRY).await;
        }
    })
}

/// Live values, only while the popup is open.
fn refresh(sinks: Vec<Sink>) -> impl Stream<Item = Message> {
    cosmic::iced::stream::channel(1, async move |mut sender| {
        loop {
            let mut values = Vec::with_capacity(sinks.len());
            for sink in &sinks {
                if let Ok(percent) = get(sink).await {
                    values.push((sink.clone(), percent));
                }
            }
            if sender
                .send(event_message(Event::Refreshed(Arc::new(values))))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(REFRESH).await;
        }
    })
}

async fn get(sink: &Sink) -> anyhow::Result<u32> {
    match sink {
        Sink::Backlight { name, max } => {
            let raw = read_number(&format!("/sys/class/backlight/{name}/brightness")).await?;
            Ok(to_percent(raw, *max))
        }
        Sink::Ddc { bus, max } => {
            let (current, _) = ddc_get(*bus).await?;
            Ok(to_percent(current, *max))
        }
    }
}

async fn set(sink: &Sink, percent: u32) -> anyhow::Result<()> {
    match sink {
        Sink::Backlight { name, max } => {
            let raw = from_percent(percent.max(SYSFS_FLOOR), *max);
            set_backlight(name, raw).await
        }
        Sink::Ddc { bus, max } => ddc_set(*bus, from_percent(percent, *max)).await,
    }
}

fn to_percent(raw: u32, max: u32) -> u32 {
    if max == 0 {
        return 0;
    }
    ((f64::from(raw) / f64::from(max)) * 100.0).round().min(100.0) as u32
}

fn from_percent(percent: u32, max: u32) -> u32 {
    ((f64::from(percent.min(100)) / 100.0) * f64::from(max)).round() as u32
}

/// Internal panels, newest kernel naming first (`/sys/class/backlight` holds
/// one directory per panel and is the only interface the kernel offers; there is
/// nothing to subscribe to, hence the popup-gated re-read).
async fn backlights() -> Vec<Found> {
    let mut found = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir("/sys/class/backlight").await else {
        return found;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let base = format!("/sys/class/backlight/{name}");
        let (Ok(max), Ok(current)) = (
            read_number(&format!("{base}/max_brightness")).await,
            read_number(&format!("{base}/brightness")).await,
        ) else {
            continue;
        };
        if max == 0 {
            continue;
        }
        found.push(Found {
            percent: to_percent(current, max),
            sink: Sink::Backlight {
                name: name.clone(),
                max,
            },
            label: name,
            connector: None,
        });
    }
    found
}

async fn read_number(path: &str) -> anyhow::Result<u32> {
    Ok(tokio::fs::read_to_string(path).await?.trim().parse()?)
}

/// Writes through logind, so the bar needs no privilege on
/// `/sys/class/backlight/*/brightness`.
async fn set_backlight(name: &str, raw: u32) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;
    let manager = Login1ManagerProxy::new(&connection).await?;
    let path = manager.get_session_by_pid(std::process::id()).await?;
    Login1SessionProxy::builder(&connection)
        .path(path)?
        .build()
        .await?
        .set_brightness("backlight", name, raw)
        .await?;
    Ok(())
}

/// `ddcutil detect --brief`, parsed into one display per usable monitor.
async fn ddc_detect() -> anyhow::Result<Vec<Found>> {
    let output = ddcutil(&["detect", "--brief"]).await?;
    let mut found = Vec::new();
    let mut bus = None;
    let mut connector = None;
    let mut label = None;

    let mut flush = |bus: &mut Option<u32>, connector: &mut Option<String>, label: &mut Option<String>| {
        if let Some(bus) = bus.take() {
            found.push((bus, connector.take(), label.take()));
        } else {
            connector.take();
            label.take();
        }
    };

    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("Display ") || line.starts_with("Invalid display") {
            flush(&mut bus, &mut connector, &mut label);
        } else if let Some(value) = line.strip_prefix("I2C bus:") {
            bus = value.trim().rsplit('-').next().and_then(|n| n.parse().ok());
        } else if let Some(value) = line.strip_prefix("DRM connector:") {
            // `card1-DP-2` is the same connector niri calls `DP-2`.
            let value = value.trim();
            connector = value
                .split_once('-')
                .map(|(_, rest)| rest.to_string())
                .filter(|rest| !rest.is_empty());
        } else if let Some(value) = line.strip_prefix("Monitor:") {
            // `MFG:MODEL:SERIAL`
            let value = value.trim();
            label = value
                .split(':')
                .nth(1)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .or_else(|| Some(value.to_string()));
        }
    }
    flush(&mut bus, &mut connector, &mut label);

    let mut displays = Vec::with_capacity(found.len());
    for (bus, connector, label) in found {
        // A bus that will not answer VCP 0x10 has no brightness to offer.
        match ddc_get(bus).await {
            Ok((current, max)) => displays.push(Found {
                percent: to_percent(current, max),
                sink: Sink::Ddc { bus, max },
                label: label.unwrap_or_else(|| format!("i2c-{bus}")),
                connector,
            }),
            Err(error) => log::debug!("ddc bus {bus}: {error:#}"),
        }
    }
    Ok(displays)
}

/// `VCP 10 C <current> <max>`
async fn ddc_get(bus: u32) -> anyhow::Result<(u32, u32)> {
    let output = ddcutil(&["getvcp", "10", "--brief", "--bus", &bus.to_string()]).await?;
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("VCP 10"))
        .ok_or_else(|| anyhow::anyhow!("no VCP 10 in ddcutil output"))?;
    let mut fields = line.split_whitespace().skip(3);
    let current = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("unparsable ddcutil value: {line}"))?;
    let max = fields.next().and_then(|value| value.parse().ok()).unwrap_or(100);
    Ok((current, max.max(1)))
}

async fn ddc_set(bus: u32, value: u32) -> anyhow::Result<()> {
    ddcutil(&[
        "setvcp",
        "10",
        &value.to_string(),
        "--bus",
        &bus.to_string(),
    ])
    .await
    .map(|_| ())
}

/// One `ddcutil` run. `ddcutil` is a child process because it is the only
/// DDC/CI implementation available here; it is never run from a view, and a
/// hung i2c transaction is bounded by [`DDC_TIMEOUT`].
async fn ddcutil(args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        DDC_TIMEOUT,
        tokio::process::Command::new("ddcutil")
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ddcutil {} timed out", args.join(" ")))??;

    if !output.status.success() {
        anyhow::bail!(
            "ddcutil {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A wrapper that reports wheel notches and nothing else.
///
/// `mouse_area` cannot be used for this: it captures every left press and
/// release that lands on it (`iced/widget/src/mouse_area.rs`, unconditional
/// `shell.capture_event()`), so the bar's own cell button never sees the click
/// and the popup would never open. This forwards everything to the content and
/// only consumes `WheelScrolled` while the cursor is inside the cell.
struct Wheel<'a, Message> {
    content: Element<'a, Message>,
    on_scroll: Box<dyn Fn(mouse::ScrollDelta) -> Message + 'a>,
}

impl<'a, Message> Wheel<'a, Message> {
    fn new(
        content: impl Into<Element<'a, Message>>,
        on_scroll: impl Fn(mouse::ScrollDelta) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            on_scroll: Box::new(on_scroll),
        }
    }
}

impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for Wheel<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &cosmic::iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if shell.is_event_captured() || !cursor.is_over(layout.bounds()) {
            return;
        }
        if let cosmic::iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) = event {
            shell.publish((self.on_scroll)(*delta));
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<
        cosmic::iced::advanced::overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>,
    > {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<Wheel<'a, Message>> for Element<'a, Message> {
    fn from(wheel: Wheel<'a, Message>) -> Self {
        Element::new(wheel)
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait Login1Manager {
    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait Login1Session {
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}
