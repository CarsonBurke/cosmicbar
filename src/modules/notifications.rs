//! Notifications: mako, driven by its own D-Bus interface.
//!
//! mako publishes `fr.emersion.Mako` on the `org.freedesktop.Notifications`
//! bus name (path `/fr/emersion/Mako`, confirmed by introspection on this
//! machine, mako 1.11). Its `Notifications` and `Modes` properties are
//! declared `emits-invalidation`, so every arrival, expiry, dismissal and mode
//! change lands as a `PropertiesChanged` with the name invalidated; the module
//! re-reads the lists on that signal and never polls. waybar ran `makoctl
//! list` on a five-second interval and counted lines with grep.
//!
//! The waybar module could only count, toggle do-not-disturb and dismiss
//! everything. Here the popup is the notification list itself: per-entry
//! dismiss, per-entry default action, restore from history, mode toggle.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
use cosmic::iced::{ContentFit, Length, Subscription};
use cosmic::widget;
use zbus::zvariant::Value;

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

/// waybar painted `#custom-mako` `@tray`, which is `@mantle`.
pub const ISLAND: Island = Island::Join;

/// nf-md-bell_badge: something is waiting.
const BELL_WAITING: &str = "\u{f116b}";
/// nf-md-bell_outline: nothing waiting.
const BELL_IDLE: &str = "\u{f009c}";
/// nf-md-bell_off: do-not-disturb, with notifications piling up behind it.
const BELL_DND_WAITING: &str = "\u{f009b}";
/// nf-md-bell_off_outline: do-not-disturb and nothing waiting.
const BELL_DND_IDLE: &str = "\u{f0a91}";
/// nf-md-close: dismiss one entry.
const DISMISS: &str = "\u{f0156}";

/// mako's mode for "hold everything back", the one `makoctl mode -t
/// do-not-disturb` toggled.
const DND: &str = "do-not-disturb";
/// mako's always-present base mode; dropping it would break its config match.
const DEFAULT_MODE: &str = "default";

const SERVICE: &str = "org.freedesktop.Notifications";
const MAKO_PATH: &str = "/fr/emersion/Mako";

/// Longest summary and body rendered in the popup before elision.
const SUMMARY_LIMIT: usize = 64;
const BODY_LIMIT: usize = 160;
/// History entries shown; mako's own `max-history` is usually 5.
const HISTORY_LIMIT: usize = 5;
/// Preview texture bounds. Full-resolution screenshots are decoded and
/// downsampled off the UI thread before a widget ever sees them.
const IMAGE_WIDTH: u32 = 384;
const IMAGE_HEIGHT: u32 = 180;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_ALLOC: u64 = 192 * 1024 * 1024;

/// Reconnect ladder for a session bus that is down or restarting.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A session that lasted this long was healthy: the next failure starts the
/// ladder over instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// One notification, flattened out of mako's `a{sv}` into what the bar draws.
#[derive(Debug, Clone)]
pub struct Notification {
    id: u32,
    app_name: String,
    summary: String,
    body: String,
    /// Local raster image supplied through mako's `app-icon` field. The path is
    /// cheap notification metadata; its thumbnail is loaded only for the popup.
    image_path: Option<PathBuf>,
    /// 0 low, 1 normal, 2 critical, per the freedesktop spec.
    urgency: u8,
    /// Action key to label, in the order mako returned them.
    actions: Vec<(String, String)>,
}

/// Everything the module knows, parsed off the UI thread.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    waiting: Vec<Notification>,
    history: Vec<Notification>,
    modes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageCache {
    entries: HashMap<PathBuf, CachedImage>,
}

#[derive(Debug, Clone)]
struct CachedImage {
    fingerprint: Fingerprint,
    /// `None` remembers a failed decode until the file changes.
    handle: Option<widget::image::Handle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone)]
struct ImageRequest {
    revision: u64,
    paths: Vec<PathBuf>,
    cached: ImageCache,
}

// The cached handles are input data, not subscription identity. A result must
// not restart its own request; a new mako snapshot must invalidate old work
// even when an application overwrites the same path.
impl PartialEq for ImageRequest {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.paths == other.paths
    }
}

impl Eq for ImageRequest {}

impl Hash for ImageRequest {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.revision.hash(state);
        self.paths.hash(state);
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The session bus, with a terse `Debug`: the bar logs every message it
/// handles, and a `zbus::Connection` prints its entire match-rule table.
#[derive(Clone)]
pub struct Bus(zbus::Connection);

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bus")
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// mako answered. Carries the bus so `update` can call it back.
    Connected(Bus),
    Snapshot(Arc<Snapshot>),
    ImagesLoaded {
        revision: u64,
        paths: Arc<[PathBuf]>,
        images: ImageCache,
    },
    ImagesCleared,
    /// The service is not on the bus: the module hides.
    Absent,
    Disconnected,
    Dismiss(u32),
    DismissAll,
    Invoke { id: u32, action: String },
    Restore,
    ToggleDnd,
    /// Result of a mutation, so a failure is visible instead of silent.
    Mutated(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    connection: Option<zbus::Connection>,
    snapshot: Option<Arc<Snapshot>>,
    revision: u64,
    images: ImageCache,
    error: Option<String>,
}

impl State {
    /// Text and counts stay on the always-live mako signal stream. A second
    /// subscription exists only while the popup is open: it loads bounded
    /// thumbnails and is dropped — canceling queued work — on close.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        let live = Subscription::run(stream).map(event_message);
        let images = if open {
            Subscription::run_with(
                ImageRequest {
                    revision: self.revision,
                    paths: self.image_paths(),
                    cached: self.images.clone(),
                },
                image_stream,
            )
            .map(event_message)
        } else {
            Subscription::run(clear_images).map(event_message)
        };
        Subscription::batch([live, images])
    }
    fn image_paths(&self) -> Vec<PathBuf> {
        self.snapshot
            .as_deref()
            .map(image_paths)
            .unwrap_or_default()
    }


    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Connected(Bus(connection)) => {
                self.connection = Some(connection);
                self.error = None;
                Task::none()
            }
            Event::Snapshot(snapshot) => {
                self.revision = self.revision.wrapping_add(1);
                let paths = image_paths(&snapshot);
                self.images.entries.retain(|path, _| paths.contains(path));
                self.snapshot = Some(snapshot);
                Task::none()
            }
            Event::ImagesLoaded {
                revision,
                paths,
                images,
            } => {
                if revision == self.revision && self.image_paths().as_slice() == paths.as_ref() {
                    self.images = images;
                }
                Task::none()
            }
            Event::ImagesCleared => {
                self.images.entries.clear();
                Task::none()
            }
            Event::Absent => {
                self.snapshot = None;
                self.images.entries.clear();
                Task::none()
            }
            Event::Disconnected => {
                self.connection = None;
                self.snapshot = None;
                self.images.entries.clear();
                Task::none()
            }
            Event::Dismiss(id) => self.call(move |mako| async move {
                mako.dismiss_notifications(HashMap::from([("id", Value::U32(id))]))
                    .await
            }),
            Event::DismissAll => self
                .call(|mako| async move {
                    mako.dismiss_notifications(HashMap::from([("all", Value::Bool(true))]))
                        .await
                })
                // Nothing is left to act on, so the list should not stay up.
                .chain(Task::done(cosmic::Action::App(Message::ClosePopup))),
            Event::Invoke { id, action } => self
                .call(move |mako| async move { mako.invoke_action(id, &action).await })
                // The action usually raises a window; the popup would be in
                // front of whatever the user just asked for.
                .chain(Task::done(cosmic::Action::App(Message::ClosePopup))),
            Event::Restore => self.call(|mako| async move { mako.restore_notification().await }),
            Event::ToggleDnd => {
                let modes = self.modes_after_dnd_toggle();
                self.call(move |mako| async move {
                    let modes: Vec<&str> = modes.iter().map(String::as_str).collect();
                    mako.set_modes(&modes).await
                })
            }
            Event::Mutated(result) => {
                self.error = result.err();
                Task::none()
            }
        }
    }

    /// `None` hides the module: with no notification service there is nothing
    /// to count and nothing the popup could do.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let snapshot = self.snapshot.as_ref()?;
        let palette = ctx.palette;
        let count = snapshot.waiting.len();
        let dnd = snapshot.dnd();

        let (glyph, color) = match (dnd, count) {
            // waybar's `.dnd-notification` and `.dnd-none` were both @overlay0.
            (true, 0) => (BELL_DND_IDLE, palette.overlay0),
            (true, _) => (BELL_DND_WAITING, palette.overlay0),
            (false, 0) => (BELL_IDLE, palette.muted()),
            // waybar's `.notification` was @accent; a critical notification
            // earns the critical colour instead.
            (false, _) => (
                BELL_WAITING,
                if snapshot.critical() {
                    palette.red
                } else {
                    palette.accent()
                },
            ),
        };

        let rest = if count > 0 {
            count.to_string()
        } else {
            String::new()
        };
        Some(crate::theme::label(
            glyph,
            rest,
            ctx.font_size,
            cosmic::theme::Text::Color(color),
        ))
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let snapshot = self.snapshot.as_ref()?;
        let palette = ctx.palette;
        let waiting = snapshot.waiting.len();
        let dnd = snapshot.dnd();

        // The popup hangs under the bell glyph, so the header only owes the
        // count it is listing and whether mako is holding them back.
        let mut heading = popup::lines().push(
            popup::title(format!("{waiting} waiting"), ctx).class(cosmic::theme::Text::Color(
                if waiting == 0 || dnd {
                    palette.muted()
                } else if snapshot.critical() {
                    palette.red
                } else {
                    palette.accent()
                },
            )),
        );
        if dnd {
            heading = heading.push(popup::detail("do not disturb", ctx));
        }
        let mut card = Card::new().block(popup::split(heading, []));

        let history: Vec<&Notification> = snapshot.history.iter().take(HISTORY_LIMIT).collect();
        if waiting > 0 || !history.is_empty() {
            let mut list = popup::column();
            for notification in &snapshot.waiting {
                list = list.push(self.entry(notification, ctx));
            }
            if !history.is_empty() {
                list = list.push(popup::split(
                    popup::section("history", ctx),
                    [popup::chip(
                        "restore",
                        Chip::Plain,
                        ctx,
                        Some(event_message(Event::Restore)),
                    )],
                ));
                // A dismissed notification has no actions left to offer, so
                // its line is just enough to recognise what was restored.
                for notification in &history {
                    list = list.push(popup::detail(
                        format!(
                            "{} · {}",
                            notification.app_name,
                            elide(&notification.summary, SUMMARY_LIMIT)
                        ),
                        ctx,
                    ));
                }
            }
            card = card.list(list);
        }

        let modes = snapshot
            .modes
            .iter()
            .filter(|mode| *mode != DEFAULT_MODE)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        card = card.block(popup::split(
            popup::detail(
                if modes.is_empty() {
                    "mode: default".to_owned()
                } else {
                    format!("mode: {modes}")
                },
                ctx,
            ),
            [
                popup::chip(
                    if dnd { "allow" } else { "do not disturb" },
                    Chip::Plain,
                    ctx,
                    Some(event_message(Event::ToggleDnd)),
                ),
                popup::chip(
                    "dismiss all",
                    Chip::Danger,
                    ctx,
                    (waiting > 0).then(|| event_message(Event::DismissAll)),
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

    /// One notification: clicking its text or image invokes the default
    /// action, the glyph chip dismisses it, and any further actions get a chip
    /// of their own.
    fn entry<'a>(&'a self, notification: &'a Notification, ctx: &Ctx) -> Element<'a, Message> {
        let palette = ctx.palette;
        let mut lines = popup::lines().push(
            popup::detail(
                format!(
                    "{}{}",
                    notification.app_name,
                    match notification.urgency {
                        2 => " · critical",
                        0 => " · low",
                        _ => "",
                    }
                ),
                ctx,
            )
            .class(cosmic::theme::Text::Color(match notification.urgency {
                2 => palette.red,
                0 => palette.overlay0,
                _ => palette.muted(),
            })),
        );
        lines = lines.push(popup::item(elide(&notification.summary, SUMMARY_LIMIT), ctx));
        if !notification.body.is_empty() {
            lines = lines.push(popup::detail(elide(&notification.body, BODY_LIMIT), ctx));
        }

        let default_action = notification
            .actions
            .iter()
            .find(|(key, _)| key == "default")
            .map(|(key, _)| key);
        let invoke_default = || {
            default_action.map(|action| {
                event_message(Event::Invoke {
                    id: notification.id,
                    action: action.clone(),
                })
            })
        };
        // Only a notification that has somewhere to go is a click target; one
        // without a default action is text that happens to have chips beside
        // it, and lighting it up would promise an action it cannot perform.
        let content: Element<'a, Message> = match invoke_default() {
            Some(message) => popup::row(lines, palette, Some(message)),
            None => lines.into(),
        };

        let mut actions: Vec<Element<'a, Message>> = notification
            .actions
            .iter()
            .filter(|(key, _)| key != "default")
            .map(|(key, label)| {
                popup::chip(
                    label.as_str(),
                    Chip::Plain,
                    ctx,
                    Some(event_message(Event::Invoke {
                        id: notification.id,
                        action: key.clone(),
                    })),
                )
            })
            .collect();
        actions.push(popup::icon_chip(
            DISMISS,
            Chip::Danger,
            ctx,
            Some(event_message(Event::Dismiss(notification.id))),
        ));

        let mut entry = popup::lines().push(popup::split(content, actions));
        if let Some(handle) = notification
            .image_path
            .as_deref()
            .and_then(|path| self.images.handle(path))
        {
            let image = widget::image(handle)
                .width(Length::Fill)
                .height(Length::Fixed(IMAGE_HEIGHT as f32))
                .content_fit(ContentFit::Contain)
                .border_radius(crate::theme::ROW_CORNERS);
            entry = entry.push(match invoke_default() {
                Some(message) => popup::row(image, palette, Some(message)),
                None => image.into(),
            });
        }
        entry.into()
    }

    /// The mode list mako should end up with when do-not-disturb is toggled.
    fn modes_after_dnd_toggle(&self) -> Vec<String> {
        let current = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.modes.clone())
            .unwrap_or_default();
        let mut modes: Vec<String> = current
            .iter()
            .filter(|mode| *mode != DND)
            .cloned()
            .collect();
        if modes.is_empty() {
            modes.push(DEFAULT_MODE.to_owned());
        }
        if current.iter().all(|mode| mode != DND) {
            modes.push(DND.to_owned());
        }
        modes
    }

    /// Run one mako method on the session bus. mako answers every mutation
    /// with a `PropertiesChanged`, so the refreshed list arrives through the
    /// subscription instead of being fetched here.
    fn call<F, Fut>(&self, request: F) -> Task<Message>
    where
        F: FnOnce(MakoProxy<'static>) -> Fut + Send + 'static,
        Fut: Future<Output = zbus::Result<()>> + Send,
    {
        let Some(connection) = self.connection.clone() else {
            return Task::none();
        };
        Task::future(async move {
            let result = async {
                let mako = MakoProxy::new(&connection).await?;
                request(mako).await
            }
            .await;
            cosmic::Action::App(event_message(Event::Mutated(
                result.map_err(|error| error.to_string()),
            )))
        })
    }
}

impl Snapshot {
    fn dnd(&self) -> bool {
        self.modes.iter().any(|mode| mode == DND)
    }

    fn critical(&self) -> bool {
        self.waiting.iter().any(|entry| entry.urgency >= 2)
    }
}

/// mako's private interface. Only the methods are declared: the two properties
/// carry the same data as `ListNotifications`/`ListModes`, and asking for them
/// as methods keeps zbus's property cache out of the picture.
#[zbus::proxy(
    interface = "fr.emersion.Mako",
    default_service = "org.freedesktop.Notifications",
    default_path = "/fr/emersion/Mako"
)]
trait Mako {
    /// Keys mako understands: `id` (u32), `all` (bool), `group` (bool),
    /// `history` (bool, default true).
    fn dismiss_notifications(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn invoke_action(&self, id: u32, action: &str) -> zbus::Result<()>;
    fn list_notifications(
        &self,
    ) -> zbus::Result<Vec<HashMap<String, zbus::zvariant::OwnedValue>>>;
    fn list_history(&self) -> zbus::Result<Vec<HashMap<String, zbus::zvariant::OwnedValue>>>;
    fn list_modes(&self) -> zbus::Result<Vec<String>>;
    fn restore_notification(&self) -> zbus::Result<()>;
    fn set_modes(&self, modes: &[&str]) -> zbus::Result<()>;
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Notifications(event))
}

fn clear_images() -> impl Stream<Item = Event> {
    cosmic::iced::futures::stream::once(async { Event::ImagesCleared })
}

fn image_stream(request: &ImageRequest) -> impl Stream<Item = Event> + use<> {
    let revision = request.revision;
    let mut images = request.cached.clone();
    let paths = request.paths.clone();
    cosmic::iced::stream::channel(1, async move |mut sender| {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        images.refresh(paths.clone(), cancelled.clone()).await;
        if !cancelled.load(Ordering::Acquire) {
            let _ = sender
                .send(Event::ImagesLoaded {
                    revision,
                    paths: paths.into(),
                    images,
                })
                .await;
        }
    })
}

fn stream() -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(8, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            if let Err(error) = session(&mut sender).await {
                log::debug!("mako subscription ended: {error:#}");
            }
            let _ = sender.send(Event::Disconnected).await;
            if started.elapsed() >= STABLE_SESSION {
                attempt = 0;
            }
            let delay = RECONNECT_BACKOFF_SECS[attempt.min(RECONNECT_BACKOFF_SECS.len() - 1)];
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

/// One session bus connection's worth of updates. Returns only when the bus
/// itself goes away; mako coming and going is handled in place, so restarting
/// the daemon does not cost a reconnect ladder.
async fn session(
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Event>,
) -> anyhow::Result<()> {
    let connection = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
    // Only this one name: mako restarting, or not running yet.
    let mut owner_changed = dbus
        .receive_name_owner_changed_with_args(&[(0, SERVICE)])
        .await?;
    // `Notifications` and `Modes` are emits-invalidation, so the signal says
    // "re-read" rather than carrying the new value.
    let properties = zbus::fdo::PropertiesProxy::builder(&connection)
        .destination(SERVICE)?
        .path(MAKO_PATH)?
        .build()
        .await?;
    let mut changed = properties.receive_properties_changed().await?;

    if sender
        .send(Event::Connected(Bus(connection.clone())))
        .await
        .is_err()
    {
        return Ok(());
    }
    if publish(&connection, sender).await.is_err() {
        return Ok(());
    }

    loop {
        let refresh = tokio::select! {
            signal = changed.next() => signal.is_some(),
            // Any owner change for the name means mako appeared, vanished or
            // restarted; one refresh answers all three.
            signal = owner_changed.next() => signal.is_some(),
        };
        if !refresh {
            // Both signal streams ended: the connection is finished.
            return Ok(());
        }
        if publish(&connection, sender).await.is_err() {
            return Ok(());
        }
    }
}

/// Read mako's lists and push them. A failure here means the service is not on
/// the bus, which is a hidden module rather than an error.
async fn publish(
    connection: &zbus::Connection,
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Event>,
) -> Result<(), ()> {
    let event = match snapshot(connection).await {
        Ok(snapshot) => Event::Snapshot(Arc::new(snapshot)),
        Err(error) => {
            log::debug!("mako unavailable: {error:#}");
            Event::Absent
        }
    };
    sender.send(event).await.map_err(|_| ())
}

async fn snapshot(connection: &zbus::Connection) -> anyhow::Result<Snapshot> {
    let mako = MakoProxy::new(connection).await?;
    let waiting = mako.list_notifications().await?;
    let history = mako.list_history().await?;
    Ok(Snapshot {
        waiting: waiting.iter().map(parse).collect(),
        history: history.iter().map(parse).collect(),
        modes: mako.list_modes().await?,
    })
}

fn image_paths(snapshot: &Snapshot) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = snapshot
        .waiting
        .iter()
        .filter_map(|notification| notification.image_path.clone())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

type Props = HashMap<String, zbus::zvariant::OwnedValue>;

/// Turn one `a{sv}` entry into a notification. Every field is optional as far
/// as this code is concerned: a missing or oddly typed key costs its own value
/// and nothing else.
fn parse(props: &Props) -> Notification {
    Notification {
        id: props
            .get("id")
            .and_then(|value| value.downcast_ref::<u32>().ok())
            .unwrap_or_default(),
        app_name: string(props, "app-name"),
        summary: string(props, "summary"),
        body: string(props, "body"),
        image_path: local_image_path(&string(props, "app-icon")),
        urgency: props
            .get("urgency")
            .and_then(|value| value.downcast_ref::<u8>().ok())
            .unwrap_or(1),
        actions: props.get("actions").map(actions).unwrap_or_default(),
    }
}

fn string(props: &Props, key: &str) -> String {
    props
        .get(key)
        .and_then(|value| value.downcast_ref::<&str>().ok())
        .unwrap_or_default()
        .to_owned()
}

impl ImageCache {
    /// Keep only requested images. Cache hits reuse the thumbnail handle;
    /// misses decode together on Tokio's blocking pool.
    async fn refresh(&mut self, paths: Vec<PathBuf>, cancelled: Arc<AtomicBool>) {
        let previous = std::mem::take(&mut self.entries);
        match tokio::task::spawn_blocking(move || load_images(paths, previous, &cancelled)).await {
            Ok(entries) => self.entries = entries,
            Err(error) => log::debug!("notification image worker failed: {error}"),
        }
    }

    fn handle(&self, path: &Path) -> Option<widget::image::Handle> {
        self.entries.get(path)?.handle.clone()
    }
}

fn load_images(
    paths: Vec<PathBuf>,
    mut previous: HashMap<PathBuf, CachedImage>,
    cancelled: &AtomicBool,
) -> HashMap<PathBuf, CachedImage> {
    let mut current = HashMap::with_capacity(paths.len());
    for path in paths {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        if current.contains_key(&path) {
            continue;
        }
        let Some(fingerprint) = fingerprint(&path) else {
            continue;
        };
        if let Some(cached) = previous.remove(&path)
            && cached.fingerprint == fingerprint
        {
            current.insert(path, cached);
            continue;
        }
        let handle = decode_thumbnail(&path);
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        current.insert(
            path,
            CachedImage {
                fingerprint,
                handle,
            },
        );
    }
    current
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = path.metadata().ok()?;
    metadata.is_file().then(|| Fingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn decode_thumbnail(path: &Path) -> Option<widget::image::Handle> {
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);

    let pixels = reader
        .decode()
        .inspect_err(|error| {
            log::debug!(
                "notification image {} could not be decoded: {error}",
                path.display()
            );
        })
        .ok()?
        .thumbnail(IMAGE_WIDTH, IMAGE_HEIGHT)
        .to_rgba8();
    let (width, height) = pixels.dimensions();
    Some(widget::image::Handle::from_rgba(
        width,
        height,
        pixels.into_raw(),
    ))
}

/// Mako exposes the original `app_icon` argument but not notification hints.
/// Screenshot tools, including niri's, put their capture here as a local path
/// or `file:` URI. Theme names remain ordinary app icons and are deliberately
/// not expanded into a large preview.
fn local_image_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    let path = Path::new(value);
    if path.is_absolute() {
        return Some(path.to_owned());
    }
    let url = url::Url::parse(value).ok()?;
    (url.scheme() == "file")
        .then(|| url.to_file_path().ok())
        .flatten()
}

/// `a{ss}` of action key to label.
fn actions(value: &zbus::zvariant::OwnedValue) -> Vec<(String, String)> {
    let Value::Dict(dict) = &**value else {
        return Vec::new();
    };
    dict.iter()
        .filter_map(|(key, label)| {
            Some((
                key.downcast_ref::<&str>().ok()?.to_owned(),
                label.downcast_ref::<&str>().ok()?.to_owned(),
            ))
        })
        .collect()
}

fn elide(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', " ");
    match flat.char_indices().nth(limit) {
        Some((index, _)) => format!("{}…", flat[..index].trim_end()),
        None => flat,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Event, IMAGE_HEIGHT, IMAGE_WIDTH, ImageCache, State, load_images, local_image_path,
    };
    use cosmic::widget;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, atomic::AtomicBool};

    #[test]
    fn local_image_path_accepts_paths_and_file_uris_only() {
        assert_eq!(
            local_image_path("/tmp/capture.png"),
            Some(PathBuf::from("/tmp/capture.png"))
        );
        assert_eq!(
            local_image_path("file:///tmp/a%20capture.png"),
            Some(PathBuf::from("/tmp/a capture.png"))
        );
        assert_eq!(local_image_path("camera-photo"), None);
        assert_eq!(local_image_path("https://example.com/capture.png"), None);
    }

    #[test]
    fn cancelled_loader_skips_queued_images() {
        let image = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/popup.png");
        let cancelled = AtomicBool::new(true);
        assert!(load_images(vec![image], HashMap::new(), &cancelled).is_empty());
    }

    #[tokio::test]
    async fn image_cache_decodes_a_bounded_thumbnail() {
        let image = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/popup.png");
        let mut cache = ImageCache::default();
        cache
            .refresh(
                vec![image.clone()],
                Arc::new(AtomicBool::new(false)),
            )
            .await;

        let handle = cache
            .entries
            .get(&image)
            .unwrap()
            .handle
            .clone()
            .unwrap();
        let widget::image::Handle::Rgba { width, height, .. } = &handle else {
            panic!("thumbnail was not decoded to RGBA");
        };
        assert!(*width <= IMAGE_WIDTH);
        assert!(*height <= IMAGE_HEIGHT);

        cache
            .refresh(
                vec![image.clone()],
                Arc::new(AtomicBool::new(false)),
            )
            .await;
        assert_eq!(
            cache.entries.get(&image).unwrap().handle.as_ref(),
            Some(&handle)
        );
    }

    #[tokio::test]
    async fn image_cache_remembers_decode_failures() {
        let text = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut cache = ImageCache::default();
        cache
            .refresh(vec![text.clone()], Arc::new(AtomicBool::new(false)))
            .await;
        assert!(cache.entries.get(&text).unwrap().handle.is_none());

        cache
            .refresh(vec![text.clone()], Arc::new(AtomicBool::new(false)))
            .await;
        assert!(cache.entries.get(&text).unwrap().handle.is_none());
    }

    #[tokio::test]
    async fn closing_popup_releases_thumbnail_handles() {
        let image = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/popup.png");
        let mut state = State::default();
        state
            .images
            .refresh(vec![image], Arc::new(AtomicBool::new(false)))
            .await;
        assert!(!state.images.entries.is_empty());

        let _task = state.update(Event::ImagesCleared);
        assert!(state.images.entries.is_empty());
    }
}
