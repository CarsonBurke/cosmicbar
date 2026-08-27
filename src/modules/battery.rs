//! Battery module, fed by UPower over D-Bus.
//!
//! Waybar's `battery` module reads `/sys/class/power_supply` on an interval and
//! only ever knows about the laptop battery — on a desktop like this one it is
//! dead weight. This module asks UPower instead, so it also sees the batteries
//! UPower reports for wireless peripherals (mouse, keyboard, headset) and any
//! UPS, and it updates from `PropertiesChanged` signals rather than a timer.
//!
//! The bar cell mirrors `battery.jsonc` (`{icon} {capacity}%`, the same ten
//! charge glyphs, warning at 20% and critical at 10%) whenever a real system
//! battery exists. With no system battery it falls back to the peripheral that
//! needs charging soonest, and with no battery-like device at all the module
//! hides itself completely.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
use cosmic::iced::{Alignment, Subscription};
use cosmic::widget;
use zbus::zvariant::{OwnedValue, Value};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card};
use crate::theme::Island;

/// waybar: `@battery` → surface0.
pub const ISLAND: Island = Island::Start;

/// waybar `format-icons`: empty through full, indexed by tens of a percent.
const CHARGE_GLYPHS: [&str; 10] = [
    "\u{f008e}",
    "\u{f007b}",
    "\u{f007c}",
    "\u{f007d}",
    "\u{f007e}",
    "\u{f007f}",
    "\u{f0080}",
    "\u{f0081}",
    "\u{f0082}",
    "\u{f0079}",
];
/// waybar `format-charging`: nf-md-flash.
const CHARGING_GLYPH: &str = "\u{f0241}";
/// nf-md-battery-unknown, for a device whose kind has no glyph of its own.
const UNKNOWN_GLYPH: &str = "\u{f0091}";

/// waybar `states`.
const WARNING_PERCENT: f64 = 20.0;
const CRITICAL_PERCENT: f64 = 10.0;

/// UPower emits one `PropertiesChanged` per device per refresh; a short wait
/// turns a burst into a single rescan.
const COALESCE: Duration = Duration::from_millis(150);
/// Reconnect ladder for a UPower that is down or restarting.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A session this long was healthy: the next failure starts the ladder over.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// `org.freedesktop.UPower.Device.Type`.
mod kind {
    pub const UNKNOWN: u32 = 0;
    pub const LINE_POWER: u32 = 1;
    pub const BATTERY: u32 = 2;
    pub const UPS: u32 = 3;
}

/// `org.freedesktop.UPower.Device.State`.
mod state {
    pub const CHARGING: u32 = 1;
    pub const DISCHARGING: u32 = 2;
    pub const EMPTY: u32 = 3;
    pub const FULLY_CHARGED: u32 = 4;
    pub const PENDING_CHARGE: u32 = 5;
    pub const PENDING_DISCHARGE: u32 = 6;
}

/// `org.freedesktop.UPower.Device.BatteryLevel`. `UNKNOWN` and `NONE` both mean
/// "use the percentage"; `LOW` and above mean the device only reports a coarse
/// level and UPower's percentage is to be ignored (`upower -d` prints
/// "should be ignored" for exactly those).
mod level {
    // Values are `UpDeviceLevel` from libupower-glib, not a dense 0..n range
    // of the words `upower -d` prints - hence the explicit list.
    pub const UNKNOWN: u32 = 0;
    pub const NONE: u32 = 1;
    pub const DISCHARGING: u32 = 2;
    pub const LOW: u32 = 3;
    pub const CRITICAL: u32 = 4;
    pub const ACTION: u32 = 5;

    /// Does this device report a usable percentage?
    pub fn exact(level: u32) -> bool {
        matches!(level, UNKNOWN | NONE)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Device {
    kind: u32,
    state: u32,
    percentage: f64,
    battery_level: u32,
    /// Seconds, 0 when UPower cannot estimate.
    time_to_empty: i64,
    time_to_full: i64,
    /// Watts, signed by direction of flow.
    energy_rate: f64,
    /// True for something that powers the machine, false for a peripheral.
    power_supply: bool,
    present: bool,
    vendor: String,
    model: String,
    /// Unix seconds of UPower's last refresh of this device.
    updated: i64,
}

impl Device {
    /// A device the bar can report a charge for.
    fn charged(&self) -> bool {
        self.kind != kind::LINE_POWER
            && self.kind != kind::UNKNOWN
            && (self.percentage > 0.0 || !level::exact(self.battery_level))
    }

    /// Powers the machine, as opposed to a peripheral: UPower's `PowerSupply`
    /// is the discriminator, and a UPS counts.
    fn system(&self) -> bool {
        self.power_supply && matches!(self.kind, kind::BATTERY | kind::UPS) && self.present
    }

    fn charging(&self) -> bool {
        matches!(self.state, state::CHARGING | state::PENDING_CHARGE)
    }

    fn name(&self) -> String {
        match (self.vendor.trim(), self.model.trim()) {
            ("", "") => kind_name(self.kind).to_string(),
            ("", model) => model.to_string(),
            (vendor, "") => vendor.to_string(),
            (vendor, model) if model.starts_with(vendor) => model.to_string(),
            (vendor, model) => format!("{vendor} {model}"),
        }
    }

    /// `55%`, or the coarse word for a device that only reports a level.
    fn charge(&self) -> String {
        if level::exact(self.battery_level) {
            format!("{:.0}%", self.percentage)
        } else {
            level_name(self.battery_level).to_string()
        }
    }

    fn glyph(&self) -> &'static str {
        if self.system() {
            if self.charging() {
                return CHARGING_GLYPH;
            }
            let step = ((self.percentage / 10.0) as usize).min(CHARGE_GLYPHS.len() - 1);
            return CHARGE_GLYPHS[step];
        }
        kind_glyph(self.kind)
    }

    /// Percentage for threshold decisions; a coarse `low`/`critical` level maps
    /// onto the same thresholds so both kinds of device colour alike.
    fn severity(&self) -> f64 {
        match self.battery_level {
            level::LOW => WARNING_PERCENT,
            // `action` is UPower saying it is about to act on this battery.
            level::CRITICAL | level::ACTION => CRITICAL_PERCENT,
            // A UPS reporting only `discharging` says nothing about charge.
            level::DISCHARGING => 100.0,
            // A coarse `normal`/`high`/`full` is not a warning; anything else
            // means the percentage is the truth.
            other if !level::exact(other) => 100.0,
            _ => self.percentage,
        }
    }

    /// Which colour band this device is in, without a palette in hand: charging
    /// first, then the two warning thresholds, then whether it powers the
    /// machine at all. Split from `color` so a snapshot can be compared for
    /// "draws the same cell" against the same rule the cell is painted by.
    fn tier(&self) -> u8 {
        if self.charging() {
            // waybar `#battery.charging { color: @charging }`
            return 0;
        }
        let severity = self.severity();
        if severity <= CRITICAL_PERCENT {
            1
        } else if severity <= WARNING_PERCENT {
            2
        } else if self.system() {
            3
        } else {
            4
        }
    }

    fn color(&self, ctx: &Ctx) -> cosmic::iced::Color {
        match self.tier() {
            0 => ctx.palette.green,
            1 => ctx.palette.red,
            2 => ctx.palette.yellow,
            3 => ctx.palette.fg(),
            // A healthy peripheral is background information.
            _ => ctx.palette.muted(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    on_battery: bool,
    /// UPower's composite device, when it stands for a real battery.
    display: Option<Device>,
    /// Every other battery UPower knows about, worst charge first.
    devices: Vec<Device>,
}

impl Snapshot {
    /// The device the bar cell speaks for: a battery that powers the machine,
    /// never a peripheral. A desktop's only UPower "batteries" are its mouse
    /// and its headset, and those already have their own modules — reporting a
    /// headset's 10% as *the* battery reads as the machine dying.
    fn headline(&self) -> Option<&Device> {
        self.display
            .as_ref()
            .filter(|device| device.system())
            .or_else(|| self.devices.iter().find(|device| device.system()))
    }

    /// What the bar cell draws, and nothing else: the headline device's glyph,
    /// its charge as it is printed, and its colour band. Mirrors `view`. A
    /// peripheral's percentage moving is popup detail and must not repaint the
    /// bar, and on a desktop with no system battery there is no cell at all.
    fn bar_key(&self) -> Option<(&'static str, String, u8)> {
        let device = self.headline()?;
        Some((device.glyph(), device.charge(), device.tier()))
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Snapshot(Arc<Snapshot>),
    /// UPower went away; the module hides until it comes back.
    Unavailable,
}

#[derive(Debug, Default)]
pub struct State {
    snapshot: Option<Arc<Snapshot>>,
}

impl State {
    /// Always subscribed: a battery warning must arrive whether or not anyone
    /// has the popup open, and UPower pushes, so it costs nothing. `open` is
    /// part of the identity so the stream knows whether the popup's device list
    /// is on screen; while it is not, a snapshot that draws the same cell is
    /// dropped instead of repainting the bar for a mouse's percentage.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        Subscription::run_with(open, stream)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Snapshot(snapshot) => self.snapshot = Some(snapshot),
            Event::Unavailable => self.snapshot = None,
        }
        Task::none()
    }

    /// `None` hides the module: this machine has no battery at all, and an
    /// empty battery readout would be a lie.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let device = self.snapshot.as_ref()?.headline()?;
        Some(
            crate::theme::label(
                device.glyph(),
                device.charge(),
                ctx.font_size,
                cosmic::theme::Text::Color(device.color(ctx)),
            ),
        )
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let palette = ctx.palette;
        let snapshot = self.snapshot.as_ref()?;
        // Which side of the wall socket the machine is on is the one thing no
        // bar cell can say, so it is the card's title, and the battery the cell
        // does speak for supplies the value beside it.
        let headline: Option<Element<'_, Message>> = snapshot.headline().map(|device| {
            popup::item(device.charge(), ctx)
                .class(cosmic::theme::Text::Color(device.color(ctx)))
                .into()
        });
        let mut card = Card::new().block(popup::split(
            popup::title(
                if snapshot.on_battery {
                    "on battery"
                } else {
                    "on AC power"
                },
                ctx,
            )
            // Running the machine down is worth flagging; mains is the state
            // every other reading in the card is written for.
            .class(cosmic::theme::Text::Color(if snapshot.on_battery {
                palette.peach
            } else {
                palette.fg()
            })),
            headline,
        ));

        for device in snapshot
            .display
            .iter()
            .filter(|device| device.system())
            .chain(snapshot.devices.iter())
        {
            let mut detail = vec![state_name(device.state).to_string()];
            detail.push(kind_name(device.kind).to_string());
            if device.time_to_empty > 0 {
                detail.push(format!("{} to empty", duration(device.time_to_empty)));
            }
            if device.time_to_full > 0 {
                detail.push(format!("{} to full", duration(device.time_to_full)));
            }
            if device.energy_rate.abs() >= 0.01 {
                detail.push(format!("{:.1} W", device.energy_rate.abs()));
            }
            if let Some(ago) = stale(device.updated, ctx.now_ms) {
                detail.push(format!("updated {ago} ago"));
            }

            let charge: Element<'_, Message> = popup::item(device.charge(), ctx)
                .class(cosmic::theme::Text::Color(device.color(ctx)))
                .into();
            card = card.block(popup::split(
                popup::lines()
                    .push(
                        // The colour band belongs to the glyph and the charge:
                        // five device names in five colours down one card is no
                        // longer a column of names.
                        widget::Row::new()
                            .push(
                                crate::theme::glyph_text(device.glyph(), ctx.body())
                                    .class(cosmic::theme::Text::Color(device.color(ctx))),
                            )
                            .push(popup::item(device.name(), ctx))
                            .spacing(crate::theme::GLYPH_GAP)
                            .align_y(Alignment::Center),
                    )
                    .push(popup::detail(detail.join(" · "), ctx)),
                [charge],
            ));
        }

        Some(
            card.maybe(
                (snapshot.display.is_none() && snapshot.devices.is_empty())
                    .then(|| popup::detail("UPower reports no batteries", ctx)),
            )
            .build(),
        )
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Battery(event))
}

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower",
    gen_blocking = false
)]
trait UPower {
    fn enumerate_devices(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;
    fn get_display_device(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    #[zbus(signal)]
    fn device_added(&self, device: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn device_removed(&self, device: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

/// A reconnecting subscription, like `mlqd`'s: UPower restarting must not leave
/// the bar showing a frozen charge.
fn stream(open: &bool) -> impl Stream<Item = Message> + use<> {
    let detailed = *open;
    cosmic::iced::stream::channel(4, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            if let Err(error) = session(&mut sender, detailed).await {
                log::debug!("upower subscription ended: {error:#}");
            }
            let _ = sender.send(event_message(Event::Unavailable)).await;
            if started.elapsed() >= STABLE_SESSION {
                attempt = 0;
            }
            let delay = RECONNECT_BACKOFF_SECS[attempt.min(RECONNECT_BACKOFF_SECS.len() - 1)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

/// One UPower connection's worth of snapshots. `detailed` means the popup, which
/// lists every device UPower knows about, is on screen; with it shut only the
/// machine's own battery is visible and the rest is noise.
async fn session(
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Message>,
    detailed: bool,
) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;
    let upower = UPowerProxy::new(&connection).await?;

    // One rule for every device's properties: UPower owns the whole tree, so
    // there is no need for a proxy (and a signal match) per device.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.UPower")?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .build();
    let changes = zbus::MessageStream::for_match_rule(rule, &connection, Some(16)).await?;
    let added = upower.receive_device_added().await?;
    let removed = upower.receive_device_removed().await?;
    let mut changed = cosmic::iced::futures::stream::select_all([
        changes.map(|_| ()).boxed(),
        added.map(|_| ()).boxed(),
        removed.map(|_| ()).boxed(),
    ]);

    // What the last snapshot sent to the bar drew, while the popup is shut.
    let mut drawn = None;
    loop {
        let snapshot = scan(&connection, &upower).await?;
        // A peripheral's percentage ticking down is a row in the popup and
        // nothing on the bar: with the popup shut, that is not a frame.
        let key = snapshot.bar_key();
        if detailed || drawn.as_ref() != Some(&key) {
            drawn = Some(key);
            if sender
                .send(event_message(Event::Snapshot(Arc::new(snapshot))))
                .await
                .is_err()
            {
                // The bar dropped the subscription.
                return Ok(());
            }
        }
        if changed.next().await.is_none() {
            return Ok(());
        }
        tokio::time::sleep(COALESCE).await;
    }
}

async fn scan(connection: &zbus::Connection, upower: &UPowerProxy<'_>) -> anyhow::Result<Snapshot> {
    let mut devices = Vec::new();
    for path in upower.enumerate_devices().await? {
        match read_device(connection, &path).await {
            Ok(device) if device.charged() => devices.push(device),
            Ok(_) => {}
            // A device can vanish between enumeration and the property read.
            Err(error) => log::debug!("upower device {path}: {error:#}"),
        }
    }
    // Worst first: the popup and the bar fallback both want the urgent one.
    devices.sort_by(|left, right| {
        left.severity()
            .partial_cmp(&right.severity())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let display = match upower.get_display_device().await {
        Ok(path) => read_device(connection, &path).await.ok(),
        Err(error) => {
            log::debug!("upower display device: {error:#}");
            None
        }
    };

    Ok(Snapshot {
        on_battery: upower.on_battery().await.unwrap_or(false),
        display,
        devices,
    })
}

/// All of a device's properties in one round trip.
async fn read_device(
    connection: &zbus::Connection,
    path: &zbus::zvariant::OwnedObjectPath,
) -> anyhow::Result<Device> {
    let properties = zbus::fdo::PropertiesProxy::builder(connection)
        .destination("org.freedesktop.UPower")?
        .path(path.as_ref())?
        .build()
        .await?
        .get_all("org.freedesktop.UPower.Device".try_into()?)
        .await?;

    Ok(Device {
        kind: number(&properties, "Type") as u32,
        state: number(&properties, "State") as u32,
        percentage: number(&properties, "Percentage"),
        battery_level: number(&properties, "BatteryLevel") as u32,
        time_to_empty: number(&properties, "TimeToEmpty") as i64,
        time_to_full: number(&properties, "TimeToFull") as i64,
        energy_rate: number(&properties, "EnergyRate"),
        power_supply: flag(&properties, "PowerSupply"),
        present: flag(&properties, "IsPresent"),
        vendor: text(&properties, "Vendor"),
        model: text(&properties, "Model"),
        updated: number(&properties, "UpdateTime") as i64,
    })
}

/// UPower mixes `d`, `u`, `x` and `t` across these properties; one numeric
/// accessor keeps the device reader readable.
fn number(properties: &HashMap<String, OwnedValue>, key: &str) -> f64 {
    match properties.get(key).map(|value| &**value) {
        Some(Value::F64(value)) => *value,
        Some(Value::U32(value)) => f64::from(*value),
        Some(Value::I32(value)) => f64::from(*value),
        Some(Value::U64(value)) => *value as f64,
        Some(Value::I64(value)) => *value as f64,
        Some(Value::U16(value)) => f64::from(*value),
        Some(Value::I16(value)) => f64::from(*value),
        _ => 0.0,
    }
}

fn text(properties: &HashMap<String, OwnedValue>, key: &str) -> String {
    match properties.get(key).map(|value| &**value) {
        Some(Value::Str(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn flag(properties: &HashMap<String, OwnedValue>, key: &str) -> bool {
    matches!(
        properties.get(key).map(|value| &**value),
        Some(Value::Bool(true))
    )
}

fn kind_name(kind: u32) -> &'static str {
    match kind {
        kind::LINE_POWER => "AC",
        kind::BATTERY => "battery",
        kind::UPS => "UPS",
        4 => "monitor",
        5 => "mouse",
        6 => "keyboard",
        7 => "PDA",
        8 => "phone",
        9 => "media player",
        10 => "tablet",
        11 => "computer",
        12 => "gaming input",
        13 => "pen",
        14 => "touchpad",
        15 => "modem",
        16 => "network device",
        17 => "headset",
        18 => "speakers",
        19 => "headphones",
        20 => "video",
        21 => "audio device",
        22 => "remote control",
        23 => "printer",
        24 => "scanner",
        25 => "camera",
        26 => "wearable",
        27 => "toy",
        28 => "bluetooth device",
        _ => "device",
    }
}

/// Peripheral glyphs; every codepoint checked against the bar's nerd font.
fn kind_glyph(kind: u32) -> &'static str {
    match kind {
        // nf-md-mouse, nf-md-keyboard, nf-md-headset, nf-md-headphones,
        // nf-md-cellphone, nf-md-tablet, nf-md-gamepad-variant
        5 => "\u{f037d}",
        6 => "\u{f030c}",
        17 => "\u{f02ce}",
        18 | 19 | 21 => "\u{f02cb}",
        8 | 9 => "\u{f011c}",
        10 => "\u{f04f6}",
        12 => "\u{f0297}",
        _ => UNKNOWN_GLYPH,
    }
}

fn state_name(value: u32) -> &'static str {
    match value {
        state::CHARGING => "charging",
        state::DISCHARGING => "discharging",
        state::EMPTY => "empty",
        state::FULLY_CHARGED => "fully charged",
        state::PENDING_CHARGE => "pending charge",
        state::PENDING_DISCHARGE => "pending discharge",
        _ => "state unknown",
    }
}

fn level_name(value: u32) -> &'static str {
    match value {
        2 => "low",
        3 => "critical",
        4 => "normal",
        5 => "high",
        6 => "full",
        _ => "unknown",
    }
}

/// waybar `format-time`: `{H} hr {M} min`.
fn duration(seconds: i64) -> String {
    let minutes = seconds / 60;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, minutes) => format!("{hours} hr {minutes} min"),
    }
}

/// How long ago UPower last heard from a device, once that is long enough to
/// matter — a bluetooth headset that is off keeps its last known charge.
fn stale(updated: i64, now_ms: i64) -> Option<String> {
    const STALE_AFTER: i64 = 600;
    let age = now_ms / 1000 - updated;
    if updated <= 0 || age < STALE_AFTER {
        return None;
    }
    Some(match age {
        STALE_AFTER..3600 => format!("{} min", age / 60),
        3600..86400 => format!("{} hr", age / 3600),
        _ => format!("{} d", age / 86400),
    })
}
