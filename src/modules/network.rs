//! NetworkManager module: the primary connection, its addresses, the known
//! profiles and the live wifi scan.
//!
//! Push driven end to end. One system-bus connection subscribes to
//! `PropertiesChanged` from NetworkManager — the manager itself
//! (`PrimaryConnection`, `State`, `Connectivity`, `WirelessEnabled`), the
//! active connections, the devices (`Device`, `Device.Wired`,
//! `Device.Wireless`), the access points and the IP configs — plus the
//! `Settings` and `Device.Wireless` object signals, and re-reads only what a
//! signal invalidated. Access-point strength updates are patched in place
//! instead of triggering a re-read, and bursts are coalesced, so a wifi scan
//! costs a handful of D-Bus round trips rather than one per signal.
//!
//! The popup's own content — the saved profiles and the list of neighbouring
//! access points — is read only while that popup is on screen: `open` is part
//! of the subscription's identity, so the session restarts with those reads,
//! and with the signals that only touch them, switched on and off. While the
//! popup is shut a snapshot that would draw the identical bar cell is dropped
//! rather than sent, so a neighbour's wifi scan costs the bar nothing.
//!
//! The one polled source is `/sys/class/net/<iface>/statistics/{rx,tx}_bytes`:
//! the kernel has no push interface for byte counters. It is sampled once a
//! second, and only while this module's popup is on screen, because the
//! sampler lives in a subscription that only exists then.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream, StreamExt, channel::mpsc::Sender};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};
use zbus::message::{Message as BusMessage, Type as MessageType};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::theme::{Island, Palette};

/// waybar drew `#network` on the `@tray` (mantle) background.
pub const ISLAND: Island = Island::Start;

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const IF_MANAGER: &str = "org.freedesktop.NetworkManager";
const IF_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const IF_PROFILE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const IF_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
/// Prefix shared by `Device`, `Device.Wired`, `Device.Wireless` and friends.
const IF_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const IF_WIRED: &str = "org.freedesktop.NetworkManager.Device.Wired";
const IF_WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const IF_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const IF_IP4: &str = "org.freedesktop.NetworkManager.IP4Config";
const IF_PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const DBUS: &str = "org.freedesktop.DBus";

/// NMActiveConnectionState.
const ACTIVE_ACTIVATING: u32 = 1;
const ACTIVE_ACTIVATED: u32 = 2;
/// NMConnectivityState. `UNKNOWN` is what a machine with the connectivity
/// check disabled reports, so it must not be read as "no internet".
const CONNECTIVITY_UNKNOWN: u32 = 0;
const CONNECTIVITY_FULL: u32 = 4;
/// NMDeviceType.
const DEVICE_WIFI: u32 = 2;
/// NMState: NM_STATE_CONNECTING.
const STATE_CONNECTING: u32 = 40;

/// Glyphs, all verified against the CommitMono Nerd Font cmap. The first seven
/// are exactly the ones `~/.config/waybar/modules/network.jsonc` used.
const ICON_ETHERNET: &str = "\u{f0200}";
const ICON_WIFI: [&str; 4] = ["\u{f091f}", "\u{f0922}", "\u{f0925}", "\u{f0928}"];
const ICON_WIFI_OFF: &str = "\u{f092e}";
const ICON_WIFI_DOWN: &str = "\u{f092f}";
/// md-wifi_strength_alert_outline: associated, but no route out.
const ICON_NO_INTERNET: &str = "\u{f092b}";
/// md-lan_disconnect, for a machine with no wifi at all.
const ICON_LAN_DOWN: &str = "\u{f0319}";
/// md-lan, for tunnels and anything that is neither wifi nor wired.
const ICON_OTHER: &str = "\u{f0317}";
const ICON_LOCK: &str = "\u{f033e}";
const ICON_DOWN: &str = "\u{f01da}";
const ICON_UP: &str = "\u{f0552}";
const ICON_SCAN: &str = "\u{f0450}";
const ICON_TERMINAL: &str = "\u{f018d}";

/// Longest connection name rendered in the bar.
const NAME_LIMIT: usize = 18;
/// Longest SSID rendered in a popup row.
const SSID_LIMIT: usize = 22;
/// Rows per popup list before it is cut off. The bar clips a popup at 720px,
/// so both lists stay short enough for the whole card to be visible.
const AP_LIMIT: usize = 6;
const PROFILE_LIMIT: usize = 6;
/// Signal bursts are coalesced for this long: one wifi scan retunes the
/// strength of every visible access point.
const COALESCE: Duration = Duration::from_millis(180);
/// Enough room for a scan's worth of signals while a snapshot is being read.
const SIGNAL_QUEUE: usize = 256;
/// Reconnect ladder for a NetworkManager that is down or restarting.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A session that lasted this long was healthy: the next failure starts the
/// ladder over instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// Byte counters have no push interface; this is the popup-only sample rate.
const RATE_INTERVAL: Duration = Duration::from_secs(1);
/// NetworkManager rate-limits scans itself; this only nudges it while the
/// popup is open.
const SCAN_INTERVAL: Duration = Duration::from_secs(20);

/// Manager properties that change what the bar shows.
const MANAGER_KEYS: &[&str] = &[
    "PrimaryConnection",
    "PrimaryConnectionType",
    "ActiveConnections",
    "ActivatingConnection",
    "State",
    "Connectivity",
    "WirelessEnabled",
    "WirelessHardwareEnabled",
    "Devices",
    "AllDevices",
];

/// Device properties worth a re-read. Everything else a device emits
/// (`Device.Statistics` counters above all) is ignored.
const DEVICE_KEYS: &[&str] = &[
    "State",
    "ActiveConnection",
    "Ip4Config",
    "Carrier",
    "Speed",
    "ActiveAccessPoint",
    "Bitrate",
    "LastScan",
];

#[derive(Debug, Clone)]
pub enum Event {
    Snapshot(Arc<Snapshot>),
    /// NetworkManager is gone or unreachable; the subscriber is retrying.
    Unavailable,
    /// One `/sys` sample pair, in bytes per second.
    Rates { rx: f64, tx: f64 },
    Activate {
        profile: String,
        device: String,
        specific: String,
    },
    Deactivate {
        active: String,
    },
    Rescan,
    SetWireless(bool),
    /// Flip the radio, whichever way it is set right now: a right-click on the
    /// bar cell has no room to say which way it meant.
    ToggleWireless,
    /// A brand-new SSID needs a passphrase prompt, which belongs in a TUI.
    Nmtui(String),
    /// Result of a mutation, so a failure is visible instead of silent.
    Done(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    snapshot: Option<Arc<Snapshot>>,
    available: bool,
    /// Last sampled rx/tx rate of the primary device, in bytes per second.
    rates: Option<(f64, f64)>,
    /// Object path of the profile or active connection with a call in flight.
    busy: Option<String>,
    error: Option<String>,
}

/// Everything the module renders, resolved by the subscription so that
/// `view`/`popup` never touch D-Bus.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    state: u32,
    connectivity: u32,
    wireless_enabled: bool,
    /// The wifi device, when the machine has one. Known even when the primary
    /// connection is wired, because the popup still lists access points.
    wifi: Option<Device>,
    primary: Option<Active>,
    /// Active connections that are not the primary one: VPNs, tunnels.
    secondary: Vec<Active>,
    profiles: Vec<Profile>,
    aps: Vec<Ap>,
}

/// What the bar cell draws, and nothing else: `headline`'s glyph, the words it
/// puts after the glyph, and the colour tier it picked. Two snapshots with the
/// same key paint the same cell, so the second one is not worth a frame.
///
/// The wifi glyph is bucketed — `wifi_icon` picks one of four icons per 25
/// strength points, exactly as waybar did — so the raw strength deliberately
/// stays out of the key: a link that drifts inside one bucket redraws nothing.
type BarKey = (&'static str, &'static str, Tier);

/// The colour half of a headline, without a palette in hand. Split out of
/// `headline` — the same split as cpu's `usage_tier`/`usage_state` — so the key
/// above can compare colours a subscription has no palette to look up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Connected, with a route out.
    Up,
    /// Coming up.
    Pending,
    /// Associated, but nothing past the local link.
    Degraded,
    /// Radio switched off: the user's own doing, not a fault.
    Off,
    /// Nothing connected.
    Down,
}

impl Tier {
    fn color(self, palette: &Palette) -> cosmic::iced::Color {
        match self {
            Tier::Up => palette.fg(),
            Tier::Pending => palette.peach,
            Tier::Degraded => palette.yellow,
            Tier::Off => palette.muted(),
            Tier::Down => palette.red,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Device {
    path: String,
    iface: String,
    kind: u32,
}

#[derive(Debug, Default, Clone)]
struct Active {
    path: String,
    /// Object path of the profile in `Settings` that this came from.
    profile: String,
    id: String,
    kind: String,
    state: u32,
    device: Device,
    ap: Option<ApLink>,
    /// Wifi link rate, in kb/s.
    bitrate: Option<u32>,
    /// Wired link speed, in Mb/s.
    speed: Option<u32>,
    carrier: Option<bool>,
    ip4: Ip4,
}

#[derive(Debug, Default, Clone)]
struct ApLink {
    path: String,
    ssid: String,
    strength: u8,
    /// MHz.
    frequency: u32,
}

#[derive(Debug, Default, Clone)]
struct Ip4 {
    addresses: Vec<String>,
    gateway: Option<String>,
    dns: Vec<String>,
}

#[derive(Debug, Clone)]
struct Profile {
    path: String,
    id: String,
    kind: String,
    ssid: Option<String>,
    /// Object path of the active connection using this profile, when it is up.
    active: Option<String>,
}

#[derive(Debug, Clone)]
struct Ap {
    path: String,
    ssid: String,
    strength: u8,
    security: &'static str,
    /// MHz.
    frequency: u32,
    active: bool,
    /// Profile that can be activated for this SSID, when one is saved.
    profile: Option<String>,
}

impl State {
    /// The always-on signal subscription, plus — only while the popup is on
    /// screen — a 1s `/sys` byte-counter sampler and a scan nudge. iced starts
    /// and stops those two as they enter and leave this list.
    ///
    /// `open` is part of the signal subscription's identity too: the session
    /// behind it reads the popup's own content only while the popup is up, so
    /// iced restarts it with those reads switched on and off.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        let mut subscriptions = vec![Subscription::run_with(open, events)];
        if open && self.available {
            if let Some(iface) = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.primary.as_ref())
                .map(|active| Arc::<str>::from(active.device.iface.as_str()))
                .filter(|iface| !iface.is_empty())
            {
                subscriptions.push(Subscription::run_with(iface, rate_stream));
            }
            if let Some(device) = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.wifi.as_ref())
                .filter(|_| self.snapshot.as_ref().is_some_and(|s| s.wireless_enabled))
                .map(|wifi| Arc::<str>::from(wifi.path.as_str()))
            {
                subscriptions.push(Subscription::run_with(device, scan_stream));
            }
        }
        Subscription::batch(subscriptions)
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
                self.rates = None;
                self.busy = None;
                Task::none()
            }
            Event::Rates { rx, tx } => {
                self.rates = Some((rx, tx));
                Task::none()
            }
            Event::Activate {
                profile,
                device,
                specific,
            } => {
                self.busy = Some(profile.clone());
                self.error = None;
                Task::batch([
                    mutate(async move { activate(profile, device, specific).await }),
                    close_popup(),
                ])
            }
            Event::Deactivate { active } => {
                self.busy = Some(active.clone());
                self.error = None;
                Task::batch([
                    mutate(async move { deactivate(active).await }),
                    close_popup(),
                ])
            }
            Event::Rescan => match self.snapshot.as_ref().and_then(|s| s.wifi.as_ref()) {
                Some(wifi) => {
                    let device = wifi.path.clone();
                    mutate(async move { request_scan(&device).await })
                }
                None => Task::none(),
            },
            Event::SetWireless(enabled) => mutate(async move { set_wireless(enabled).await }),
            Event::ToggleWireless => match self.snapshot.as_ref() {
                Some(snapshot) => {
                    let enabled = !snapshot.wireless_enabled;
                    mutate(async move { set_wireless(enabled).await })
                }
                None => Task::none(),
            },
            Event::Nmtui(terminal) => Task::batch([
                mutate(async move { spawn(&terminal, "nmtui").await }),
                close_popup(),
            ]),
            Event::Done(result) => {
                self.busy = None;
                self.error = result.err();
                Task::none()
            }
        }
    }

    /// `None` hides the module: no NetworkManager, nothing to say.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let snapshot = self.snapshot.as_ref().filter(|_| self.available)?;
        let (glyph, rest, color) = snapshot.headline(&ctx.palette);
        Some(crate::theme::label(
            glyph,
            rest,
            ctx.font_size,
            cosmic::theme::Text::Color(color),
        ))
    }

    /// The rate sampler and the connection state already push a redraw when
    /// they change, so the bar never needs a per-second tick for this module.
    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.available && self.snapshot.is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let snapshot = self.snapshot.as_ref().filter(|_| self.available)?;
        let palette = ctx.palette;
        let mut body = widget::Column::new().spacing(6).width(Length::Fill);

        let (glyph, _, color) = snapshot.headline(&palette);
        let mut header = widget::Row::new()
            .align_y(Alignment::Center)
            .spacing(8)
            .push(
                // The bar cell is the glyph alone, so the popup is where the
                // network's name is spelled out.
                crate::theme::label(
                    glyph,
                    snapshot.name(),
                    ctx.font_size,
                    cosmic::theme::Text::Color(color),
                )
                .apply(widget::container)
                .width(Length::Fill),
            );
        if snapshot.wifi.is_some() {
            if snapshot.wireless_enabled {
                header = header.push(button(palette, ICON_SCAN, Event::Rescan));
            }
            header = header.push(button(
                palette,
                if snapshot.wireless_enabled {
                    "wifi off"
                } else {
                    "wifi on"
                },
                Event::SetWireless(!snapshot.wireless_enabled),
            ));
        }
        header = header.push(button(
            palette,
            ICON_TERMINAL,
            Event::Nmtui(ctx.terminal.clone()),
        ));
        body = body
            .push(header)
            .push(detail(
                ctx,
                "state",
                state_name(snapshot.state, snapshot.connectivity),
            ));

        if let Some(active) = &snapshot.primary {
            if !active.device.iface.is_empty() {
                body = body.push(detail(ctx, "interface", active.device.iface.clone()));
            }
            if !active.ip4.addresses.is_empty() {
                body = body.push(detail(ctx, "ipv4", active.ip4.addresses.join(", ")));
            }
            if let Some(gateway) = &active.ip4.gateway {
                body = body.push(detail(ctx, "gateway", gateway.clone()));
            }
            if !active.ip4.dns.is_empty() {
                body = body.push(detail(ctx, "dns", active.ip4.dns.join(", ")));
            }
            if let Some(ap) = &active.ap {
                let mut signal = format!("{}% · {:.3} GHz", ap.strength, ap.frequency as f32 / 1000.0);
                if let Some(bitrate) = active.bitrate {
                    signal.push_str(&format!(" · {} Mb/s", bitrate / 1000));
                }
                body = body.push(detail(ctx, "signal", signal));
            }
            if let Some(speed) = active.speed.filter(|speed| *speed > 0) {
                let carrier = match active.carrier {
                    Some(false) => " · no carrier",
                    _ => "",
                };
                body = body.push(detail(ctx, "link", format!("{speed} Mb/s{carrier}")));
            }
            match self.rates {
                Some((rx, tx)) => {
                    body = body.push(detail(
                        ctx,
                        "traffic",
                        format!("{ICON_DOWN} {}   {ICON_UP} {}", rate(rx), rate(tx)),
                    ));
                }
                None => {
                    body = body.push(detail(ctx, "traffic", "sampling…".into()));
                }
            }
            if !snapshot.secondary.is_empty() {
                let also = snapshot
                    .secondary
                    .iter()
                    .map(|active| active.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                body = body.push(detail(ctx, "also up", also));
            }
        }

        // Saved profiles, active ones first. Both lists are capped: the bar
        // clips a popup at 720px, and a list that runs past the edge is worse
        // than one that says how much it is hiding.
        let profiles: Vec<&Profile> = snapshot
            .profiles
            .iter()
            .filter(|profile| profile.kind != "loopback")
            .collect();
        if !profiles.is_empty() {
            body = body
                .push(widget::divider::horizontal::default())
                .push(section(ctx, "saved"));
            for profile in profiles.iter().take(PROFILE_LIMIT) {
                let action = match &profile.active {
                    Some(active) => self.action(
                        palette,
                        "down",
                        active,
                        Event::Deactivate {
                            active: active.clone(),
                        },
                    ),
                    None => self.action(
                        palette,
                        "up",
                        &profile.path,
                        Event::Activate {
                            profile: profile.path.clone(),
                            // "/" lets NetworkManager pick the device.
                            device: "/".into(),
                            specific: "/".into(),
                        },
                    ),
                };
                body = body.push(
                    widget::Row::new()
                        .push(
                            crate::theme::text(elide(&profile.id, SSID_LIMIT))
                                .class(cosmic::theme::Text::Color(if profile.active.is_some() {
                                    palette.green
                                } else {
                                    palette.fg()
                                }))
                                .width(Length::Fill),
                        )
                        .push(
                            crate::theme::text(kind_name(&profile.kind).to_string())
                                .size(ctx.small())
                                .class(cosmic::theme::Text::Color(palette.overlay0)),
                        )
                        .push(action)
                        .align_y(Alignment::Center)
                        .spacing(8),
                );
            }
            body = more(body, ctx, profiles.len(), PROFILE_LIMIT);
        }

        if let Some(wifi) = &snapshot.wifi {
            body = body
                .push(widget::divider::horizontal::default())
                .push(section(ctx, "nearby"));
            if !snapshot.wireless_enabled {
                body = body.push(
                    crate::theme::text("radio off")
                        .size(ctx.small())
                        .class(cosmic::theme::Text::Color(palette.muted())),
                );
            } else if snapshot.aps.is_empty() {
                body = body.push(
                    crate::theme::text("scanning…")
                        .size(ctx.small())
                        .class(cosmic::theme::Text::Color(palette.muted())),
                );
            }
            for ap in snapshot.aps.iter().take(AP_LIMIT) {
                let security = if ap.security == "open" {
                    ap.security.to_string()
                } else {
                    format!("{ICON_LOCK} {}", ap.security)
                };
                let action: Element<'_, Message> = if ap.active {
                    crate::theme::text("active")
                        .size(ctx.small())
                        .class(cosmic::theme::Text::Color(palette.green))
                        .into()
                } else {
                    match &ap.profile {
                        Some(profile) => self.action(
                            palette,
                            "join",
                            profile,
                            Event::Activate {
                                profile: profile.clone(),
                                device: wifi.path.clone(),
                                specific: ap.path.clone(),
                            },
                        ),
                        // A new SSID needs a passphrase prompt: that is nmtui's job.
                        None => button(palette, ICON_TERMINAL, Event::Nmtui(ctx.terminal.clone())),
                    }
                };
                body = body.push(
                    widget::Row::new()
                        .push(
                            crate::theme::text(format!(
                                "{} {}",
                                wifi_icon(ap.strength),
                                elide(&ap.ssid, SSID_LIMIT)
                            ))
                            .class(cosmic::theme::Text::Color(if ap.active {
                                palette.green
                            } else {
                                palette.fg()
                            }))
                            .width(Length::Fill),
                        )
                        .push(
                            crate::theme::text(format!(
                                "{}% · {} · {security}",
                                ap.strength,
                                band(ap.frequency)
                            ))
                            .size(ctx.small())
                            .class(cosmic::theme::Text::Color(palette.overlay0)),
                        )
                        .push(action)
                        .align_y(Alignment::Center)
                        .spacing(8),
                );
            }
            body = more(body, ctx, snapshot.aps.len(), AP_LIMIT);
        }

        if let Some(error) = &self.error {
            body = body.push(widget::divider::horizontal::default()).push(
                crate::theme::text(error.clone())
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(palette.red)),
            );
        }

        Some(body.apply(widget::container).padding(12).into())
    }

    /// A row button that turns into an inert `…` while its call is in flight.
    fn action<'a>(
        &self,
        palette: Palette,
        label: &'a str,
        key: &str,
        event: Event,
    ) -> Element<'a, Message> {
        if self.busy.as_deref() == Some(key) {
            return crate::theme::text("…").into();
        }
        button(palette, label, event)
    }
}

impl Snapshot {
    /// Glyph, the rest of the label, and its colour. Split because the bar
    /// draws the two halves as separate text runs: one string would carry a
    /// full mono space between icon and name.
    fn headline(&self, palette: &Palette) -> (&'static str, &'static str, cosmic::iced::Color) {
        let (glyph, rest, tier) = self.bar_key();
        (glyph, rest, tier.color(palette))
    }

    /// The cell's visual identity, palette-free. `headline` is the only other
    /// caller and does nothing but look the tier's colour up, so this key
    /// cannot drift from what `view` draws.
    ///
    /// A working connection is the glyph alone — the wifi bars already say
    /// "connected, this strong", and which network it is belongs in the popup.
    /// Only the states a glyph cannot spell out keep words.
    fn bar_key(&self) -> BarKey {
        if let Some(active) = &self.primary {
            let icon = if active.kind.starts_with("802-11-wireless") {
                wifi_icon(active.ap.as_ref().map_or(0, |ap| ap.strength))
            } else if active.kind.starts_with("802-3-ethernet") {
                ICON_ETHERNET
            } else {
                ICON_OTHER
            };
            if active.state != ACTIVE_ACTIVATED {
                return (icon, "connecting…", Tier::Pending);
            }
            if self.connectivity != CONNECTIVITY_FULL
                && self.connectivity != CONNECTIVITY_UNKNOWN
            {
                return (ICON_NO_INTERNET, "no internet", Tier::Degraded);
            }
            return (icon, "", Tier::Up);
        }

        let down = if self.wifi.is_some() {
            ICON_WIFI_DOWN
        } else {
            ICON_LAN_DOWN
        };
        if self.wifi.is_some() && !self.wireless_enabled {
            return (ICON_WIFI_OFF, "off", Tier::Off);
        }
        if self.state == STATE_CONNECTING
            || self
                .secondary
                .iter()
                .any(|active| active.state == ACTIVE_ACTIVATING)
        {
            return (down, "connecting…", Tier::Pending);
        }
        (down, "offline", Tier::Down)
    }

    /// The device identities `State::subscription` keys its popup-only children
    /// on. None of it is visible in the cell, but a change here must still
    /// reach the bar: the rate sampler and the scan nudge are built from the
    /// snapshot the bar is holding the instant the popup opens, before the
    /// restarted session's first detailed snapshot can land, so a stale
    /// interface would sample a device that is gone and a stale radio flag
    /// would ask a switched-off radio to scan.
    fn subscription_keys(&self) -> (&str, &str, bool) {
        (
            self.primary
                .as_ref()
                .map_or("", |active| active.device.iface.as_str()),
            self.wifi.as_ref().map_or("", |wifi| wifi.path.as_str()),
            self.wireless_enabled,
        )
    }

    /// The connection's own name: an SSID for wifi, the profile id otherwise,
    /// and a word for the states where there is nothing connected to name.
    fn name(&self) -> String {
        let Some(active) = &self.primary else {
            return if self.wifi.is_some() && !self.wireless_enabled {
                "wifi off".to_string()
            } else {
                "offline".to_string()
            };
        };
        let name = active
            .ap
            .as_ref()
            .map(|ap| ap.ssid.clone())
            .filter(|ssid| !ssid.is_empty())
            .unwrap_or_else(|| active.id.clone());
        elide(&name, NAME_LIMIT)
    }

    /// Strongest first, one row per SSID.
    fn sort_aps(&mut self) {
        self.aps
            .sort_by(|a, b| a.ssid.cmp(&b.ssid).then(b.strength.cmp(&a.strength)));
        self.aps.dedup_by(|a, b| a.ssid == b.ssid);
        self.aps.sort_by(|a, b| {
            b.active
                .cmp(&a.active)
                .then(b.strength.cmp(&a.strength))
                .then(a.ssid.cmp(&b.ssid))
        });
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Network(event))
}

fn close_popup() -> Task<Message> {
    Task::done(cosmic::Action::App(Message::ClosePopup))
}

/// Run a mutation and report its outcome, so a refused call is visible.
fn mutate(
    call: impl Future<Output = anyhow::Result<()>> + Send + 'static,
) -> Task<Message> {
    Task::future(async move {
        cosmic::Action::App(event_message(Event::Done(
            call.await.map_err(|error| format!("{error:#}")),
        )))
    })
}

fn wifi_icon(strength: u8) -> &'static str {
    // waybar picked one of four icons by `strength / 25`.
    ICON_WIFI[((strength as usize) / 25).min(ICON_WIFI.len() - 1)]
}

/// Wifi band from an access point's channel frequency.
fn band(mhz: u32) -> &'static str {
    match mhz {
        0 => "?",
        1..3000 => "2.4 GHz",
        3000..5925 => "5 GHz",
        _ => "6 GHz",
    }
}

fn kind_name(kind: &str) -> &str {
    match kind {
        "802-11-wireless" => "wifi",
        "802-3-ethernet" => "ethernet",
        "vpn" | "wireguard" => "vpn",
        other => other,
    }
}

fn state_name(state: u32, connectivity: u32) -> String {
    let state = match state {
        10 => "asleep",
        20 => "disconnected",
        30 => "disconnecting",
        40 => "connecting",
        50 => "connected (local)",
        60 => "connected (site)",
        70 => "connected",
        _ => "unknown",
    };
    let connectivity = match connectivity {
        1 => " · no connectivity",
        2 => " · captive portal",
        3 => " · limited",
        _ => "",
    };
    format!("{state}{connectivity}")
}

fn rate(bytes: f64) -> String {
    match bytes {
        bytes if bytes < 1_000.0 => format!("{bytes:.0} B/s"),
        bytes if bytes < 1_000_000.0 => format!("{:.0} kB/s", bytes / 1_000.0),
        bytes => format!("{:.1} MB/s", bytes / 1_000_000.0),
    }
}

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A text button whose label is drawn in the bar's font: `button::text` would
/// hand the label to COSMIC's Open Sans and render every nerd glyph as tofu.
fn button<'a>(
    palette: Palette,
    label: impl Into<std::borrow::Cow<'a, str>> + 'a,
    event: Event,
) -> Element<'a, Message> {
    widget::button::custom(crate::theme::text(label))
        .class(crate::theme::chip(palette))
        .on_press(event_message(event))
        .into()
}

fn detail<'a>(ctx: &Ctx, label: &'a str, value: String) -> Element<'a, Message> {
    widget::Row::new()
        .spacing(8)
        .push(
            crate::theme::text(label)
                .size(ctx.small())
                .class(cosmic::theme::Text::Color(ctx.palette.overlay0))
                .width(Length::Fixed(76.0)),
        )
        .push(
            crate::theme::text(value)
                .size(ctx.small())
                .class(cosmic::theme::Text::Color(ctx.palette.muted()))
                .width(Length::Fill),
        )
        .into()
}

/// `+N more` when a list was cut off.
fn more<'a>(
    body: widget::Column<'a, Message, cosmic::Theme>,
    ctx: &Ctx,
    total: usize,
    limit: usize,
) -> widget::Column<'a, Message, cosmic::Theme> {
    if total <= limit {
        return body;
    }
    body.push(
        crate::theme::text(format!("+{} more", total - limit))
            .size(ctx.small())
            .class(cosmic::theme::Text::Color(ctx.palette.overlay0)),
    )
}

fn section<'a>(ctx: &Ctx, label: &'a str) -> Element<'a, Message> {
    crate::theme::text(label)
        .size(ctx.small())
        .class(cosmic::theme::Text::Color(ctx.palette.overlay0))
        .into()
}

// ---------------------------------------------------------------- subscription

/// The always-on half: NetworkManager's signals, turned into snapshots. `open`
/// is part of the subscription's identity, so opening the popup restarts the
/// session with the popup-only reads switched on and closing it switches them
/// back off.
fn events(open: &bool) -> impl Stream<Item = Message> + use<> {
    let detailed = *open;
    cosmic::iced::stream::channel(8, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            if let Err(error) = session(&mut sender, detailed).await {
                log::debug!("network: session ended: {error:#}");
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

/// One connection's worth of snapshots. Returns when NetworkManager's bus name
/// changes owner, so the caller re-reads everything from scratch.
///
/// `detailed` is the popup's state. While the popup is shut nothing on screen
/// reads the saved profiles or the neighbourhood scan, so this session neither
/// reads them nor wakes for the signals that only touch them.
async fn session(sender: &mut Sender<Message>, detailed: bool) -> anyhow::Result<()> {
    let bus = Bus::system().await?;

    // Subscribed before the ownership check, so a NetworkManager that starts
    // during the check is not missed.
    let owner = signals(&bus.conn, DBUS, Some(DBUS), Some("NameOwnerChanged"), Some(NM)).await?;
    let mut owner = owner;
    if !bus.has_owner(NM).await? {
        let _ = sender.send(event_message(Event::Unavailable)).await;
        log::debug!("network: waiting for {NM} on the bus");
        loop {
            let message = owner.next().await.context("bus stream ended")??;
            if new_owner(&message).is_some_and(|owner| !owner.is_empty()) {
                break;
            }
        }
    }

    let changed = signals(
        &bus.conn,
        NM,
        Some(IF_PROPERTIES),
        Some("PropertiesChanged"),
        None,
    )
    .await?;
    let wireless = signals(&bus.conn, NM, Some(IF_WIRELESS), None, None).await?;
    let settings = signals(&bus.conn, NM, Some(IF_SETTINGS), None, None).await?;
    let mut signals = futures::stream::select_all(vec![changed, wireless, settings, owner]);

    let mut snapshot = read_snapshot(&bus, detailed).await;
    // The snapshot the bar is holding, so the next one can be measured against
    // what is actually on screen before it costs a relayout.
    let mut drawn = None;
    if !publish(sender, &mut drawn, &snapshot, detailed).await {
        return Ok(());
    }

    let mut reread = false;
    let mut dirty = false;
    loop {
        // With work pending, the loop waits only for the coalescing window:
        // a scan's worth of strength updates becomes one snapshot.
        let message = if dirty {
            match tokio::time::timeout(COALESCE, signals.next()).await {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(()),
                Err(_) => {
                    if reread {
                        snapshot = read_snapshot(&bus, detailed).await;
                        reread = false;
                    } else {
                        snapshot.sort_aps();
                    }
                    dirty = false;
                    if !publish(sender, &mut drawn, &snapshot, detailed).await {
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

        match classify(&message?, &mut snapshot, detailed) {
            Change::Ignore => {}
            Change::Patch => dirty = true,
            Change::Reread => {
                reread = true;
                dirty = true;
            }
            // NetworkManager restarted: every object path we hold is stale.
            Change::Restart => return Ok(()),
        }
    }
}

/// Hand a snapshot to the bar unless the bar would redraw the module exactly as
/// it already looks. Returns `false` once the bar has dropped the subscription.
///
/// While the popup is shut the cell is the whole module, and NetworkManager's
/// property churn keeps resolving to the same cell: a link's strength drifts
/// inside one `wifi_icon` bucket, a device republishes its bitrate, a scan
/// stamps `LastScan`. Each of those snapshots would cost a relayout and a
/// repaint of identical pixels. With the popup open every snapshot carries rows
/// the cell does not, so all of them go through.
async fn publish(
    sender: &mut Sender<Message>,
    drawn: &mut Option<Arc<Snapshot>>,
    snapshot: &Snapshot,
    detailed: bool,
) -> bool {
    if !detailed
        && drawn.as_ref().is_some_and(|last| {
            last.bar_key() == snapshot.bar_key()
                && last.subscription_keys() == snapshot.subscription_keys()
        })
    {
        return true;
    }
    let snapshot = Arc::new(snapshot.clone());
    *drawn = Some(Arc::clone(&snapshot));
    sender
        .send(event_message(Event::Snapshot(snapshot)))
        .await
        .is_ok()
}

enum Change {
    Ignore,
    /// The snapshot was updated in place; emit it.
    Patch,
    /// Re-read the snapshot from D-Bus, then emit it.
    Reread,
    Restart,
}

fn classify(message: &BusMessage, snapshot: &mut Snapshot, detailed: bool) -> Change {
    let header = message.header();
    let member = header.member().map(|member| member.as_str()).unwrap_or("");
    let path = header.path().map(|path| path.as_str()).unwrap_or("");
    match member {
        "NameOwnerChanged" => Change::Restart,
        // Neighbouring access points and saved profiles are popup-only rows.
        // With the popup shut, re-reading them would spend a GetSettings round
        // trip per profile and a GetAll per access point to build a snapshot
        // that draws the identical cell, so the signal is dropped instead.
        "AccessPointAdded" | "AccessPointRemoved" | "NewConnection" | "ConnectionRemoved" => {
            if detailed {
                Change::Reread
            } else {
                Change::Ignore
            }
        }
        "PropertiesChanged" => {
            let Ok((interface, changed, _invalidated)) =
                message
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            else {
                return Change::Ignore;
            };
            match interface.as_str() {
                IF_MANAGER => {
                    if changed.keys().any(|key| MANAGER_KEYS.contains(&key.as_str())) {
                        Change::Reread
                    } else {
                        Change::Ignore
                    }
                }
                IF_ACTIVE | IF_IP4 => Change::Reread,
                IF_AP => patch_strength(snapshot, path, &changed),
                interface if interface.starts_with(IF_DEVICE) => {
                    if changed.keys().any(|key| DEVICE_KEYS.contains(&key.as_str())) {
                        Change::Reread
                    } else {
                        Change::Ignore
                    }
                }
                _ => Change::Ignore,
            }
        }
        _ => Change::Ignore,
    }
}

/// Access-point strength moves constantly; patching it costs no round trips.
/// With the popup shut the access-point list is empty, so the only strength
/// this can touch is the primary link's own — and the cell draws that in
/// buckets, which is what `publish` compares.
fn patch_strength(
    snapshot: &mut Snapshot,
    path: &str,
    changed: &HashMap<String, OwnedValue>,
) -> Change {
    let Some(strength) = u8_of(changed, "Strength") else {
        return Change::Ignore;
    };
    let mut touched = false;
    if let Some(ap) = snapshot.aps.iter_mut().find(|ap| ap.path == path) {
        touched |= ap.strength != strength;
        ap.strength = strength;
    }
    if let Some(link) = snapshot
        .primary
        .as_mut()
        .and_then(|active| active.ap.as_mut())
        .filter(|link| link.path == path)
    {
        touched |= link.strength != strength;
        link.strength = strength;
    }
    if touched { Change::Patch } else { Change::Ignore }
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

// ------------------------------------------------------------- popup sampling

/// `Subscription::run_with` takes a plain function pointer, so a popup-only
/// stream is boxed instead of returning an opaque type tied to the argument's
/// lifetime.
type Boxed = std::pin::Pin<Box<dyn Stream<Item = Message> + Send>>;

/// Byte counters are the one source with no push interface, so they are
/// polled — once a second, and only while the popup is on screen.
fn rate_stream(iface: &Arc<str>) -> Boxed {
    let iface = Arc::clone(iface);
    Box::pin(cosmic::iced::stream::channel(4, async move |mut sender| {
        let mut previous: Option<(Instant, u64, u64)> = None;
        loop {
            if let Some((rx, tx)) = counters(&iface) {
                let now = Instant::now();
                if let Some((then, rx0, tx0)) = previous {
                    let seconds = now.duration_since(then).as_secs_f64().max(0.001);
                    let event = Event::Rates {
                        rx: rx.saturating_sub(rx0) as f64 / seconds,
                        tx: tx.saturating_sub(tx0) as f64 / seconds,
                    };
                    if sender.send(event_message(event)).await.is_err() {
                        return;
                    }
                }
                previous = Some((now, rx, tx));
            }
            tokio::time::sleep(RATE_INTERVAL).await;
        }
    }))
}

/// Two `read`s of a sysfs u64 each second; not worth a thread hop.
fn counters(iface: &str) -> Option<(u64, u64)> {
    let counter = |name: &str| -> Option<u64> {
        std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{name}_bytes"))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    Some((counter("rx")?, counter("tx")?))
}

/// Opening the popup asks for a fresh scan; the results arrive as signals.
fn scan_stream(device: &Arc<str>) -> Boxed {
    let device = Arc::clone(device);
    Box::pin(cosmic::iced::stream::channel(1, async move |mut sender| {
        loop {
            if let Err(error) = request_scan(&device).await {
                log::debug!("network: RequestScan: {error:#}");
                let _ = sender
                    .send(event_message(Event::Done(Err(format!("{error:#}")))))
                    .await;
            }
            tokio::time::sleep(SCAN_INTERVAL).await;
        }
    }))
}

// ------------------------------------------------------------------ D-Bus I/O

struct Bus {
    conn: Connection,
}

impl Bus {
    async fn system() -> anyhow::Result<Self> {
        Ok(Self {
            conn: Connection::system()
                .await
                .context("connecting to the system bus")?,
        })
    }

    async fn has_owner(&self, name: &str) -> anyhow::Result<bool> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        Ok(dbus.name_has_owner(name.try_into()?).await?)
    }

    /// Every property of one interface in one round trip. A vanished object or
    /// an unexpected reply degrades to an empty map: a snapshot with holes in
    /// it beats no snapshot at all.
    async fn props(&self, path: &str, interface: &'static str) -> HashMap<String, OwnedValue> {
        match self.try_props(path, interface).await {
            Ok(props) => props,
            Err(error) => {
                log::debug!("network: GetAll {interface} on {path}: {error:#}");
                HashMap::new()
            }
        }
    }

    async fn try_props(
        &self,
        path: &str,
        interface: &'static str,
    ) -> anyhow::Result<HashMap<String, OwnedValue>> {
        let proxy = zbus::fdo::PropertiesProxy::builder(&self.conn)
            .destination(NM)?
            .path(path.to_owned())?
            .build()
            .await?;
        Ok(proxy.get_all(interface.try_into()?).await?)
    }

    async fn proxy(&self, path: &str, interface: &'static str) -> anyhow::Result<Proxy<'static>> {
        Ok(Proxy::new(&self.conn, NM, path.to_owned(), interface).await?)
    }
}

/// `detailed` adds the two halves only the popup shows: the saved profiles and
/// the neighbourhood scan, which cost a round trip per profile and per access
/// point. With the popup shut they stay empty, and opening it restarts the
/// session with them switched on.
async fn read_snapshot(bus: &Bus, detailed: bool) -> Snapshot {
    let manager = bus.props(NM_PATH, IF_MANAGER).await;
    let mut snapshot = Snapshot {
        state: u32_of(&manager, "State").unwrap_or(0),
        connectivity: u32_of(&manager, "Connectivity").unwrap_or(CONNECTIVITY_UNKNOWN),
        wireless_enabled: bool_of(&manager, "WirelessEnabled").unwrap_or(false),
        ..Snapshot::default()
    };

    // Devices first: the popup lists access points even when the primary
    // connection is the wired one.
    let mut devices: HashMap<String, Device> = HashMap::new();
    for path in paths_of(&manager, "Devices") {
        let props = bus.props(&path, IF_DEVICE).await;
        let device = Device {
            path: path.clone(),
            iface: string_of(&props, "Interface").unwrap_or_default(),
            kind: u32_of(&props, "DeviceType").unwrap_or(0),
        };
        if device.kind == DEVICE_WIFI && snapshot.wifi.is_none() {
            snapshot.wifi = Some(device.clone());
        }
        devices.insert(path, device);
    }

    let primary = path_of(&manager, "PrimaryConnection").filter(|path| path != "/");
    for path in paths_of(&manager, "ActiveConnections") {
        let active = read_active(bus, &path, &devices).await;
        if primary.as_deref() == Some(path.as_str()) {
            snapshot.primary = Some(active);
        } else {
            snapshot.secondary.push(active);
        }
    }
    // A connection that is still coming up is not primary yet, but the bar
    // should say "connecting" with its name rather than "offline".
    if snapshot.primary.is_none()
        && let Some(index) = snapshot.secondary.iter().position(|active| {
            active.state == ACTIVE_ACTIVATING
                && (active.kind.starts_with("802-11-wireless")
                    || active.kind.starts_with("802-3-ethernet"))
        }) {
            snapshot.primary = Some(snapshot.secondary.remove(index));
        }

    if detailed {
        snapshot.profiles = read_profiles(bus, &snapshot).await;
        if let Some(wifi) = snapshot.wifi.clone() {
            snapshot.aps = read_access_points(bus, &wifi, &snapshot).await;
        }
        snapshot.sort_aps();
    }
    snapshot
}

async fn read_active(bus: &Bus, path: &str, devices: &HashMap<String, Device>) -> Active {
    let props = bus.props(path, IF_ACTIVE).await;
    let device = paths_of(&props, "Devices")
        .into_iter()
        .find_map(|path| devices.get(&path).cloned())
        .unwrap_or_default();
    let mut active = Active {
        path: path.to_owned(),
        profile: path_of(&props, "Connection").unwrap_or_default(),
        id: string_of(&props, "Id").unwrap_or_default(),
        kind: string_of(&props, "Type").unwrap_or_default(),
        state: u32_of(&props, "State").unwrap_or(0),
        device,
        ..Active::default()
    };

    if let Some(ip4) = path_of(&props, "Ip4Config").filter(|path| path != "/") {
        active.ip4 = read_ip4(bus, &ip4).await;
    }
    if active.device.path.is_empty() {
        return active;
    }
    if active.kind.starts_with("802-11-wireless") {
        let wireless = bus.props(&active.device.path, IF_WIRELESS).await;
        active.bitrate = u32_of(&wireless, "Bitrate");
        if let Some(path) = path_of(&wireless, "ActiveAccessPoint").filter(|path| path != "/") {
            let props = bus.props(&path, IF_AP).await;
            active.ap = Some(ApLink {
                ssid: ssid_of(&props).unwrap_or_default(),
                strength: u8_of(&props, "Strength").unwrap_or(0),
                frequency: u32_of(&props, "Frequency").unwrap_or(0),
                path,
            });
        }
    } else if active.kind.starts_with("802-3-ethernet") {
        let wired = bus.props(&active.device.path, IF_WIRED).await;
        active.speed = u32_of(&wired, "Speed");
        active.carrier = bool_of(&wired, "Carrier");
    }
    active
}

async fn read_ip4(bus: &Bus, path: &str) -> Ip4 {
    let props = bus.props(path, IF_IP4).await;
    Ip4 {
        addresses: dicts_of(&props, "AddressData")
            .into_iter()
            .filter_map(|entry| {
                let address = dict_string(&entry, "address")?;
                Some(match dict_u32(&entry, "prefix") {
                    Some(prefix) => format!("{address}/{prefix}"),
                    None => address,
                })
            })
            .collect(),
        gateway: string_of(&props, "Gateway").filter(|gateway| !gateway.is_empty()),
        dns: dicts_of(&props, "NameserverData")
            .into_iter()
            .filter_map(|entry| dict_string(&entry, "address"))
            .collect(),
    }
}

async fn read_profiles(bus: &Bus, snapshot: &Snapshot) -> Vec<Profile> {
    let paths: Vec<OwnedObjectPath> = match bus.proxy(SETTINGS_PATH, IF_SETTINGS).await {
        Ok(proxy) => match proxy.call("ListConnections", &()).await {
            Ok(paths) => paths,
            Err(error) => {
                log::debug!("network: ListConnections: {error}");
                return Vec::new();
            }
        },
        Err(error) => {
            log::debug!("network: Settings proxy: {error:#}");
            return Vec::new();
        }
    };

    let mut profiles = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path.as_str().to_owned();
        let Ok(proxy) = bus.proxy(&path, IF_PROFILE).await else {
            continue;
        };
        let settings: HashMap<String, HashMap<String, OwnedValue>> =
            match proxy.call("GetSettings", &()).await {
                Ok(settings) => settings,
                Err(error) => {
                    log::debug!("network: GetSettings on {path}: {error}");
                    continue;
                }
            };
        let Some(connection) = settings.get("connection") else {
            continue;
        };
        let active = snapshot
            .primary
            .iter()
            .chain(snapshot.secondary.iter())
            .find(|active| active.profile == path)
            .map(|active| active.path.clone());
        profiles.push(Profile {
            id: string_of(connection, "id").unwrap_or_else(|| path.clone()),
            kind: string_of(connection, "type").unwrap_or_default(),
            ssid: settings.get("802-11-wireless").and_then(ssid_of),
            active,
            path,
        });
    }
    profiles.sort_by(|a, b| {
        b.active
            .is_some()
            .cmp(&a.active.is_some())
            .then_with(|| a.id.to_lowercase().cmp(&b.id.to_lowercase()))
    });
    profiles
}

async fn read_access_points(bus: &Bus, wifi: &Device, snapshot: &Snapshot) -> Vec<Ap> {
    let Ok(proxy) = bus.proxy(&wifi.path, IF_WIRELESS).await else {
        return Vec::new();
    };
    let paths: Vec<OwnedObjectPath> = match proxy.call("GetAllAccessPoints", &()).await {
        Ok(paths) => paths,
        Err(error) => {
            log::debug!("network: GetAllAccessPoints: {error}");
            return Vec::new();
        }
    };
    let active = snapshot
        .primary
        .as_ref()
        .and_then(|active| active.ap.as_ref())
        .map(|link| link.path.clone());

    let mut aps = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path.as_str().to_owned();
        let props = bus.props(&path, IF_AP).await;
        // A hidden SSID has nothing to show and nothing to click.
        let Some(ssid) = ssid_of(&props).filter(|ssid| !ssid.is_empty()) else {
            continue;
        };
        let profile = snapshot
            .profiles
            .iter()
            .find(|profile| profile.ssid.as_deref() == Some(ssid.as_str()))
            .map(|profile| profile.path.clone());
        aps.push(Ap {
            strength: u8_of(&props, "Strength").unwrap_or(0),
            security: security(
                u32_of(&props, "Flags").unwrap_or(0),
                u32_of(&props, "WpaFlags").unwrap_or(0),
                u32_of(&props, "RsnFlags").unwrap_or(0),
            ),
            frequency: u32_of(&props, "Frequency").unwrap_or(0),
            active: active.as_deref() == Some(path.as_str()),
            ssid,
            profile,
            path,
        });
    }
    aps
}

/// NM80211ApFlags / NM80211ApSecurityFlags, narrowed to the label a human
/// needs before clicking.
fn security(flags: u32, wpa: u32, rsn: u32) -> &'static str {
    const PRIVACY: u32 = 0x1;
    const PSK: u32 = 0x100;
    const EAP: u32 = 0x200;
    const SAE: u32 = 0x400;
    const OWE: u32 = 0x800;
    const OWE_TM: u32 = 0x1000;
    const EAP_SUITE_B: u32 = 0x2000;
    if rsn & SAE != 0 {
        "wpa3"
    } else if rsn & EAP_SUITE_B != 0 {
        "wpa3-enterprise"
    } else if (rsn | wpa) & EAP != 0 {
        "802.1x"
    } else if rsn & (OWE | OWE_TM) != 0 {
        "owe"
    } else if rsn & PSK != 0 {
        "wpa2"
    } else if wpa & PSK != 0 {
        "wpa"
    } else if flags & PRIVACY != 0 {
        "wep"
    } else {
        "open"
    }
}

async fn activate(profile: String, device: String, specific: String) -> anyhow::Result<()> {
    let bus = Bus::system().await?;
    let proxy = bus.proxy(NM_PATH, IF_MANAGER).await?;
    let _: OwnedObjectPath = proxy
        .call(
            "ActivateConnection",
            &(objpath(&profile)?, objpath(&device)?, objpath(&specific)?),
        )
        .await
        .with_context(|| format!("activating {profile}"))?;
    Ok(())
}

async fn deactivate(active: String) -> anyhow::Result<()> {
    let bus = Bus::system().await?;
    let proxy = bus.proxy(NM_PATH, IF_MANAGER).await?;
    proxy
        .call::<_, _, ()>("DeactivateConnection", &(objpath(&active)?,))
        .await
        .with_context(|| format!("deactivating {active}"))?;
    Ok(())
}

async fn request_scan(device: &str) -> anyhow::Result<()> {
    let bus = Bus::system().await?;
    let proxy = bus.proxy(device, IF_WIRELESS).await?;
    let options: HashMap<String, Value<'_>> = HashMap::new();
    proxy
        .call::<_, _, ()>("RequestScan", &(options,))
        .await
        .context("requesting a wifi scan")?;
    Ok(())
}

async fn set_wireless(enabled: bool) -> anyhow::Result<()> {
    let bus = Bus::system().await?;
    let proxy = bus.proxy(NM_PATH, IF_PROPERTIES).await?;
    proxy
        .call::<_, _, ()>(
            "Set",
            &(IF_MANAGER, "WirelessEnabled", Value::Bool(enabled)),
        )
        .await
        .context("setting WirelessEnabled")?;
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

fn objpath(path: &str) -> anyhow::Result<ObjectPath<'static>> {
    ObjectPath::try_from(path.to_owned()).with_context(|| format!("bad object path {path}"))
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

fn u32_of(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    match field(props, key)? {
        Value::U32(value) => Some(*value),
        Value::I32(value) => u32::try_from(*value).ok(),
        Value::U16(value) => Some(u32::from(*value)),
        Value::U8(value) => Some(u32::from(*value)),
        Value::U64(value) => u32::try_from(*value).ok(),
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

fn path_of(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match field(props, key)? {
        Value::ObjectPath(value) => Some(value.as_str().to_owned()),
        Value::Str(value) => Some(value.as_str().to_owned()),
        _ => None,
    }
}

fn paths_of(props: &HashMap<String, OwnedValue>, key: &str) -> Vec<String> {
    let Some(Value::Array(array)) = field(props, key) else {
        return Vec::new();
    };
    array
        .inner()
        .iter()
        .filter_map(|value| match peel(value) {
            Value::ObjectPath(path) => Some(path.as_str().to_owned()),
            Value::Str(path) => Some(path.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

/// `ay` SSID, which is bytes and not guaranteed to be UTF-8.
fn ssid_of(props: &HashMap<String, OwnedValue>) -> Option<String> {
    let Value::Array(array) = field(props, "Ssid").or_else(|| field(props, "ssid"))? else {
        return None;
    };
    let bytes: Vec<u8> = array
        .inner()
        .iter()
        .filter_map(|value| match peel(value) {
            Value::U8(byte) => Some(*byte),
            _ => None,
        })
        .collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The `a{sv}` entries of an `aa{sv}` property, flattened to owned pairs so
/// callers do not have to juggle zvariant lifetimes.
fn dicts_of(props: &HashMap<String, OwnedValue>, key: &str) -> Vec<Vec<(String, OwnedValue)>> {
    let Some(Value::Array(array)) = field(props, key) else {
        return Vec::new();
    };
    array
        .inner()
        .iter()
        .filter_map(|value| match peel(value) {
            Value::Dict(dict) => Some(
                dict.iter()
                    .filter_map(|(key, value)| {
                        let key = match peel(key) {
                            Value::Str(key) => key.as_str().to_owned(),
                            _ => return None,
                        };
                        let value = OwnedValue::try_from(peel(value).try_clone().ok()?).ok()?;
                        Some((key, value))
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

fn dict_string(entry: &[(String, OwnedValue)], key: &str) -> Option<String> {
    entry.iter().find(|(name, _)| name == key).and_then(|(_, value)| {
        match peel(value) {
            Value::Str(value) => Some(value.as_str().to_owned()),
            _ => None,
        }
    })
}

fn dict_u32(entry: &[(String, OwnedValue)], key: &str) -> Option<u32> {
    entry.iter().find(|(name, _)| name == key).and_then(|(_, value)| {
        match peel(value) {
            Value::U32(value) => Some(*value),
            Value::I32(value) => u32::try_from(*value).ok(),
            _ => None,
        }
    })
}
