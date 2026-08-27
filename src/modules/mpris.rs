//! MPRIS2: what is playing, and a transport that actually works.
//!
//! Every player on the session bus is discovered once with `ListNames` and then
//! tracked by signal: `NameOwnerChanged` for players coming and going, one
//! `PropertiesChanged` match rule for all of their state, and one `Seeked` match
//! rule for jumps. Two match rules cover any number of players, so adding a
//! player costs one `GetAll` round trip and nothing recurring.
//!
//! `Position` is the one property MPRIS explicitly does not emit changes for, so
//! the bar advances it from the wall clock between samples and re-reads it only
//! when something happened (status change, seek) or while the popup is open.
//!
//! Waybar parity: `modules/mpris.jsonc` drew `{player_icon} {player}` with a
//! `{title} - {artist}` tooltip, and dimmed `#mpris.paused`. Here the icon and
//! the dimming stay, the bar shows the track instead of the player's name, and
//! the tooltip becomes a transport: seek bar, previous / play-pause / next /
//! stop, shuffle and loop when the player advertises them, and a picker when
//! more than one player is running.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::channel::mpsc::{UnboundedSender, unbounded};
use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
use cosmic::iced::{Length, Subscription};
use cosmic::widget;

use zbus::message::Message as BusMessage;
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

/// waybar gave `#mpris` padding only, so it sits on the bar background.
pub const ISLAND: Island = Island::Flat;

const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
/// Every MPRIS bus name starts with this.
const NAME_PREFIX: &str = "org.mpris.MediaPlayer2.";
/// `playerctld` is a proxy that re-exports whichever player is active, so
/// tracking it would list the same track twice and make the picker lie.
const PROXY_NAMES: [&str; 1] = ["org.mpris.MediaPlayer2.playerctld"];

/// A real player, as opposed to a proxy or an unrelated bus name.
fn is_player(name: &str) -> bool {
    name.starts_with(NAME_PREFIX) && !PROXY_NAMES.contains(&name)
}

/// nf-md-play: `player-icons.default` in `mpris.jsonc`.
const PLAY: &str = "\u{f040a}";
/// nf-md-pause: `status-icons.paused`.
const PAUSE: &str = "\u{f03e4}";
/// nf-md-stop.
const STOP: &str = "\u{f04db}";
/// nf-md-skip_previous / _next.
const PREVIOUS: &str = "\u{f04ae}";
const NEXT: &str = "\u{f04ad}";
/// nf-md-shuffle / _disabled.
const SHUFFLE: &str = "\u{f049d}";
const SHUFFLE_OFF: &str = "\u{f049e}";
/// nf-md-repeat / _off / _once.
const REPEAT: &str = "\u{f0456}";
const REPEAT_OFF: &str = "\u{f0457}";
const REPEAT_ONCE: &str = "\u{f0458}";
/// nf-md-music: stands in for a player with no metadata yet.
const MUSIC: &str = "\u{f075a}";

/// Longest bar label, glyph excluded. waybar allowed 1000 characters, which
/// would push every island off the bar.
const LABEL_LIMIT: usize = 45;
/// Reconnect ladder for a session bus that is not there yet, as in `mlqd.rs`.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A session that lasted this long was healthy.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// How often the popup re-reads `Position` from the player, so a drifting or
/// externally seeked stream still lines up with the seek bar.
const RESYNC: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl Status {
    fn parse(text: &str) -> Self {
        match text {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

/// One player's state, as far as the bar cares.
#[derive(Debug, Clone, Default)]
pub struct Player {
    /// Well-known bus name, the stable identity used for selection.
    bus: String,
    /// `org.mpris.MediaPlayer2.Identity`, else the bus name's last component.
    identity: String,
    status: Status,
    title: String,
    artist: String,
    album: String,
    length_us: i64,
    /// `Position` as last read, and the wall clock when it was read.
    position_us: i64,
    sampled_ms: i64,
    rate: f64,
    can_next: bool,
    can_previous: bool,
    can_pause: bool,
    can_play: bool,
    can_seek: bool,
    /// `None` when the player does not implement the optional property.
    shuffle: Option<bool>,
    loop_status: Option<String>,
    /// `mpris:trackid`, which `SetPosition` needs.
    track: Option<String>,
}

impl Player {
    /// Where the stream is now: `Position` plus the time since it was sampled,
    /// because MPRIS never signals `Position` changes.
    fn elapsed_us(&self, now_ms: i64) -> i64 {
        if self.status != Status::Playing {
            return self.position_us.max(0);
        }
        let since_ms = (now_ms - self.sampled_ms).max(0) as f64;
        let advanced = (since_ms * 1000.0 * self.rate.max(0.0)) as i64;
        let position = self.position_us.max(0).saturating_add(advanced);
        if self.length_us > 0 {
            position.min(self.length_us)
        } else {
            position
        }
    }

    fn glyph(&self) -> &'static str {
        match self.status {
            Status::Playing => PLAY,
            Status::Paused => PAUSE,
            Status::Stopped => MUSIC,
        }
    }
}

/// What the popup asks a player to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Next,
    Previous,
    PlayPause,
    Stop,
    /// Absolute position, in microseconds.
    Seek(i64),
    Shuffle(bool),
    /// One of MPRIS's `None`, `Track`, `Playlist`.
    Loop(&'static str),
    /// Re-read `Position`; the popup's seek bar wants the player's own number.
    Resync,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The full player set, sorted by bus name so the bar does not reshuffle.
    Players(Arc<Vec<Player>>),
    /// No session bus, or the connection dropped.
    Gone,
    /// The user picked a player in the popup.
    Select(String),
    /// Dragging the seek bar. Held locally so a drag is not one `SetPosition`
    /// per pixel; the player is told once, on release.
    Scrub(u32),
    /// Seek bar released: commit wherever it was left.
    ScrubEnd,
    /// Run `action` against the currently chosen player.
    Dispatch(Action),
    /// Result of a dispatch, so a refusal is visible instead of silent.
    Acted(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    players: Arc<Vec<Player>>,
    /// Bus name the user pinned in the popup; cleared when it disappears.
    selected: Option<String>,
    /// Seek target, in seconds, while the bar is being dragged.
    scrub: Option<u32>,
    error: Option<String>,
}

impl State {
    /// The player set is one bus connection with two match rules — cheap enough
    /// to keep always. The per-second `Position` re-read is not, so it runs only
    /// while the popup is on screen and something is actually playing.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        let live = Subscription::run(events).map(event_message);
        match open && self.current().is_some_and(|player| player.status == Status::Playing) {
            true => Subscription::batch([
                live,
                cosmic::iced::time::every(RESYNC)
                    .map(|_| event_message(Event::Dispatch(Action::Resync))),
            ]),
            false => live,
        }
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Players(players) => {
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|bus| !players.iter().any(|player| &player.bus == bus))
                {
                    self.selected = None;
                }
                self.players = players;
                Task::none()
            }
            Event::Gone => {
                self.players = Arc::default();
                self.selected = None;
                self.scrub = None;
                Task::none()
            }
            Event::Select(bus) => {
                self.selected = Some(bus);
                self.scrub = None;
                Task::none()
            }
            Event::Scrub(seconds) => {
                self.scrub = Some(seconds);
                Task::none()
            }
            Event::ScrubEnd => match self.scrub.take() {
                // The track can change under a drag; a target past the end of
                // the new one would be refused or land somewhere absurd.
                Some(seconds) => {
                    let length = self.current().map_or(0, |player| player.length_us);
                    let target = (i64::from(seconds) * 1_000_000).clamp(0, length);
                    self.update(Event::Dispatch(Action::Seek(target)))
                }
                None => Task::none(),
            },
            Event::Dispatch(action) => {
                let Some(bus) = self.current().map(|player| player.bus.clone()) else {
                    return Task::none();
                };
                Task::future(async move {
                    cosmic::Action::App(event_message(Event::Acted(dispatch(bus, action).await)))
                })
            }
            Event::Acted(result) => {
                self.error = result.err();
                Task::none()
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let player = self.current()?;
        // The player, never the track: this sits on screen while screensharing.
        // waybar's format was `{player_icon} {player}` for the same reason. The
        // title lives in the popup, which only opens on a click.
        Some(crate::theme::label(
            player.glyph(),
            elide(&player.identity, LABEL_LIMIT),
            ctx.font_size,
            // waybar's `#mpris.paused` dimmed to `@hover-fg`.
            cosmic::theme::Text::Color(match player.status {
                Status::Playing => ctx.palette.fg(),
                _ => ctx.palette.overlay0,
            }),
        ))
    }

    /// The elapsed time and the seek bar live in the popup, and the bar cell
    /// only carries the player's name: nothing on the bar changes each second,
    /// so the fast clock is worth asking for only while the popup is open.
    pub fn fast_tick(&self, open: bool) -> bool {
        open && self
            .players
            .iter()
            .any(|player| player.status == Status::Playing)
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.current().is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let player = self.current()?;
        let palette = ctx.palette;
        let mut card = Card::new();

        // Which player everything below is about has to be settled before any
        // of it means anything, so the picker sits above the track rather than
        // beside it.
        if self.players.len() > 1 {
            let mut picker = widget::Row::new().spacing(popup::ROW_GAP);
            for other in self.players.iter() {
                picker = picker.push(popup::chip(
                    elide(&other.identity, 14),
                    match other.bus == player.bus {
                        true => Chip::Accent,
                        false => Chip::Plain,
                    },
                    ctx,
                    Some(event_message(Event::Select(other.bus.clone()))),
                ));
            }
            card = card.block(picker);
        }

        let mut track = popup::lines().push(popup::title(
            match player.title.is_empty() {
                true => player.identity.as_str(),
                false => player.title.as_str(),
            },
            ctx,
        ));
        if !player.artist.is_empty() {
            track = track.push(popup::detail(player.artist.as_str(), ctx));
        }
        if !player.album.is_empty() {
            track = track.push(popup::detail(player.album.as_str(), ctx));
        }
        card = card.block(track);

        let played = player.elapsed_us(ctx.now_ms);
        if player.length_us > 0 {
            let total = (player.length_us / 1_000_000).max(1) as u32;
            // While dragging, the bar and the clock follow the finger, not the
            // player: the seek is only sent on release.
            let at = match self.scrub {
                Some(seconds) => seconds.min(total),
                None => (played / 1_000_000).clamp(0, total as i64) as u32,
            };
            let seek: Element<'_, Message> = if player.can_seek {
                widget::slider(0..=total, at, |seconds| {
                    event_message(Event::Scrub(seconds))
                })
                .on_release(event_message(Event::ScrubEnd))
                .step(1u32)
                .width(Length::Fill)
                .into()
            } else {
                widget::progress_bar::determinate_linear(at as f32 / total as f32)
                    .width(Length::Fill)
                    .into()
            };
            let elapsed = popup::detail(clock(i64::from(at) * 1_000_000), ctx);
            card = card.block(popup::column().push(seek).push(popup::split(
                match self.scrub {
                    Some(_) => elapsed.class(cosmic::theme::Text::Color(palette.accent())),
                    None => elapsed,
                },
                [popup::detail(clock(player.length_us), ctx).into()],
            )));
        } else if played > 0 {
            card = card.block(popup::detail(clock(played), ctx));
        }

        let playing = player.status == Status::Playing;
        let transport = widget::Row::new()
            .spacing(popup::ROW_GAP)
            .push(control(
                ctx,
                PREVIOUS,
                Chip::Plain,
                player.can_previous.then_some(Action::Previous),
            ))
            .push(control(
                ctx,
                if playing { PAUSE } else { PLAY },
                Chip::Accent,
                // A player that can neither pause nor play has no transport.
                (player.can_pause || player.can_play).then_some(Action::PlayPause),
            ))
            .push(control(
                ctx,
                NEXT,
                Chip::Plain,
                player.can_next.then_some(Action::Next),
            ))
            .push(control(
                ctx,
                STOP,
                Chip::Plain,
                (player.status != Status::Stopped).then_some(Action::Stop),
            ));

        // Shuffle and loop are states rather than verbs, so they sit where a
        // block's actions sit and light up when they are on.
        let mut modes: Vec<Element<'_, Message>> = Vec::new();
        if let Some(shuffle) = player.shuffle {
            modes.push(control(
                ctx,
                if shuffle { SHUFFLE } else { SHUFFLE_OFF },
                match shuffle {
                    true => Chip::Accent,
                    false => Chip::Plain,
                },
                Some(Action::Shuffle(!shuffle)),
            ));
        }
        if let Some(mode) = player.loop_status.as_deref() {
            let (glyph, style) = match mode {
                "Track" => (REPEAT_ONCE, Chip::Accent),
                "Playlist" => (REPEAT, Chip::Accent),
                _ => (REPEAT_OFF, Chip::Plain),
            };
            modes.push(control(
                ctx,
                glyph,
                style,
                Some(Action::Loop(next_loop(mode))),
            ));
        }
        card = card.block(popup::split(transport, modes));

        Some(
            card.maybe(self.error.as_ref().map(|error| {
                popup::detail(error.as_str(), ctx).class(cosmic::theme::Text::Color(palette.red))
            }))
            .build(),
        )
    }

    /// The player the bar speaks for: the pinned one, else the one that is
    /// playing, else the one that is merely paused, else the first.
    fn current(&self) -> Option<&Player> {
        if let Some(bus) = &self.selected
            && let Some(player) = self.players.iter().find(|player| &player.bus == bus)
        {
            return Some(player);
        }
        self.players
            .iter()
            .find(|player| player.status == Status::Playing)
            .or_else(|| {
                self.players
                    .iter()
                    .find(|player| player.status == Status::Paused)
            })
            .or_else(|| self.players.first())
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Mpris(event))
}

/// One transport button, drawn dead when the player says it cannot: the
/// affordance staying put is what tells you the player is the limit.
fn control<'a>(
    ctx: &Ctx,
    glyph: &'a str,
    style: Chip,
    action: Option<Action>,
) -> Element<'a, Message> {
    popup::icon_chip(
        glyph,
        style,
        ctx,
        action.map(|action| event_message(Event::Dispatch(action))),
    )
}

/// MPRIS cycles `None` -> `Playlist` -> `Track`, which is the order players
/// themselves offer.
fn next_loop(current: &str) -> &'static str {
    match current {
        "Playlist" => "Track",
        "Track" => "None",
        _ => "Playlist",
    }
}

/// `m:ss`, or `h:mm:ss` past an hour.
fn clock(micros: i64) -> String {
    let seconds = (micros / 1_000_000).max(0);
    match seconds {
        ..3600 => format!("{}:{:02}", seconds / 60, seconds % 60),
        _ => format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        ),
    }
}

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/*----------------------------------------------------------------------------
    the session bus worker
----------------------------------------------------------------------------*/

/// A request from the popup, with somewhere to put the answer.
struct Request {
    bus: String,
    action: Action,
    reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// The live worker's request channel. Replaced whenever the bar restarts the
/// subscription, so an action never targets a dead connection.
static REQUESTS: LazyLock<Mutex<Option<UnboundedSender<Request>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Run one action against one player and wait for the bus to answer.
async fn dispatch(bus: String, action: Action) -> Result<(), String> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let sender = REQUESTS
        .lock()
        .map_err(|_| "mpris worker poisoned".to_owned())?
        .clone()
        .ok_or_else(|| "no session bus".to_owned())?;
    sender
        .unbounded_send(Request { bus, action, reply })
        .map_err(|_| "mpris worker gone".to_owned())?;
    answer.await.map_err(|_| "mpris worker gone".to_owned())?
}

fn events() -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(8, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            if let Err(error) = session(&mut sender).await {
                log::debug!("mpris session ended: {error}");
            }
            if let Ok(mut requests) = REQUESTS.lock() {
                *requests = None;
            }
            if sender.send(Event::Gone).await.is_err() {
                return;
            }
            if started.elapsed() >= STABLE_SESSION {
                attempt = 0;
            }
            let delay = RECONNECT_BACKOFF_SECS[attempt.min(RECONNECT_BACKOFF_SECS.len() - 1)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

type Events = cosmic::iced::futures::channel::mpsc::Sender<Event>;

/// Anything the worker has to react to, folded into one stream.
enum Wire {
    Name(zbus::fdo::NameOwnerChanged),
    Properties(BusMessage),
    Seeked(BusMessage),
    Request(Request),
}

/// One bus connection's worth of players. Returns when the bus goes away.
async fn session(events: &mut Events) -> zbus::Result<()> {
    let connection = Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;

    // Two match rules cover every player, present and future, so a new player
    // costs one `GetAll` and no extra subscriptions.
    let properties = MessageStream::for_match_rule(
        MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path(OBJECT_PATH)?
            .build(),
        &connection,
        Some(64),
    )
    .await?;
    let seeked = MessageStream::for_match_rule(
        MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(PLAYER_IFACE)?
            .member("Seeked")?
            .path(OBJECT_PATH)?
            .build(),
        &connection,
        Some(16),
    )
    .await?;
    let names = dbus.receive_name_owner_changed().await?;

    let (sender, requests) = unbounded::<Request>();
    if let Ok(mut slot) = REQUESTS.lock() {
        *slot = Some(sender);
    }

    let mut players: BTreeMap<String, Player> = BTreeMap::new();
    // Unique bus name -> well-known name, so a signal's sender can be resolved.
    let mut owners: HashMap<String, String> = HashMap::new();

    for name in dbus.list_names().await? {
        let name = name.as_str().to_owned();
        if !is_player(&name) {
            continue;
        }
        if let Ok(bus) = BusName::try_from(name.clone())
            && let Ok(owner) = dbus.get_name_owner(bus).await
        {
            owners.insert(owner.as_str().to_owned(), name.clone());
        }
        if let Some(player) = load(&connection, &name).await {
            players.insert(name, player);
        }
    }
    publish(events, &players).await?;

    let mut wires = cosmic::iced::futures::stream::select_all([
        names.map(Wire::Name).boxed(),
        properties.filter_map(ok).map(Wire::Properties).boxed(),
        seeked.filter_map(ok).map(Wire::Seeked).boxed(),
        requests.map(Wire::Request).boxed(),
    ]);

    while let Some(wire) = wires.next().await {
        let mut changed = false;
        match wire {
            Wire::Name(signal) => {
                let Ok(args) = signal.args() else { continue };
                let name = args.name().as_str().to_owned();
                if !is_player(&name) {
                    continue;
                }
                if let Some(old) = args.old_owner().as_ref() {
                    owners.remove(old.as_str());
                }
                match args.new_owner().as_ref() {
                    Some(new) => {
                        owners.insert(new.as_str().to_owned(), name.clone());
                        if let Some(player) = load(&connection, &name).await {
                            players.insert(name, player);
                            changed = true;
                        }
                    }
                    None => changed = players.remove(&name).is_some(),
                }
            }
            Wire::Properties(message) => {
                let Some(bus) = sender_of(&message, &owners) else {
                    continue;
                };
                let Some(player) = players.get_mut(&bus) else {
                    continue;
                };
                let body = message.body();
                let Ok((interface, updates, invalidated)) =
                    body.deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                else {
                    continue;
                };
                if interface == PLAYER_IFACE {
                    let was = player.status;
                    apply(player, &updates, now_ms());
                    // A status flip moves `Position` without a signal, and so
                    // does anything the player chose to invalidate.
                    if was != player.status
                        || updates.contains_key("Metadata")
                        || invalidated.iter().any(|name| name == "Position")
                    {
                        resync(&connection, player).await;
                    }
                    changed = true;
                } else if interface == ROOT_IFACE
                    && let Some(identity) = updates.get("Identity").and_then(text) {
                        player.identity = identity;
                        changed = true;
                    }
            }
            Wire::Seeked(message) => {
                let Some(bus) = sender_of(&message, &owners) else {
                    continue;
                };
                let Some(player) = players.get_mut(&bus) else {
                    continue;
                };
                if let Ok(position) = message.body().deserialize::<i64>() {
                    player.position_us = position;
                    player.sampled_ms = now_ms();
                    changed = true;
                }
            }
            Wire::Request(Request { bus, action, reply }) => {
                let outcome = match players.get_mut(&bus) {
                    Some(player) if action == Action::Resync => {
                        resync(&connection, player).await;
                        changed = true;
                        Ok(())
                    }
                    Some(player) => {
                        let player = player.clone();
                        act(&connection, &player, &action)
                            .await
                            .map_err(|error| error.to_string())
                    }
                    None => Err("player is gone".to_owned()),
                };
                let _ = reply.send(outcome);
            }
        }
        if changed {
            publish(events, &players).await?;
        }
    }

    Ok(())
}

/// Send the whole player set; a handful of players makes diffing pointless.
async fn publish(events: &mut Events, players: &BTreeMap<String, Player>) -> zbus::Result<()> {
    let snapshot: Vec<Player> = players.values().cloned().collect();
    events
        .send(Event::Players(Arc::new(snapshot)))
        .await
        .map_err(|_| zbus::Error::InputOutput(Arc::new(std::io::Error::other("bar gone"))))
}

fn ok(message: zbus::Result<BusMessage>) -> std::future::Ready<Option<BusMessage>> {
    std::future::ready(message.ok())
}

/// Which player a signal came from. Signals carry the sender's unique name,
/// which only the `NameOwnerChanged` bookkeeping can map back to a player.
fn sender_of(message: &BusMessage, owners: &HashMap<String, String>) -> Option<String> {
    let header = message.header();
    let sender = header.sender()?;
    owners.get(sender.as_str()).cloned()
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Read a player's whole state in two calls.
async fn load(connection: &Connection, bus: &str) -> Option<Player> {
    let properties = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(bus.to_owned())
        .ok()?
        .path(OBJECT_PATH)
        .ok()?
        .build()
        .await
        .ok()?;

    let mut player = Player {
        bus: bus.to_owned(),
        identity: bus.rsplit('.').next().unwrap_or(bus).to_owned(),
        rate: 1.0,
        ..Player::default()
    };
    if let Ok(root) = properties.get_all(ROOT_IFACE.try_into().ok()?).await
        && let Some(identity) = root.get("Identity").and_then(text)
        && !identity.is_empty()
    {
        player.identity = identity;
    }
    // A name on the bus that does not answer for the Player interface is not a
    // player the bar can drive.
    let state = properties.get_all(PLAYER_IFACE.try_into().ok()?).await.ok()?;
    apply(&mut player, &state, now_ms());
    if !state.contains_key("Position") {
        resync(connection, &mut player).await;
    }
    Some(player)
}

/// Fold a `GetAll` result or a `PropertiesChanged` payload into a player.
fn apply(player: &mut Player, updates: &HashMap<String, OwnedValue>, now: i64) {
    if let Some(status) = updates.get("PlaybackStatus").and_then(text) {
        player.status = Status::parse(&status);
    }
    if let Some(rate) = updates.get("Rate").and_then(number) {
        // A zero or negative rate would freeze or reverse the clock.
        player.rate = if rate > 0.0 { rate } else { 1.0 };
    }
    if let Some(shuffle) = updates.get("Shuffle").and_then(flag) {
        player.shuffle = Some(shuffle);
    }
    if let Some(mode) = updates.get("LoopStatus").and_then(text) {
        player.loop_status = Some(mode);
    }
    if let Some(can) = updates.get("CanGoNext").and_then(flag) {
        player.can_next = can;
    }
    if let Some(can) = updates.get("CanGoPrevious").and_then(flag) {
        player.can_previous = can;
    }
    if let Some(can) = updates.get("CanPause").and_then(flag) {
        player.can_pause = can;
    }
    if let Some(can) = updates.get("CanPlay").and_then(flag) {
        player.can_play = can;
    }
    if let Some(can) = updates.get("CanSeek").and_then(flag) {
        player.can_seek = can;
    }
    // Metadata comes before `Position` on purpose: a new track restarts the
    // timeline, and a `GetAll` reply carries both, so resetting afterwards
    // would throw away the position the player just told us.
    if let Some(metadata) = updates.get("Metadata") {
        let fields = dictionary(metadata);
        let track = fields
            .get("mpris:trackid")
            .and_then(|value| text_value(value));
        let title = fields
            .get("xesam:title")
            .and_then(|value| text_value(value))
            .unwrap_or_default();
        // The same track's metadata being re-announced — a cover arriving
        // late, a title correction — must not rewind the stream.
        if track != player.track || title != player.title {
            player.position_us = 0;
            player.sampled_ms = now;
        }
        player.track = track;
        player.title = title;
        player.artist = fields
            .get("xesam:artist")
            .map(|value| strings(value).join(", "))
            .unwrap_or_default();
        player.album = fields
            .get("xesam:album")
            .and_then(|value| text_value(value))
            .unwrap_or_default();
        player.length_us = fields
            .get("mpris:length")
            .and_then(|value| integer_value(value))
            .unwrap_or(0);
    }
    if let Some(position) = updates.get("Position").and_then(integer) {
        player.position_us = position;
        player.sampled_ms = now;
    }
}

/// Ask the player where it actually is.
async fn resync(connection: &Connection, player: &mut Player) {
    let Ok(properties) = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(player.bus.clone())
        .and_then(|builder| builder.path(OBJECT_PATH))
    else {
        return;
    };
    let Ok(properties) = properties.build().await else {
        return;
    };
    let Ok(interface) = PLAYER_IFACE.try_into() else {
        return;
    };
    if let Ok(value) = properties.get(interface, "Position").await
        && let Some(position) = integer_value(&value)
    {
        player.position_us = position;
        player.sampled_ms = now_ms();
    }
}

/// Call one MPRIS method, or set one MPRIS property.
async fn act(connection: &Connection, player: &Player, action: &Action) -> zbus::Result<()> {
    let proxy = Proxy::new(connection, player.bus.clone(), OBJECT_PATH, PLAYER_IFACE).await?;
    match action {
        Action::Next => proxy.call("Next", &()).await,
        Action::Previous => proxy.call("Previous", &()).await,
        Action::PlayPause => proxy.call("PlayPause", &()).await,
        Action::Stop => proxy.call("Stop", &()).await,
        Action::Shuffle(on) => proxy.set_property("Shuffle", *on).await.map_err(Into::into),
        Action::Loop(mode) => proxy
            .set_property("LoopStatus", *mode)
            .await
            .map_err(Into::into),
        // `SetPosition` is the only absolute seek, and it needs the track it
        // applies to; without a usable track id, fall back to a relative jump.
        Action::Seek(target) => match player
            .track
            .as_deref()
            .and_then(|track| ObjectPath::try_from(track).ok())
        {
            Some(track) => proxy.call("SetPosition", &(&track, *target)).await,
            None => {
                let offset = target - player.elapsed_us(now_ms());
                proxy.call("Seek", &offset).await
            }
        },
        // Handled by the worker, which owns the player state.
        Action::Resync => Ok(()),
    }
}

/*----------------------------------------------------------------------------
    zvariant helpers: players are careless about types, so accept what fits
----------------------------------------------------------------------------*/

fn text(value: &OwnedValue) -> Option<String> {
    text_value(value)
}

fn text_value(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(text) => Some(text.to_string()),
        Value::ObjectPath(path) => Some(path.to_string()),
        Value::Value(inner) => text_value(inner),
        // Some players hand a single-element array where a string is expected.
        Value::Array(array) => array.inner().first().and_then(text_value),
        _ => None,
    }
}

fn integer(value: &OwnedValue) -> Option<i64> {
    integer_value(value)
}

fn integer_value(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::I64(number) => Some(*number),
        Value::U64(number) => Some(*number as i64),
        Value::I32(number) => Some(*number as i64),
        Value::U32(number) => Some(*number as i64),
        Value::I16(number) => Some(*number as i64),
        Value::U16(number) => Some(*number as i64),
        Value::U8(number) => Some(*number as i64),
        Value::F64(number) => Some(*number as i64),
        Value::Value(inner) => integer_value(inner),
        _ => None,
    }
}

fn number(value: &OwnedValue) -> Option<f64> {
    match &**value {
        Value::F64(number) => Some(*number),
        other => integer_value(other).map(|number| number as f64),
    }
}

fn flag(value: &OwnedValue) -> Option<bool> {
    match &**value {
        Value::Bool(flag) => Some(*flag),
        Value::Value(inner) => match &**inner {
            Value::Bool(flag) => Some(*flag),
            _ => None,
        },
        _ => None,
    }
}

/// `xesam:artist` is an array of strings, but players ship plain strings too.
fn strings(value: &Value<'_>) -> Vec<String> {
    match value {
        Value::Array(array) => array.inner().iter().filter_map(text_value).collect(),
        Value::Value(inner) => strings(inner),
        other => text_value(other).into_iter().collect(),
    }
}

/// `Metadata` is `a{sv}`, sometimes wrapped in another variant.
fn dictionary(value: &Value<'_>) -> HashMap<String, Value<'static>> {
    match value {
        Value::Dict(dict) => dict
            .iter()
            .filter_map(|(key, entry)| {
                Some((text_value(key)?, entry.try_to_owned().ok()?.into()))
            })
            .collect(),
        Value::Value(inner) => dictionary(inner),
        _ => HashMap::new(),
    }
}
