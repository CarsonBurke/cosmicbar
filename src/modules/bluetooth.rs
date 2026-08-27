//! BlueZ module: adapter state, connected devices and their batteries.
//!
//! Push driven. One system-bus connection reads the whole BlueZ object tree
//! once through `org.freedesktop.DBus.ObjectManager`, then keeps it current
//! from signals: `InterfacesAdded`/`InterfacesRemoved` as devices appear and
//! vanish, and `PropertiesChanged` on `org.bluez.Adapter1` (`Powered`,
//! `Discovering`), `org.bluez.Device1` (`Connected`, `Name`, `RSSI`, …) and
//! `org.bluez.Battery1` (`Percentage`). Nothing polls, nothing shells out to
//! `bluetoothctl`, and no property is read twice: every update is applied to
//! the tree in memory.
//!
//! Mutations (`Connect`, `Disconnect`, `Pair`, `Powered`, discovery) go out on
//! a cached connection that lives as long as the bar, because BlueZ stops a
//! discovery session as soon as the client that started it disconnects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream, StreamExt, channel::mpsc::Sender};
use cosmic::iced::{Alignment, Subscription};
use cosmic::widget;
use zbus::message::{Message as BusMessage, Type as MessageType};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::{Island, Palette};

/// waybar drew `#bluetooth` on the `@tray` (mantle) background.
pub const ISLAND: Island = Island::Join;

const BLUEZ: &str = "org.bluez";
const IF_ADAPTER: &str = "org.bluez.Adapter1";
const IF_DEVICE: &str = "org.bluez.Device1";
const IF_BATTERY: &str = "org.bluez.Battery1";
const IF_OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const IF_PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const DBUS: &str = "org.freedesktop.DBus";

/// Glyphs. The four state icons are exactly the ones
/// `~/.config/waybar/modules/bluetooth.jsonc` used; the rest are verified
/// against the CommitMono Nerd Font cmap.
const ICON_BLUETOOTH: &str = "\u{f00af}";
const ICON_ON: &str = "\u{f00b0}";
const ICON_CONNECTED: &str = "\u{f00b1}";
const ICON_OFF: &str = "\u{f00b2}";
const ICON_HEADSET: &str = "\u{f02cb}";
const ICON_SPEAKER: &str = "\u{f04c3}";
const ICON_KEYBOARD: &str = "\u{f030c}";
const ICON_MOUSE: &str = "\u{f037d}";
const ICON_GAMEPAD: &str = "\u{f0296}";
const ICON_PHONE: &str = "\u{f011c}";
const ICON_COMPUTER: &str = "\u{f0322}";
const ICON_DISPLAY: &str = "\u{f0379}";
const ICON_PRINTER: &str = "\u{f042a}";
const ICON_WATCH: &str = "\u{f0589}";

/// Longest device name rendered in a popup row: a name that wraps costs a
/// whole extra line in a popup that has to fit the screen.
const ROW_LIMIT: usize = 18;
/// Unpaired devices listed while scanning.
const NEARBY_LIMIT: usize = 6;
/// A discovery session emits a signal per advertisement; coalesce the burst.
const COALESCE: Duration = Duration::from_millis(180);
/// Enough room for a discovery burst while the tree is being read.
const SIGNAL_QUEUE: usize = 256;
/// Reconnect ladder for a bluetoothd that is down or restarting.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A session that lasted this long was healthy: the next failure starts the
/// ladder over instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// Battery levels that mirror the waybar CSS meanings (warning, critical).
const BATTERY_CRITICAL: u8 = 20;
const BATTERY_WARNING: u8 = 35;

#[derive(Debug, Clone)]
pub enum Event {
    Snapshot(Arc<Snapshot>),
    /// BlueZ is gone or unreachable; the subscriber is retrying.
    Unavailable,
    SetPowered(bool),
    /// Flip the adapter's power, whichever way it is set right now.
    TogglePowered,
    SetDiscovering(bool),
    Connect(String),
    Disconnect(String),
    Pair(String),
    /// Pairing that needs a PIN prompt belongs in a TUI.
    Terminal(String),
    Done {
        /// Object path the call was made on, so its row stops being busy.
        subject: Option<String>,
        result: Result<(), String>,
    },
}

#[derive(Debug, Default)]
pub struct State {
    snapshot: Option<Arc<Snapshot>>,
    available: bool,
    /// Object path with a call in flight: its row shows `…` instead of a button.
    busy: Option<String>,
    error: Option<String>,
}

/// The BlueZ object tree, reduced to what the module renders.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    adapter: Option<Adapter>,
    devices: Vec<Device>,
}

#[derive(Debug, Default, Clone)]
struct Adapter {
    path: String,
    alias: String,
    address: String,
    powered: bool,
    discovering: bool,
}

#[derive(Debug, Default, Clone)]
struct Device {
    path: String,
    address: String,
    name: String,
    icon: Option<String>,
    connected: bool,
    paired: bool,
    battery: Option<u8>,
    rssi: Option<i16>,
}

impl State {
    /// BlueZ pushes everything this module needs, but the popup reads far more
    /// of the tree than the cell does: per-device RSSI above all, which every
    /// connected device readvertises a few times a minute. `open` is part of
    /// the subscription's identity, so opening the popup restarts the session
    /// with those changes emitted again and closing it drops them.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        Subscription::run_with(open, events)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Snapshot(snapshot) => {
                self.snapshot = Some(snapshot);
                self.available = true;
                Task::none()
            }
            Event::Unavailable => {
                self.available = false;
                self.busy = None;
                Task::none()
            }
            Event::SetPowered(powered) => match self.adapter_path() {
                Some(adapter) => {
                    self.busy = Some(adapter.clone());
                    self.error = None;
                    mutate(adapter.clone(), async move {
                        set_property(&adapter, IF_ADAPTER, "Powered", Value::Bool(powered)).await
                    })
                }
                None => Task::none(),
            },
            Event::TogglePowered => {
                let powered = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.adapter.as_ref())
                    .map(|adapter| adapter.powered);
                match powered {
                    Some(powered) => self.update(Event::SetPowered(!powered)),
                    None => Task::none(),
                }
            }
            Event::SetDiscovering(discovering) => match self.adapter_path() {
                Some(adapter) => {
                    self.busy = Some(adapter.clone());
                    self.error = None;
                    mutate(adapter.clone(), async move {
                        call(
                            &adapter,
                            IF_ADAPTER,
                            if discovering {
                                "StartDiscovery"
                            } else {
                                "StopDiscovery"
                            },
                        )
                        .await
                    })
                }
                None => Task::none(),
            },
            Event::Connect(device) => {
                self.busy = Some(device.clone());
                self.error = None;
                mutate(device.clone(), async move {
                    call(&device, IF_DEVICE, "Connect").await
                })
            }
            Event::Disconnect(device) => {
                self.busy = Some(device.clone());
                self.error = None;
                mutate(device.clone(), async move {
                    call(&device, IF_DEVICE, "Disconnect").await
                })
            }
            Event::Pair(device) => {
                self.busy = Some(device.clone());
                self.error = None;
                mutate(device.clone(), async move {
                    // Pairing first, then the connection it was wanted for.
                    call(&device, IF_DEVICE, "Pair").await?;
                    call(&device, IF_DEVICE, "Connect").await
                })
            }
            Event::Terminal(terminal) => Task::batch([
                mutate("".into(), async move { spawn(&terminal, "bluetoothctl").await }),
                Task::done(cosmic::Action::App(Message::ClosePopup)),
            ]),
            Event::Done { subject, result } => {
                if subject.is_none() || subject.as_deref() == self.busy.as_deref() {
                    self.busy = None;
                }
                self.error = result.err();
                Task::none()
            }
        }
    }

    /// `None` hides the module: no BlueZ, or no adapter to talk about.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let snapshot = self.snapshot.as_ref().filter(|_| self.available)?;
        let adapter = snapshot.adapter.as_ref()?;
        let palette = ctx.palette;

        if !adapter.powered {
            return Some(
                crate::theme::glyph_only(ICON_OFF, ctx.font_size)
                    .class(cosmic::theme::Text::Color(palette.muted()))
                    .align_y(Alignment::Center)
                    .into(),
            );
        }

        let connected = snapshot.connected().count();
        // Which device it is belongs to the popup: a headset's name is longer
        // than everything else on the bar put together, and it moves the clock
        // every time it connects. The glyph already says something is connected,
        // so a count is only worth its space past the first one. Scanning is the
        // popup's business too - the cell keeps saying what is connected while a
        // scan runs behind it.
        let (glyph, rest) = match connected {
            0 => (ICON_ON, String::new()),
            1 => (ICON_CONNECTED, String::new()),
            count => (ICON_CONNECTED, count.to_string()),
        };

        let class = cosmic::theme::Text::Color(if connected == 0 {
            palette.muted()
        } else {
            palette.fg()
        });
        let mut row = widget::Row::new()
            .align_y(Alignment::Center)
            .spacing(crate::theme::GLYPH_GAP)
            .push(crate::theme::label(glyph, rest, ctx.font_size, class));
        // The battery a connected device advertises, worst one first.
        if let Some(battery) = snapshot.worst_battery() {
            row = row.push(
                crate::theme::text(format!("{battery}%"))
                    .class(cosmic::theme::Text::Color(battery_color(battery, &palette)))
                    .align_y(Alignment::Center),
            );
        }
        Some(row.into())
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.available && self.snapshot.as_ref().is_some_and(|snapshot| snapshot.adapter.is_some())
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let snapshot = self.snapshot.as_ref().filter(|_| self.available)?;
        let adapter = snapshot.adapter.as_ref()?;
        let palette = ctx.palette;

        // The popup hangs under the bluetooth glyph, so the header owes only the
        // adapter's name and the address that tells two of them apart.
        let mut identity = popup::lines();
        if adapter.alias.is_empty() {
            // With no alias the address is the only name the adapter has, so it
            // is the title rather than a second line repeating it.
            identity = identity.push(popup::title(adapter.address.as_str(), ctx));
        } else {
            identity = identity
                .push(popup::title(adapter.alias.as_str(), ctx))
                .push(popup::detail(adapter.address.as_str(), ctx));
        }

        let mut controls = vec![self.action(
            ctx,
            if adapter.powered { "off" } else { "on" },
            // An unpowered adapter has one thing left to offer, and the rest of
            // this card is a single line saying so.
            if adapter.powered {
                Chip::Plain
            } else {
                Chip::Accent
            },
            &adapter.path,
            Event::SetPowered(!adapter.powered),
        )];
        if adapter.powered {
            controls.push(self.action(
                ctx,
                if adapter.discovering { "stop" } else { "scan" },
                Chip::Plain,
                &adapter.path,
                Event::SetDiscovering(!adapter.discovering),
            ));
        }
        let mut card = Card::new().block(popup::split(identity, controls));

        if !adapter.powered {
            return Some(
                self.footer(card.block(popup::detail("powered off", ctx)), ctx)
                    .build(),
            );
        }

        let connected: Vec<&Device> = snapshot.connected().collect();
        if !connected.is_empty() {
            let mut block = popup::column().push(popup::section("connected", ctx));
            for device in connected {
                block = block.push(self.row(
                    ctx,
                    device,
                    palette.green,
                    "disconnect",
                    Chip::Danger,
                    Event::Disconnect(device.path.clone()),
                ));
            }
            card = card.block(block);
        }

        let paired: Vec<&Device> = snapshot
            .devices
            .iter()
            .filter(|device| device.paired && !device.connected)
            .collect();
        if !paired.is_empty() {
            let mut block = popup::column().push(popup::section("paired", ctx));
            for device in paired {
                block = block.push(self.row(
                    ctx,
                    device,
                    palette.fg(),
                    "connect",
                    Chip::Plain,
                    Event::Connect(device.path.clone()),
                ));
            }
            card = card.block(block);
        }

        let nearby: Vec<&Device> = snapshot
            .devices
            .iter()
            .filter(|device| !device.paired && !device.connected)
            .collect();
        if !nearby.is_empty() {
            let mut block = popup::column().push(popup::section("nearby", ctx));
            for device in nearby.iter().take(NEARBY_LIMIT) {
                block = block.push(self.row(
                    ctx,
                    device,
                    palette.muted(),
                    "pair",
                    Chip::Plain,
                    Event::Pair(device.path.clone()),
                ));
            }
            if nearby.len() > NEARBY_LIMIT {
                block = block.push(popup::detail(
                    format!("+{} more", nearby.len() - NEARBY_LIMIT),
                    ctx,
                ));
            }
            card = card.block(block);
        } else if adapter.discovering {
            // Having found nothing yet is still what the section has to say.
            card = card.block(
                popup::column()
                    .push(popup::section("nearby", ctx))
                    .push(popup::detail("scanning…", ctx)),
            );
        }

        Some(self.footer(card, ctx).build())
    }

    /// One device row: glyph and name, the readings that belong to them, and
    /// the row's one action. The row itself is not clickable, because every
    /// verb a device has is already the chip on its right.
    fn row<'a>(
        &self,
        ctx: &Ctx,
        device: &Device,
        color: cosmic::iced::Color,
        action: &'a str,
        style: Chip,
        event: Event,
    ) -> Element<'a, Message> {
        // The card is narrow: a battery reading and an RSSI are worth a line of
        // it, a MAC address the user cannot act on is not.
        let mut readings = Vec::new();
        if let Some(battery) = device.battery {
            readings.push(format!("{battery}%"));
        }
        if let Some(rssi) = device.rssi {
            readings.push(format!("{rssi}dBm"));
        }
        let name = if device.name.is_empty() {
            &device.address
        } else {
            &device.name
        };
        let mut lines = popup::lines().push(crate::theme::label(
            device_icon(device),
            elide(name, ROW_LIMIT),
            ctx.body(),
            cosmic::theme::Text::Color(color),
        ));
        if !readings.is_empty() {
            let readings = popup::detail(readings.join(" · "), ctx);
            lines = lines.push(match device.battery {
                // A battery low enough to matter is the one reading on this
                // line worth a colour of its own.
                Some(battery) if battery <= BATTERY_WARNING => readings.class(
                    cosmic::theme::Text::Color(battery_color(battery, &ctx.palette)),
                ),
                _ => readings,
            });
        }
        popup::split(
            lines,
            [self.action(ctx, action, style, &device.path, event)],
        )
        .into()
    }

    /// A chip that goes inert while its own call is in flight: the affordance
    /// stays where it was instead of the row reflowing around a button that
    /// disappeared for a moment.
    fn action<'a>(
        &self,
        ctx: &Ctx,
        label: &'a str,
        style: Chip,
        key: &str,
        event: Event,
    ) -> Element<'a, Message> {
        let idle = self.busy.as_deref() != Some(key);
        popup::chip(label, style, ctx, idle.then(|| event_message(event)))
    }

    /// The two blocks every version of this card ends with: the escape hatch
    /// for a pairing the bar cannot drive, and whatever the last call failed
    /// with. The powered-off card is this card with less between them.
    fn footer<'a>(&'a self, card: Card<'a>, ctx: &Ctx) -> Card<'a> {
        card.block(popup::split(
            popup::detail("pairing that needs a PIN", ctx),
            [popup::chip(
                "bluetoothctl",
                Chip::Plain,
                ctx,
                Some(event_message(Event::Terminal(ctx.terminal.clone()))),
            )],
        ))
        .maybe(self.error.as_ref().map(|error| {
            popup::detail(error.as_str(), ctx).class(cosmic::theme::Text::Color(ctx.palette.red))
        }))
    }

    fn adapter_path(&self) -> Option<String> {
        self.snapshot
            .as_ref()?
            .adapter
            .as_ref()
            .map(|adapter| adapter.path.clone())
    }
}

/// What the bar cell draws, and nothing else. Must mirror `view`: an absent
/// adapter hides the module, an unpowered one draws the off glyph alone, and a
/// powered one draws the connected count - which picks the glyph, its colour,
/// and whether a number is printed at all - beside the worst connected battery
/// with the tier that colours it. Neither the connected device's name nor the
/// scan belongs in here: both are the popup's. Two snapshots with the same key
/// paint the same pixels, so the second one is not worth a relayout.
#[derive(Debug, PartialEq, Eq)]
enum BarKey {
    /// `view` returned `None`: no adapter, so no cell at all.
    Hidden,
    Off,
    On {
        connected: usize,
        /// Percentage of the worst connected battery, and its colour tier.
        battery: Option<(u8, u8)>,
    },
}

impl Snapshot {
    fn connected(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|device| device.connected)
    }

    /// Lowest battery among connected devices: the one worth knowing about.
    fn worst_battery(&self) -> Option<u8> {
        self.connected().filter_map(|device| device.battery).min()
    }

    fn bar_key(&self) -> BarKey {
        let Some(adapter) = self.adapter.as_ref() else {
            return BarKey::Hidden;
        };
        if !adapter.powered {
            return BarKey::Off;
        }
        BarKey::On {
            connected: self.connected().count(),
            battery: self
                .worst_battery()
                .map(|battery| (battery, battery_tier(battery))),
        }
    }

    fn device_mut(&mut self, path: &str) -> Option<&mut Device> {
        self.devices.iter_mut().find(|device| device.path == path)
    }

    /// Connected first, then paired, then the strongest advertisements.
    fn sort(&mut self) {
        self.devices.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(b.rssi.cmp(&a.rssi))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Bluetooth(event))
}

/// Run a mutation and report its outcome, so a refused call is visible and the
/// row it belongs to stops being busy.
fn mutate(
    subject: String,
    call: impl Future<Output = anyhow::Result<()>> + Send + 'static,
) -> Task<Message> {
    Task::future(async move {
        cosmic::Action::App(event_message(Event::Done {
            subject: Some(subject),
            result: call.await.map_err(|error| format!("{error:#}")),
        }))
    })
}

/// Which of the waybar CSS battery states a percentage is in: 0 below the
/// warning level, 1 from it, 2 from critical. Split out from the colour so two
/// snapshots can be compared for "draws the same" without a palette in hand.
fn battery_tier(battery: u8) -> u8 {
    (battery <= BATTERY_WARNING) as u8 + (battery <= BATTERY_CRITICAL) as u8
}

fn battery_color(battery: u8, palette: &Palette) -> cosmic::iced::Color {
    match battery_tier(battery) {
        2 => palette.red,
        1 => palette.yellow,
        _ => palette.muted(),
    }
}

/// BlueZ's freedesktop icon name, mapped to a glyph.
fn device_icon(device: &Device) -> &'static str {
    match device.icon.as_deref().unwrap_or_default() {
        "audio-headset" | "audio-headphones" => ICON_HEADSET,
        "audio-card" | "audio-speakers" => ICON_SPEAKER,
        "input-keyboard" => ICON_KEYBOARD,
        "input-mouse" | "input-tablet" => ICON_MOUSE,
        "input-gaming" => ICON_GAMEPAD,
        "phone" => ICON_PHONE,
        "computer" => ICON_COMPUTER,
        "video-display" | "camera-video" => ICON_DISPLAY,
        "printer" => ICON_PRINTER,
        "watch" | "wearable" => ICON_WATCH,
        _ => ICON_BLUETOOTH,
    }
}

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

// ---------------------------------------------------------------- subscription

/// One connection either way; `open` is part of the subscription's identity, so
/// opening the popup restarts the session with popup-only property changes
/// emitted again, and closing it stops them at the bus.
fn events(open: &bool) -> impl Stream<Item = Message> + use<> {
    let detailed = *open;
    cosmic::iced::stream::channel(8, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            if let Err(error) = session(&mut sender, detailed).await {
                log::debug!("bluetooth: session ended: {error:#}");
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

/// One connection's worth of snapshots. Returns when BlueZ's bus name changes
/// owner, so the tree is read again from scratch. `detailed` means the popup is
/// on screen; with it shut, only changes the cell can draw are emitted.
async fn session(sender: &mut Sender<Message>, detailed: bool) -> anyhow::Result<()> {
    let conn = connection().await?;

    // Subscribed before the ownership check, so a bluetoothd that starts
    // during the check is not missed.
    let mut owner = signals(&conn, DBUS, Some(DBUS), Some("NameOwnerChanged"), Some(BLUEZ)).await?;
    if !has_owner(&conn, BLUEZ).await? {
        let _ = sender.send(event_message(Event::Unavailable)).await;
        log::debug!("bluetooth: waiting for {BLUEZ} on the bus");
        loop {
            let message = owner.next().await.context("bus stream ended")??;
            if new_owner(&message).is_some_and(|owner| !owner.is_empty()) {
                break;
            }
        }
    }

    let interfaces = signals(&conn, BLUEZ, Some(IF_OBJECT_MANAGER), None, None).await?;
    let changed = signals(
        &conn,
        BLUEZ,
        Some(IF_PROPERTIES),
        Some("PropertiesChanged"),
        None,
    )
    .await?;
    let mut signals = futures::stream::select_all(vec![interfaces, changed, owner]);

    let mut snapshot = read_tree(&conn).await?;
    snapshot.sort();
    // What the cell drew last. The first snapshot always goes out: it is what
    // populates the module.
    let mut drawn = snapshot.bar_key();
    if sender
        .send(event_message(Event::Snapshot(Arc::new(snapshot.clone()))))
        .await
        .is_err()
    {
        return Ok(());
    }

    let mut dirty = false;
    loop {
        // With work pending the loop waits only for the coalescing window, so
        // a discovery burst becomes one snapshot rather than dozens.
        let message = if dirty {
            match tokio::time::timeout(COALESCE, signals.next()).await {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(()),
                Err(_) => {
                    dirty = false;
                    snapshot.sort();
                    // With the popup shut the cell is the whole module, and at
                    // rest most patches land on the same glyph, name and
                    // percentage: sending one costs a relayout and a repaint
                    // for identical pixels.
                    let key = snapshot.bar_key();
                    if !detailed && key == drawn {
                        continue;
                    }
                    drawn = key;
                    if sender
                        .send(event_message(Event::Snapshot(Arc::new(snapshot.clone()))))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
            }
        } else {
            match signals.next().await {
                Some(message) => message,
                None => return Ok(()),
            }
        };

        match apply(&message?, &mut snapshot) {
            Change::Ignore => {}
            // Patched in memory, but not worth waking the bar for: the cell
            // cannot draw the difference, and the popup that can is not on
            // screen. Opening the popup restarts this subscription and the new
            // session reads the whole tree again, so what the popup shows is at
            // worst one round trip behind and never diverges.
            Change::Detail => dirty |= detailed,
            Change::Patch => dirty = true,
            // BlueZ restarted: every object path we hold is stale.
            Change::Restart => return Ok(()),
        }
    }
}

/// How far one signal reached, in ascending order of what it costs the bar, so
/// folding several interfaces of one object keeps the loudest thing that
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Change {
    Ignore,
    /// The tree was updated in place, but only in fields the popup renders: a
    /// device's RSSI, icon or pairing state, the adapter's alias. RSSI is the
    /// loud one — a connected device readvertises every few seconds — and none
    /// of it fits in the cell.
    Detail,
    /// The tree was updated in place and the cell can draw the difference;
    /// emit it.
    Patch,
    Restart,
}

/// A device's own state reaches the cell only while it is connected: the cell
/// counts connected devices, names the single one and prints the worst battery
/// among them. Everything about an idle device is a popup row.
fn device_change(connected: bool) -> Change {
    if connected { Change::Patch } else { Change::Detail }
}

/// Apply one signal to the tree, and report how far the change reached.
fn apply(message: &BusMessage, snapshot: &mut Snapshot) -> Change {
    let header = message.header();
    let member = header.member().map(|member| member.as_str()).unwrap_or("");
    match member {
        "NameOwnerChanged" => Change::Restart,
        "InterfacesAdded" => {
            let Ok((path, interfaces)) = message
                .body()
                .deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>)>()
            else {
                return Change::Ignore;
            };
            let path = path.as_str().to_owned();
            let mut interfaces: Vec<_> = interfaces.iter().collect();
            interfaces.sort_by_key(|(interface, _)| merge_order(interface));
            let mut change = Change::Ignore;
            for (interface, props) in interfaces {
                change = change.max(merge(snapshot, &path, interface, props));
            }
            change
        }
        "InterfacesRemoved" => {
            let Ok((path, interfaces)) = message
                .body()
                .deserialize::<(OwnedObjectPath, Vec<String>)>()
            else {
                return Change::Ignore;
            };
            let path = path.as_str();
            let mut change = Change::Ignore;
            for interface in interfaces {
                match interface.as_str() {
                    IF_DEVICE => {
                        snapshot.devices.retain(|device| {
                            if device.path != path {
                                return true;
                            }
                            change = change.max(device_change(device.connected));
                            false
                        });
                    }
                    IF_BATTERY => {
                        if let Some(device) = snapshot.device_mut(path)
                            && device.battery.take().is_some() {
                                change = change.max(device_change(device.connected));
                            }
                    }
                    IF_ADAPTER => {
                        if snapshot
                            .adapter
                            .as_ref()
                            .is_some_and(|adapter| adapter.path == path)
                        {
                            snapshot.adapter = None;
                            snapshot.devices.clear();
                            // The module goes away with its adapter.
                            change = Change::Patch;
                        }
                    }
                    _ => {}
                }
            }
            change
        }
        "PropertiesChanged" => {
            let Ok((interface, changed, _invalidated)) = message
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            else {
                return Change::Ignore;
            };
            let Some(path) = header.path().map(|path| path.as_str().to_owned()) else {
                return Change::Ignore;
            };
            merge(snapshot, &path, &interface, &changed)
        }
        _ => Change::Ignore,
    }
}

/// Fold one interface's properties into the tree, creating the object when it
/// is new, and report how far the fold reached. Missing keys keep their
/// previous value, which is exactly what `PropertiesChanged` means.
fn merge(
    snapshot: &mut Snapshot,
    path: &str,
    interface: &str,
    props: &HashMap<String, OwnedValue>,
) -> Change {
    match interface {
        IF_ADAPTER => {
            // One bar, one adapter: the first one BlueZ reports.
            if snapshot
                .adapter
                .as_ref()
                .is_some_and(|adapter| adapter.path != path)
            {
                return Change::Ignore;
            }
            // An adapter appearing unhides the module, whichever way it is
            // powered.
            let created = snapshot.adapter.is_none();
            let adapter = snapshot.adapter.get_or_insert_with(|| Adapter {
                path: path.to_owned(),
                ..Adapter::default()
            });
            let mut change = if created { Change::Patch } else { Change::Ignore };
            if let Some(powered) = bool_of(props, "Powered") {
                if adapter.powered != powered {
                    change = Change::Patch;
                }
                adapter.powered = powered;
            }
            // Discovery drives the popup's scan button and nothing on the bar,
            // so a scan starting behind a shut popup costs no frame.
            if let Some(discovering) = bool_of(props, "Discovering") {
                if adapter.discovering != discovering {
                    change = change.max(Change::Detail);
                }
                adapter.discovering = discovering;
            }
            // The alias and the address name the adapter in the popup header;
            // the cell never prints either.
            if let Some(alias) = string_of(props, "Alias").or_else(|| string_of(props, "Name")) {
                if adapter.alias != alias {
                    change = change.max(Change::Detail);
                }
                adapter.alias = alias;
            }
            if let Some(address) = string_of(props, "Address") {
                if adapter.address != address {
                    change = change.max(Change::Detail);
                }
                adapter.address = address;
            }
            change
        }
        IF_DEVICE => {
            let is_new = snapshot.device_mut(path).is_none();
            if is_new {
                snapshot.devices.push(Device {
                    path: path.to_owned(),
                    ..Device::default()
                });
            }
            let Some(device) = snapshot.device_mut(path) else {
                return Change::Ignore;
            };
            // A device the bar has not seen before is a popup row until it is
            // connected, which the `Connected` fold below catches.
            let mut change = if is_new { Change::Detail } else { Change::Ignore };
            if let Some(connected) = bool_of(props, "Connected") {
                if device.connected != connected {
                    // The glyph, the count, the name and the battery all follow
                    // from which devices are connected.
                    change = Change::Patch;
                }
                device.connected = connected;
            }
            // Pairing state only decides which popup section a row sits in.
            if let Some(paired) = bool_of(props, "Paired") {
                if device.paired != paired {
                    change = change.max(Change::Detail);
                }
                device.paired = paired;
            }
            let mut readdressed = false;
            if let Some(address) = string_of(props, "Address") {
                readdressed = device.address != address;
                // A name derived from the old address by the fallback below has
                // to follow the new one, or the device keeps being drawn by a
                // MAC it no longer answers to.
                if readdressed && device.name == device.address {
                    device.name.clear();
                }
                device.address = address;
            }
            // `Alias` is what the user renamed the device to; `Name` is what it
            // advertises. Neither is guaranteed to be present.
            if let Some(name) = string_of(props, "Alias").or_else(|| string_of(props, "Name")) {
                if device.name != name {
                    change = change.max(device_change(device.connected));
                }
                device.name = name;
            }
            // The icon picks a popup row's glyph, and RSSI is a popup row's
            // detail line: the cell has room for neither. RSSI is also by far
            // the loudest property on this bus.
            if let Some(icon) = string_of(props, "Icon") {
                if device.icon.as_deref() != Some(icon.as_str()) {
                    change = change.max(Change::Detail);
                }
                device.icon = Some(icon);
            }
            if let Some(rssi) = i16_of(props, "RSSI") {
                if device.rssi != Some(rssi) {
                    change = change.max(Change::Detail);
                }
                device.rssi = Some(rssi);
            }
            if device.name.is_empty() {
                device.name = device.address.clone();
            }
            // A device that advertises no name is drawn by its address, in the
            // cell as much as in the popup, so for those the address is cell
            // state and not detail.
            if readdressed {
                change = change.max(if device.name == device.address {
                    device_change(device.connected)
                } else {
                    Change::Detail
                });
            }
            change
        }
        IF_BATTERY => {
            let Some(percentage) = u8_of(props, "Percentage") else {
                return Change::Ignore;
            };
            match snapshot.device_mut(path) {
                Some(device) => {
                    let change = if device.battery == Some(percentage) {
                        Change::Ignore
                    } else {
                        device_change(device.connected)
                    };
                    device.battery = Some(percentage);
                    change
                }
                None => Change::Ignore,
            }
        }
        _ => Change::Ignore,
    }
}

/// Fold order within one object: an adapter owns devices, a device owns its
/// battery, so each has to exist before the next is applied.
fn merge_order(interface: &str) -> u8 {
    match interface {
        IF_ADAPTER => 0,
        IF_DEVICE => 1,
        _ => 2,
    }
}

/// The whole BlueZ tree in one round trip.
async fn read_tree(conn: &Connection) -> anyhow::Result<Snapshot> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BLUEZ)?
        .path("/")?
        .build()
        .await?;
    let objects = proxy
        .get_managed_objects()
        .await
        .context("reading the BlueZ object tree")?;

    // `Battery1` lives on the same object path as the `Device1` it belongs to,
    // and a map hands the two over in arbitrary order, so the interfaces are
    // folded in dependency order: adapter, then device, then the rest.
    let mut entries: Vec<_> = objects
        .iter()
        .flat_map(|(path, interfaces)| {
            interfaces
                .iter()
                .map(move |(interface, props)| (path.as_str(), interface.as_str(), props))
        })
        .collect();
    entries.sort_by_key(|(path, interface, _)| (merge_order(interface), path.len()));

    let mut snapshot = Snapshot::default();
    for (path, interface, props) in entries {
        merge(&mut snapshot, path, interface, props);
    }
    Ok(snapshot)
}

/// A signal stream for one match rule, filtered by the bus.
async fn signals(
    conn: &Connection,
    sender: &str,
    interface: Option<&str>,
    member: Option<&str>,
    arg0: Option<&str>,
) -> anyhow::Result<MessageStream> {
    let mut rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(sender.to_owned())?;
    if let Some(interface) = interface {
        rule = rule.interface(interface.to_owned())?;
    }
    if let Some(member) = member {
        rule = rule.member(member.to_owned())?;
    }
    if let Some(arg0) = arg0 {
        rule = rule.add_arg(arg0.to_owned())?;
    }
    Ok(MessageStream::for_match_rule(rule.build(), conn, Some(SIGNAL_QUEUE)).await?)
}

/// Third field of `NameOwnerChanged(name, old_owner, new_owner)`.
fn new_owner(message: &BusMessage) -> Option<String> {
    message
        .body()
        .deserialize::<(String, String, String)>()
        .ok()
        .map(|(_, _, new_owner)| new_owner)
}

// ------------------------------------------------------------------ D-Bus I/O

/// One system-bus connection for the whole module. BlueZ ends a discovery
/// session when the client that started it disconnects, so the connection that
/// calls `StartDiscovery` has to outlive the click.
async fn connection() -> anyhow::Result<Connection> {
    static CONNECTION: tokio::sync::OnceCell<Connection> = tokio::sync::OnceCell::const_new();
    CONNECTION
        .get_or_try_init(Connection::system)
        .await
        .cloned()
        .context("connecting to the system bus")
}

async fn has_owner(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    Ok(dbus.name_has_owner(name.try_into()?).await?)
}

async fn call(path: &str, interface: &'static str, method: &str) -> anyhow::Result<()> {
    let conn = connection().await?;
    let proxy = Proxy::new(&conn, BLUEZ, path.to_owned(), interface).await?;
    proxy
        .call::<_, _, ()>(method, &())
        .await
        .with_context(|| format!("{method} on {path}"))?;
    Ok(())
}

async fn set_property(
    path: &str,
    interface: &'static str,
    property: &str,
    value: Value<'_>,
) -> anyhow::Result<()> {
    let conn = connection().await?;
    let proxy = Proxy::new(&conn, BLUEZ, path.to_owned(), IF_PROPERTIES).await?;
    proxy
        .call::<_, _, ()>("Set", &(interface, property, value))
        .await
        .with_context(|| format!("setting {property} on {path}"))?;
    Ok(())
}

/// Waybar opened its TUI helpers the same way: `<terminal> -e <program>`.
async fn spawn(terminal: &str, program: &str) -> anyhow::Result<()> {
    let status = tokio::process::Command::new(terminal)
        .arg("-e")
        .arg(program)
        .status()
        .await
        .with_context(|| format!("spawning {terminal} -e {program}"))?;
    if !status.success() {
        anyhow::bail!("{terminal} -e {program}: {status}");
    }
    Ok(())
}

// ------------------------------------------------------- D-Bus value plumbing

/// Unwrap nested variants, so a `v` inside a `a{sv}` reads like the value it
/// carries.
fn peel<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => peel(inner),
        other => other,
    }
}

fn field<'a>(props: &'a HashMap<String, OwnedValue>, key: &str) -> Option<&'a Value<'a>> {
    props.get(key).map(|value| peel(value))
}

fn bool_of(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    match field(props, key)? {
        Value::Bool(value) => Some(*value),
        _ => None,
    }
}

fn string_of(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match field(props, key)? {
        Value::Str(value) => Some(value.as_str().to_owned()),
        Value::ObjectPath(value) => Some(value.as_str().to_owned()),
        _ => None,
    }
}

fn u8_of(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u8> {
    match field(props, key)? {
        Value::U8(value) => Some(*value),
        Value::U32(value) => u8::try_from(*value).ok(),
        Value::I32(value) => u8::try_from(*value).ok(),
        _ => None,
    }
}

fn i16_of(props: &HashMap<String, OwnedValue>, key: &str) -> Option<i16> {
    match field(props, key)? {
        Value::I16(value) => Some(*value),
        Value::I32(value) => i16::try_from(*value).ok(),
        Value::U8(value) => Some(i16::from(*value)),
        _ => None,
    }
}


