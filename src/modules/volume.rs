//! Volume: the default sink and the default source, straight from libpulse.
//!
//! A dedicated thread owns a threaded mainloop and one context subscribed to
//! sink, source, server and sink-input events, so every change — a media key,
//! pwvucontrol, a bluetooth headset connecting, an app opening a stream —
//! reaches the bar as a push. Nothing here shells out to `pactl`, and volume is
//! written through `Introspector::set_*_volume_by_index`.
//!
//! Waybar parity: `modules/pulseaudio.jsonc` (`{icon} {volume}%` for the sink,
//! `󰍬 {volume}%` for the source, its `format-icons` glyphs, dimmed when muted,
//! scroll to change, right-click to mute, click for a mixer) and
//! `scripts/volume.sh` (steps clamped to 0..=100). The tooltip that only named
//! the device becomes a real mixer: sliders, mute toggles, device switching and
//! per-application streams.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget;

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::{Introspector, SinkInfo, SinkInputInfo, SourceInfo};
use libpulse_binding::context::subscribe::{Facility, InterestMaskSet};
use libpulse_binding::context::{Context, FlagSet, State as ContextState};
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::proplist::{Proplist, properties};
use libpulse_binding::volume::{ChannelVolumes, Volume};

use crate::bar::Message;
use crate::modules::pointer::Pointer;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

/// waybar painted `#pulseaudio` with `@volume`, which mocha maps to mantle.
pub const ISLAND: Island = Island::Start;

/// nf-md-volume_low / _medium / _high: `format-icons.default` in
/// `pulseaudio.jsonc`, picked by level exactly as waybar did.
const SPEAKER: [&str; 3] = ["\u{f057f}", "\u{f0580}", "\u{f057e}"];
/// nf-md-volume_off: `format-icons.default-muted`.
const SPEAKER_MUTED: &str = "\u{f075f}";
/// nf-md-headphones / _off: `format-icons.headphone{,-muted}`.
const HEADPHONE: &str = "\u{f02cb}";
const HEADPHONE_MUTED: &str = "\u{f07ce}";
/// nf-md-headset / _off: `format-icons.headset{,-muted}`.
const HEADSET: &str = "\u{f02ce}";
const HEADSET_MUTED: &str = "\u{f02d0}";
/// nf-md-microphone / _off: `format-source{,-muted}`.
const MIC: &str = "\u{f036c}";
const MIC_MUTED: &str = "\u{f036d}";

/// Step per wheel notch. `volume.sh` defaulted to 1, but its keybinds pass 5.
const STEP: u32 = 5;
/// Ceiling, matching `volume.sh`'s `MAX=100`: no software over-amplification.
const MAX_PCT: u32 = 100;
/// How many daemon snapshots an unreached target survives before the bar goes
/// back to telling the truth. Guards against a device that clamps or ignores
/// the value we asked for.
const TARGET_PATIENCE: u8 = 6;

/// Reconnect ladder for a daemon that is down or restarting, as in `mlqd.rs`.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A session that lasted this long was healthy: the next failure starts the
/// ladder over instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// A context that has not reached `Ready` by then is not going to.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Which mixer object a command or a target refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Sink,
    Source,
    /// One application's stream into a sink.
    Stream,
}

/// What a sink's hardware is, so the bar can pick waybar's headphone and
/// headset glyphs instead of always drawing a speaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Form {
    #[default]
    Speaker,
    Headphone,
    Headset,
}

impl Form {
    /// `device.form_factor` when the driver sets it, else the active port name,
    /// which is what pulseaudio's own icon naming falls back to.
    fn read(proplist: &Proplist, port: Option<&str>, description: &str) -> Self {
        let hint = proplist
            .get_str(properties::DEVICE_FORM_FACTOR)
            .or_else(|| port.map(str::to_owned))
            .unwrap_or_else(|| description.to_owned())
            .to_ascii_lowercase();
        if hint.contains("headset") || hint.contains("hands-free") || hint.contains("handset") {
            Self::Headset
        } else if hint.contains("headphone") || hint.contains("earpiece") {
            Self::Headphone
        } else {
            Self::Speaker
        }
    }

    fn glyph(self, muted: bool, pct: u32) -> &'static str {
        match (self, muted) {
            (Self::Headphone, false) => HEADPHONE,
            (Self::Headphone, true) => HEADPHONE_MUTED,
            (Self::Headset, false) => HEADSET,
            (Self::Headset, true) => HEADSET_MUTED,
            (Self::Speaker, true) => SPEAKER_MUTED,
            // waybar split `default` into thirds by level.
            (Self::Speaker, false) => SPEAKER[level(pct)],
        }
    }
}

/// waybar's `format-icons.default` is a three-element array indexed by level.
fn level(pct: u32) -> usize {
    match pct {
        0..33 => 0,
        33..66 => 1,
        _ => 2,
    }
}

/// A sink or source as the bar needs it: enough to render and to write back.
#[derive(Debug, Clone)]
pub struct Device {
    index: u32,
    name: String,
    description: String,
    pct: u32,
    /// Channel count, so a volume write keeps the device's channel layout.
    channels: u8,
    mute: bool,
    form: Form,
}

/// One application's stream into a sink.
#[derive(Debug, Clone)]
pub struct AppStream {
    index: u32,
    /// `application.name`, falling back to the process binary.
    app: String,
    /// `media.name`: the track or tab title, when the app sets one.
    title: Option<String>,
    pct: u32,
    channels: u8,
    mute: bool,
    /// False for streams whose volume the server will not let us set.
    writable: bool,
    corked: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Which sink and source are the defaults right now.
    Server {
        sink: Option<String>,
        source: Option<String>,
    },
    Sinks(Arc<Vec<Device>>),
    Sources(Arc<Vec<Device>>),
    Streams(Arc<Vec<AppStream>>),
    /// The context is gone; the module hides until it comes back.
    Gone,
    /// Wheel over the bar cell, in notches (positive raises).
    Scroll(f32),
    SetVolume(Kind, u32, u32),
    ToggleMute(Kind, u32),
    /// Mute the default sink, whatever it is right now: the bar cell speaks for
    /// the default, so a right-click on it cannot name an index.
    MuteDefault,
    SetDefault(Kind, String),
    /// A command reached the pulse thread; the daemon's own event carries the
    /// resulting state, so there is nothing to apply here.
    Sent,
    /// A command could not be handed over at all.
    Failed(String),
}

#[derive(Debug, Default)]
pub struct State {
    connected: bool,
    default_sink: Option<String>,
    default_source: Option<String>,
    sinks: Arc<Vec<Device>>,
    sources: Arc<Vec<Device>>,
    streams: Arc<Vec<AppStream>>,
    /// Volume the user just asked for, per (kind, index), with the number of
    /// snapshots it may still wait for. Scrolling three notches inside one
    /// frame has to add 15%, not 5% three times, so the next step is computed
    /// from the target rather than from the last snapshot.
    targets: HashMap<(Kind, u32), (u32, u8)>,
    /// Fractional wheel travel not yet worth a step.
    wheel: f32,
    error: Option<String>,
}

impl State {
    /// One pulse connection, popup or not: the bar's own text needs sink and
    /// source state, and the same subscription already carries the sink-input
    /// detail the popup mixes, so there is nothing left to gate on `open`.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::run(events).map(event_message)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Server { sink, source } => {
                if self.default_sink != sink {
                    self.targets.retain(|&(kind, _), _| kind != Kind::Sink);
                }
                if self.default_source != source {
                    self.targets.retain(|&(kind, _), _| kind != Kind::Source);
                }
                self.default_sink = sink;
                self.default_source = source;
                self.connected = true;
                Task::none()
            }
            Event::Sinks(sinks) => {
                self.sinks = sinks;
                self.connected = true;
                self.settle(Kind::Sink);
                Task::none()
            }
            Event::Sources(sources) => {
                self.sources = sources;
                self.connected = true;
                self.settle(Kind::Source);
                Task::none()
            }
            Event::Streams(streams) => {
                self.streams = streams;
                self.connected = true;
                self.settle(Kind::Stream);
                Task::none()
            }
            Event::Gone => {
                self.connected = false;
                self.targets.clear();
                Task::none()
            }
            Event::Scroll(notches) => {
                self.wheel += notches;
                let steps = self.wheel.trunc();
                if steps == 0.0 {
                    return Task::none();
                }
                self.wheel -= steps;
                let Some(sink) = self.sink() else {
                    return Task::none();
                };
                let (index, channels) = (sink.index, sink.channels);
                let current = self.target(Kind::Sink, index).unwrap_or(sink.pct);
                let wanted =
                    (current as f32 + steps * STEP as f32).clamp(0.0, MAX_PCT as f32) as u32;
                if wanted == current {
                    return Task::none();
                }
                self.aim(Kind::Sink, index, wanted);
                send(Command::Volume {
                    kind: Kind::Sink,
                    index,
                    channels,
                    pct: wanted,
                })
            }
            Event::SetVolume(kind, index, pct) => {
                let pct = pct.min(MAX_PCT);
                let Some(channels) = self.channels(kind, index) else {
                    return Task::none();
                };
                self.aim(kind, index, pct);
                send(Command::Volume {
                    kind,
                    index,
                    channels,
                    pct,
                })
            }
            Event::ToggleMute(kind, index) => {
                let Some(mute) = self.muted(kind, index) else {
                    return Task::none();
                };
                send(Command::Mute {
                    kind,
                    index,
                    mute: !mute,
                })
            }
            Event::MuteDefault => match self.sink() {
                Some(sink) => send(Command::Mute {
                    kind: Kind::Sink,
                    index: sink.index,
                    mute: !sink.mute,
                }),
                None => Task::none(),
            },
            Event::SetDefault(kind, name) => send(Command::Default { kind, name }),
            Event::Sent => {
                self.error = None;
                Task::none()
            }
            Event::Failed(error) => {
                self.error = Some(error);
                Task::none()
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if !self.connected {
            return None;
        }
        let sink = self.sink()?;
        let palette = ctx.palette;
        let mut row = widget::Row::new().spacing(8).align_y(Alignment::Center);

        // waybar's `group/pulseaudio` drew the microphone first.
        if let Some(source) = self.source() {
            let pct = self.shown(Kind::Source, source.index, source.pct);
            row = row.push(
                crate::theme::label(
                    if source.mute { MIC_MUTED } else { MIC },
                    format!("{pct}%"),
                    ctx.font_size,
                    // `.source-muted` dimmed the mic to `@hover-fg`.
                    cosmic::theme::Text::Color(if source.mute {
                        palette.overlay0
                    } else {
                        palette.fg()
                    }),
                ),
            );
        }

        let pct = self.shown(Kind::Sink, sink.index, sink.pct);
        row = row.push(
            crate::theme::label(
                sink.form.glyph(sink.mute, pct),
                format!("{pct}%"),
                ctx.font_size,
                // A muted output is the state worth noticing, so it goes red
                // rather than taking waybar's flat dimming.
                cosmic::theme::Text::Color(if sink.mute {
                    palette.red
                } else {
                    palette.fg()
                }),
            ),
        );

        // Right-click (mute) is the bar's own, from `modules::right_click`, so
        // every cell answers the same button the same way.
        Some(Pointer::new(row.into()).on_wheel(wheel).wrap())
    }

    /// Nothing here changes on its own between events.
    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.connected
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if !self.connected {
            return None;
        }
        let palette = ctx.palette;
        let mut card = Card::new();

        // The output is what the module is about, so its device names the card
        // and its level is the headline the card reports.
        if let Some(sink) = self.sink() {
            let pct = self.shown(Kind::Sink, sink.index, sink.pct);
            card = card
                .block(popup::split(
                    popup::title(elide(&sink.description, 30), ctx),
                    [popup::title(format!("{pct}%"), ctx)
                        .class(cosmic::theme::Text::Color(palette.accent()))
                        .into()],
                ))
                .block(
                    popup::column()
                        .push(popup::section("output", ctx))
                        .push(self.slider_row(ctx, Kind::Sink, sink.index, pct, sink.mute, true)),
                );
            if self.sinks.len() > 1 {
                card = card.block(switch_block(ctx, Kind::Sink, sink, &self.sinks));
            }
        }

        // The input names its own device: the card's title is already spoken
        // for, and a microphone the bar is about to unmute is worth naming.
        if let Some(source) = self.source() {
            let pct = self.shown(Kind::Source, source.index, source.pct);
            card = card.block(
                popup::column()
                    .push(popup::split(
                        popup::section("input", ctx),
                        [popup::detail(format!("{pct}%"), ctx)
                            .class(cosmic::theme::Text::Color(palette.accent()))
                            .into()],
                    ))
                    .push(
                        popup::lines()
                            .push(popup::item(elide(&source.description, 30), ctx))
                            .push(self.slider_row(
                                ctx,
                                Kind::Source,
                                source.index,
                                pct,
                                source.mute,
                                true,
                            )),
                    ),
            );
            if self.sources.len() > 1 {
                card = card.block(switch_block(ctx, Kind::Source, source, &self.sources));
            }
        }

        let live: Vec<&AppStream> = self.streams.iter().filter(|s| !s.corked).collect();
        if !live.is_empty() {
            let mut list = popup::column().push(popup::section("playing", ctx));
            for stream in live {
                let label = match &stream.title {
                    Some(title) => format!("{} · {}", stream.app, elide(title, 30)),
                    None => stream.app.clone(),
                };
                let meter = self.slider_row(
                    ctx,
                    Kind::Stream,
                    stream.index,
                    self.shown(Kind::Stream, stream.index, stream.pct),
                    stream.mute,
                    stream.writable,
                );
                list = list.push(popup::lines().push(popup::item(label, ctx)).push(meter));
            }
            // How many applications are playing is the session's business, not
            // the bar's, so this is the block that scrolls.
            card = card.list(list);
        }

        Some(
            card.maybe(self.error.as_ref().map(|error| {
                popup::detail(error.as_str(), ctx).class(cosmic::theme::Text::Color(palette.red))
            }))
            .build(),
        )
    }

    fn slider_row<'a>(
        &self,
        ctx: &Ctx,
        kind: Kind,
        index: u32,
        pct: u32,
        mute: bool,
        writable: bool,
    ) -> Element<'a, Message> {
        let meter: Element<'a, Message> = if writable {
            widget::slider(0..=MAX_PCT, pct, move |pct| {
                event_message(Event::SetVolume(kind, index, pct))
            })
            .step(1u32)
            .width(Length::Fill)
            .into()
        } else {
            popup::detail("fixed volume", ctx).into()
        };
        popup::split(
            meter,
            [popup::icon_chip(
                match (kind, mute) {
                    (Kind::Source, true) => MIC_MUTED,
                    (Kind::Source, false) => MIC,
                    (_, true) => SPEAKER_MUTED,
                    (_, false) => SPEAKER[2],
                },
                Chip::Plain,
                ctx,
                Some(event_message(Event::ToggleMute(kind, index))),
            )],
        )
        .into()
    }

    fn sink(&self) -> Option<&Device> {
        find(&self.sinks, self.default_sink.as_deref())
    }

    fn source(&self) -> Option<&Device> {
        find(&self.sources, self.default_source.as_deref())
    }

    /// The percentage to render: the pending target while one is outstanding,
    /// so scrolling and dragging feel immediate.
    fn shown(&self, kind: Kind, index: u32, actual: u32) -> u32 {
        self.target(kind, index).unwrap_or(actual)
    }

    fn target(&self, kind: Kind, index: u32) -> Option<u32> {
        self.targets.get(&(kind, index)).map(|&(pct, _)| pct)
    }

    fn aim(&mut self, kind: Kind, index: u32, pct: u32) {
        self.targets.insert((kind, index), (pct, TARGET_PATIENCE));
    }

    /// Drop targets the daemon has caught up with, and give up on the ones it
    /// keeps refusing rather than lying in the bar forever.
    fn settle(&mut self, kind: Kind) {
        let State {
            targets,
            sinks,
            sources,
            streams,
            ..
        } = self;
        targets.retain(|&(target_kind, index), (pct, patience)| {
            if target_kind != kind {
                return true;
            }
            let actual = match target_kind {
                Kind::Sink => at(sinks, index).map(|device| device.pct),
                Kind::Source => at(sources, index).map(|device| device.pct),
                Kind::Stream => stream_at(streams, index).map(|stream| stream.pct),
            };
            let Some(actual) = actual else {
                // The object is gone; so is any target for it.
                return false;
            };
            if actual.abs_diff(*pct) <= 1 {
                return false;
            }
            *patience = patience.saturating_sub(1);
            *patience > 0
        });
    }

    fn channels(&self, kind: Kind, index: u32) -> Option<u8> {
        match kind {
            Kind::Sink => at(&self.sinks, index).map(|device| device.channels),
            Kind::Source => at(&self.sources, index).map(|device| device.channels),
            Kind::Stream => stream_at(&self.streams, index).map(|stream| stream.channels),
        }
    }

    fn muted(&self, kind: Kind, index: u32) -> Option<bool> {
        match kind {
            Kind::Sink => at(&self.sinks, index).map(|device| device.mute),
            Kind::Source => at(&self.sources, index).map(|device| device.mute),
            Kind::Stream => stream_at(&self.streams, index).map(|stream| stream.mute),
        }
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Volume(event))
}

/// Wheel handler, as a plain `fn` so [`Pointer`] never has to box a closure.
fn wheel(notches: f32) -> Message {
    event_message(Event::Scroll(notches))
}

fn find<'a>(devices: &'a [Device], name: Option<&str>) -> Option<&'a Device> {
    let name = name?;
    devices.iter().find(|device| device.name == name)
}

fn at(devices: &[Device], index: u32) -> Option<&Device> {
    devices.iter().find(|device| device.index == index)
}

fn stream_at(streams: &[AppStream], index: u32) -> Option<&AppStream> {
    streams.iter().find(|stream| stream.index == index)
}

/// The devices of one kind the bar could point at instead. They are a menu, so
/// they are rows: a line that answers a click has to say so before the click.
fn switch_block<'a>(
    ctx: &Ctx,
    kind: Kind,
    current: &'a Device,
    all: &'a [Device],
) -> Element<'a, Message> {
    let mut block = popup::column().push(popup::section("switch to", ctx));
    for device in all.iter().filter(|device| device.index != current.index) {
        block = block.push(popup::row(
            popup::split(
                popup::item(elide(&device.description, 32), ctx),
                [popup::detail(format!("{}%", device.pct), ctx).into()],
            ),
            ctx.palette,
            Some(event_message(Event::SetDefault(kind, device.name.clone()))),
        ));
    }
    block.into()
}

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/*----------------------------------------------------------------------------
    the pulse thread
----------------------------------------------------------------------------*/

/// What the bar asks the pulse thread to do.
#[derive(Debug)]
enum Command {
    /// A subscription event: re-read this facility.
    Dirty(Option<Facility>),
    /// Re-read everything, for a fresh listener.
    Resync,
    /// The context changed state; the worker re-checks it.
    State,
    Volume {
        kind: Kind,
        index: u32,
        channels: u8,
        pct: u32,
    },
    Mute {
        kind: Kind,
        index: u32,
        mute: bool,
    },
    Default {
        kind: Kind,
        name: String,
    },
}

impl Command {
    /// Commands with the same key supersede each other: a flick of the wheel
    /// must reach the daemon as one volume write, not one per notch.
    fn key(&self) -> Option<(u8, Kind, u32)> {
        match self {
            Self::Volume { kind, index, .. } => Some((0, *kind, *index)),
            Self::Mute { kind, index, .. } => Some((1, *kind, *index)),
            Self::Default { kind, .. } => Some((2, *kind, 0)),
            // Refresh requests fold into a facility set instead.
            Self::Dirty(_) | Self::Resync | Self::State => None,
        }
    }
}

/// The pulse thread, started on first use and kept for the life of the process.
static PULSE: LazyLock<Pulse> = LazyLock::new(Pulse::start);

struct Pulse {
    /// `Sender` is not `Sync`, so the shared handle sits behind a mutex that is
    /// only ever held for the duration of one send.
    commands: Mutex<Sender<Command>>,
    /// Where snapshots go. Replaced whenever the bar restarts the subscription,
    /// which is why the thread outlives any single stream.
    listener: Mutex<Option<UnboundedSender<Event>>>,
}

impl Pulse {
    fn start() -> Self {
        let (commands, requests) = mpsc::channel();
        let waker = commands.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("cosmicbar-pulse".into())
            .spawn(move || worker(&requests, &waker))
        {
            log::error!("pulse thread: {error}");
        }
        Self {
            commands: Mutex::new(commands),
            listener: Mutex::new(None),
        }
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .lock()
            .map_err(|_| "pulse thread poisoned".to_owned())?
            .send(command)
            .map_err(|_| "pulse thread gone".to_owned())
    }

    /// Called from the pulse thread and from its introspection callbacks.
    fn emit(&self, event: Event) {
        if let Ok(listener) = self.listener.lock()
            && let Some(sender) = listener.as_ref()
        {
            let _ = sender.unbounded_send(event);
        }
    }

    /// Point the thread at a fresh receiver and ask for a full snapshot.
    fn listen(&self) -> UnboundedReceiver<Event> {
        let (sender, receiver) = unbounded();
        if let Ok(mut listener) = self.listener.lock() {
            *listener = Some(sender);
        }
        let _ = self.send(Command::Resync);
        receiver
    }
}

/// Hand a command to the pulse thread from `update`, off the render path.
fn send(command: Command) -> Task<Message> {
    Task::future(async move {
        cosmic::Action::App(event_message(match PULSE.send(command) {
            Ok(()) => Event::Sent,
            Err(error) => Event::Failed(error),
        }))
    })
}

fn events() -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(8, async move |mut sender| {
        let mut events = PULSE.listen();
        while let Some(event) = events.next().await {
            if sender.send(event).await.is_err() {
                return;
            }
        }
    })
}

/// Owns the mainloop and context for the life of the process, reconnecting on
/// its own so a `systemctl --user restart pipewire` needs no bar restart.
fn worker(commands: &Receiver<Command>, waker: &Sender<Command>) {
    let mut attempt = 0usize;
    loop {
        let started = Instant::now();
        if let Err(error) = session(commands, waker) {
            log::debug!("pulse session ended: {error}");
        }
        PULSE.emit(Event::Gone);
        if started.elapsed() >= STABLE_SESSION {
            attempt = 0;
        }
        let delay = Duration::from_secs(RECONNECT_BACKOFF_SECS[attempt.min(4)]);
        attempt = attempt.saturating_add(1);
        // Blocking drain: commands issued against a dead context are dropped.
        let deadline = Instant::now() + delay;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match commands.recv_timeout(left) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// One connection's worth of events. Returns when the context is lost.
fn session(commands: &Receiver<Command>, waker: &Sender<Command>) -> Result<(), String> {
    let mut mainloop = Mainloop::new().ok_or("no pulse mainloop")?;
    let mut proplist = Proplist::new().ok_or("no pulse proplist")?;
    let _ = proplist.set_str(properties::APPLICATION_NAME, "cosmicbar");
    let _ = proplist.set_str(properties::APPLICATION_ID, "dev.cosmicbar.Bar");
    let mut context =
        Context::new_with_proplist(&mainloop, "cosmicbar", &proplist).ok_or("no pulse context")?;

    // Every state change wakes this thread through the command channel, so
    // there is no need for `Mainloop::wait`/`signal`, which would mean sharing
    // the mainloop with a callback running on the mainloop's own thread.
    let state_waker = waker.clone();
    context.set_state_callback(Some(Box::new(move || {
        let _ = state_waker.send(Command::State);
    })));
    context
        .connect(None, FlagSet::NOFLAGS, None)
        .map_err(|error| format!("connect: {error}"))?;
    mainloop
        .start()
        .map_err(|error| format!("mainloop: {error}"))?;

    let outcome = run(&mut mainloop, &mut context, commands, waker);

    context.set_state_callback(None);
    context.set_subscribe_callback(None);
    context.disconnect();
    mainloop.stop();
    outcome
}

fn run(
    mainloop: &mut Mainloop,
    context: &mut Context,
    commands: &Receiver<Command>,
    waker: &Sender<Command>,
) -> Result<(), String> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        mainloop.lock();
        let state = context.get_state();
        mainloop.unlock();
        match state {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => return Err("context failed".into()),
            _ => {}
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or("context never became ready")?;
        match commands.recv_timeout(left) {
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => return Err("context never became ready".into()),
            Err(RecvTimeoutError::Disconnected) => return Err("bar gone".into()),
        }
    }

    let dirty_waker = waker.clone();
    mainloop.lock();
    context.set_subscribe_callback(Some(Box::new(move |facility, _operation, _index| {
        let _ = dirty_waker.send(Command::Dirty(facility));
    })));
    // SERVER carries default-device changes; SINK_INPUT the per-application
    // streams the popup mixes.
    let _ = context.subscribe(
        InterestMaskSet::SINK
            | InterestMaskSet::SOURCE
            | InterestMaskSet::SERVER
            | InterestMaskSet::SINK_INPUT,
        |_success| {},
    );
    refresh(&context.introspect(), Refresh::ALL);
    mainloop.unlock();

    let mut batch: Vec<Command> = Vec::new();
    loop {
        match commands.recv() {
            Ok(command) => batch.push(command),
            Err(_) => return Err("bar gone".into()),
        }
        while let Ok(command) = commands.try_recv() {
            batch.push(command);
        }
        coalesce(&mut batch);

        mainloop.lock();
        if context.get_state() != ContextState::Ready {
            mainloop.unlock();
            return Err("context lost".into());
        }
        let mut wanted = Refresh::NONE;
        for command in batch.drain(..) {
            match command {
                Command::State => {}
                Command::Dirty(facility) => wanted |= Refresh::of(facility),
                Command::Resync => wanted |= Refresh::ALL,
                Command::Volume {
                    kind,
                    index,
                    channels,
                    pct,
                } => {
                    let mut volumes = ChannelVolumes::default();
                    volumes.set(channels.max(1), from_pct(pct));
                    let mut introspect = context.introspect();
                    match kind {
                        Kind::Sink => {
                            introspect.set_sink_volume_by_index(index, &volumes, None);
                        }
                        Kind::Source => {
                            introspect.set_source_volume_by_index(index, &volumes, None);
                        }
                        Kind::Stream => {
                            introspect.set_sink_input_volume(index, &volumes, None);
                        }
                    }
                }
                Command::Mute { kind, index, mute } => {
                    let mut introspect = context.introspect();
                    match kind {
                        Kind::Sink => {
                            introspect.set_sink_mute_by_index(index, mute, None);
                        }
                        Kind::Source => {
                            introspect.set_source_mute_by_index(index, mute, None);
                        }
                        Kind::Stream => {
                            introspect.set_sink_input_mute(index, mute, None);
                        }
                    }
                }
                Command::Default { kind, name } => match kind {
                    Kind::Source => {
                        context.set_default_source(&name, |_ok| {});
                    }
                    _ => {
                        context.set_default_sink(&name, |_ok| {});
                    }
                },
            }
        }
        if wanted != Refresh::NONE {
            refresh(&context.introspect(), wanted);
        }
        mainloop.unlock();
    }
}

/// Keep only the last command per key, in place, preserving order.
fn coalesce(batch: &mut Vec<Command>) {
    if batch.len() < 2 {
        return;
    }
    let mut seen: Vec<(u8, Kind, u32)> = Vec::new();
    let mut keep = vec![true; batch.len()];
    for (position, command) in batch.iter().enumerate().rev() {
        if let Some(key) = command.key() {
            if seen.contains(&key) {
                keep[position] = false;
            } else {
                seen.push(key);
            }
        }
    }
    let mut position = 0;
    batch.retain(|_| {
        let kept = keep[position];
        position += 1;
        kept
    });
}

/// Which facilities a refresh should re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refresh(u8);

impl Refresh {
    const NONE: Self = Self(0);
    const SERVER: Self = Self(1);
    const SINKS: Self = Self(2);
    const SOURCES: Self = Self(4);
    const STREAMS: Self = Self(8);
    const ALL: Self = Self(15);

    /// A server change can move the default device, so it also re-reads both
    /// device lists; that is one extra round trip on a rare event.
    fn of(facility: Option<Facility>) -> Self {
        match facility {
            Some(Facility::Sink) => Self::SINKS,
            Some(Facility::Source) => Self::SOURCES,
            Some(Facility::SinkInput) => Self::STREAMS,
            Some(Facility::Server) => Self(Self::SERVER.0 | Self::SINKS.0 | Self::SOURCES.0),
            _ => Self::NONE,
        }
    }

    fn has(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOrAssign for Refresh {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// Issue the introspection queries for `wanted`. Each result callback owns its
/// own accumulator and emits one snapshot when the list ends.
fn refresh(introspect: &Introspector, wanted: Refresh) {
    if wanted.has(Refresh::SERVER) {
        let _ = introspect.get_server_info(|info| {
            PULSE.emit(Event::Server {
                sink: info.default_sink_name.as_deref().map(str::to_owned),
                source: info.default_source_name.as_deref().map(str::to_owned),
            });
        });
    }
    if wanted.has(Refresh::SINKS) {
        let mut devices = Vec::new();
        let _ = introspect.get_sink_info_list(move |result| match result {
            ListResult::Item(info) => devices.push(sink_device(info)),
            ListResult::End => PULSE.emit(Event::Sinks(Arc::new(std::mem::take(&mut devices)))),
            ListResult::Error => devices.clear(),
        });
    }
    if wanted.has(Refresh::SOURCES) {
        let mut devices = Vec::new();
        let _ = introspect.get_source_info_list(move |result| match result {
            // A monitor source is a sink's loopback, never something you would
            // pick as a microphone.
            ListResult::Item(info) if info.monitor_of_sink.is_none() => {
                devices.push(source_device(info));
            }
            ListResult::Item(_) => {}
            ListResult::End => PULSE.emit(Event::Sources(Arc::new(std::mem::take(&mut devices)))),
            ListResult::Error => devices.clear(),
        });
    }
    if wanted.has(Refresh::STREAMS) {
        let mut streams = Vec::new();
        let _ = introspect.get_sink_input_info_list(move |result| match result {
            ListResult::Item(info) => streams.push(app_stream(info)),
            ListResult::End => PULSE.emit(Event::Streams(Arc::new(std::mem::take(&mut streams)))),
            ListResult::Error => streams.clear(),
        });
    }
}

fn sink_device(info: &SinkInfo) -> Device {
    let name = info.name.as_deref().unwrap_or_default().to_owned();
    let description = describe(info.description.as_deref(), &name);
    Device {
        index: info.index,
        form: Form::read(
            &info.proplist,
            info.active_port
                .as_deref()
                .and_then(|port| port.name.as_deref()),
            &description,
        ),
        name,
        description,
        pct: to_pct(info.volume.avg()),
        channels: info.volume.len(),
        mute: info.mute,
    }
}

fn source_device(info: &SourceInfo) -> Device {
    let name = info.name.as_deref().unwrap_or_default().to_owned();
    Device {
        index: info.index,
        description: describe(info.description.as_deref(), &name),
        name,
        pct: to_pct(info.volume.avg()),
        channels: info.volume.len(),
        mute: info.mute,
        // A source's glyph is always the microphone.
        form: Form::Speaker,
    }
}

fn app_stream(info: &SinkInputInfo) -> AppStream {
    let app = info
        .proplist
        .get_str(properties::APPLICATION_NAME)
        .or_else(|| {
            info.proplist
                .get_str(properties::APPLICATION_PROCESS_BINARY)
        })
        .or_else(|| info.name.as_deref().map(str::to_owned))
        .unwrap_or_else(|| format!("stream {}", info.index));
    let title = info
        .proplist
        .get_str(properties::MEDIA_NAME)
        .filter(|title| !title.is_empty() && *title != app);
    AppStream {
        index: info.index,
        app,
        title,
        pct: to_pct(info.volume.avg()),
        channels: info.volume.len(),
        mute: info.mute,
        writable: info.has_volume && info.volume_writable,
        corked: info.corked,
    }
}

fn describe(description: Option<&str>, name: &str) -> String {
    description
        .filter(|description| !description.is_empty())
        .unwrap_or(name)
        .to_owned()
}

/// Pulse volumes are a linear scale where `NORMAL` is 100%. Both conversions
/// round, so a percentage written through the bar reads back unchanged.
fn to_pct(volume: Volume) -> u32 {
    let normal = Volume::NORMAL.0 as u64;
    ((volume.0 as u64 * 100 + normal / 2) / normal) as u32
}

fn from_pct(pct: u32) -> Volume {
    let normal = Volume::NORMAL.0 as u64;
    Volume(((pct.min(MAX_PCT) as u64 * normal + 50) / 100) as u32)
}
