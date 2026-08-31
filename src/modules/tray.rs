//! System tray: a `StatusNotifierItem` host with a real `DBusMenu` renderer.
//!
//! The bar hosts the `org.kde.StatusNotifierWatcher` service itself. When
//! something else already owns that name (a KDE session's `kded6`, which the
//! niri config used to start purely so waybar had a watcher to talk to) the
//! request is queued instead and we act as a plain host on the existing
//! watcher; the moment that owner goes away the name falls to us. Either way
//! no external daemon is needed.
//!
//! Everything is push: `system-tray` turns the item and menu protocols into a
//! broadcast stream of property changes, so items appear, change icon and
//! vanish without the bar ever asking.
//!
//! Interaction, mirroring the waybar tray plus what waybar could not do:
//! left click activates an item, middle click is its secondary activation,
//! right click opens the item's `DBusMenu` in the bar's popup — and the popup
//! keeps a selector row, so one popup can walk every item's menu.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget::{self, icon};
use cosmic::{Apply, Element};

use system_tray::client::{ActivateRequest, Client, Event as ClientEvent, UpdateEvent};
use system_tray::item::{IconPixmap, Status, StatusNotifierItem, Tooltip};
use system_tray::menu::{
    Disposition, MenuDiff, MenuItem, MenuType, ToggleState, ToggleType, TrayMenu,
};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

/// waybar painted the tray island `@tray`, which is `@mantle`.
pub const ISLAND: Island = Island::Join;

/// nf-md-application_outline: an item whose icon could not be resolved.
const UNKNOWN_ICON: &str = "\u{f0614}";
/// nf-md-alert_circle: drawn over an item asking for attention that has no
/// attention icon of its own (waybar used `-gtk-icon-effect: highlight`,
/// which has no equivalent for a raster handle).
const ATTENTION_BADGE: &str = "\u{f0028}";
/// nf-md-checkbox_marked / nf-md-checkbox_blank_outline.
const CHECK_ON: &str = "\u{f0132}";
const CHECK_OFF: &str = "\u{f0131}";
/// nf-md-radiobox_marked / nf-md-radiobox_blank.
const RADIO_ON: &str = "\u{f043e}";
const RADIO_OFF: &str = "\u{f043d}";
/// nf-md-chevron_down / nf-md-chevron_right: an expanded or collapsed submenu.
const SUBMENU_OPEN: &str = "\u{f0140}";
const SUBMENU_CLOSED: &str = "\u{f0142}";

/// Icon edge as a fraction of the bar height. waybar asked for 16px in a 32px
/// bar; deriving it keeps the proportion when the bar is resized.
const ICON_FRACTION: f32 = 0.56;
/// Indent per submenu level, in logical pixels.
const MENU_INDENT: f32 = 14.0;
/// Keep a multi-item picker compact even when an application publishes a
/// sentence (or a reverse-DNS identifier) as its title.
const SELECTOR_LABEL_CHARS: usize = 18;

/// Reconnect ladder for a session bus that is down or restarting.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A host that lived this long was healthy: the next failure starts the ladder
/// over instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// The object path the spec asks an item to use, and the fallback when the
/// watcher cannot say where one actually is.
const ITEM_OBJECT: &str = "/StatusNotifierItem";

/// The live host: the `system-tray` client plus a bus of our own to call items
/// back on.
///
/// `Debug` is deliberately terse. The bar logs every message it handles, and
/// both of these print their entire internal state — a `zbus::Connection`
/// alone dumps its whole match-rule table.
#[derive(Clone)]
pub struct Host {
    client: Arc<Client>,
    connection: zbus::Connection,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Host")
    }
}

/// Addresses travel as `Arc<str>`. `view` builds three messages per item on
/// every frame, and the bar redraws on every message it handles, so an owned
/// bus name here is an allocation per item per frame for a string that is set
/// once when the item registers and never edited afterwards.
#[derive(Debug, Clone)]
pub enum Event {
    /// The host came up.
    Connected(Host),
    Disconnected,
    Added(Arc<str>, Box<StatusNotifierItem>),
    Updated(Arc<str>, UpdateEvent),
    Removed(Arc<str>),
    /// Left click: the item's default activation.
    Activate(Arc<str>),
    /// Middle click: the item's secondary activation.
    Secondary(Arc<str>),
    /// Right click: show this item's menu in the popup.
    OpenMenu(Arc<str>),
    /// Popup selector: switch which item's menu is shown.
    Select(Arc<str>),
    /// Popup: expand or collapse a submenu.
    ToggleSubmenu(i32),
    /// Popup: a menu entry was clicked.
    MenuClick { address: Arc<str>, id: i32 },
    /// Result of a request, so a failure is visible instead of silent.
    Requested(Result<(), String>),
}

/// One tray item, with its icons already resolved to handles. Resolving on
/// every frame would mean an icon-theme lookup per item per frame, so the
/// handles are rebuilt only when the properties behind them change.
#[derive(Debug)]
struct Item {
    /// Unique bus name the item lives on; also its identity here.
    address: Arc<str>,
    id: String,
    title: Option<String>,
    status: Status,
    icon_name: Option<String>,
    icon_pixmap: Option<Vec<IconPixmap>>,
    attention_icon_name: Option<String>,
    attention_icon_pixmap: Option<Vec<IconPixmap>>,
    overlay_icon_name: Option<String>,
    overlay_icon_pixmap: Option<Vec<IconPixmap>>,
    theme_path: Option<String>,
    tooltip: Option<String>,
    item_is_menu: bool,
    menu_path: Option<Arc<str>>,
    menu: Option<TrayMenu>,
    handle: Option<icon::Handle>,
    overlay: Option<icon::Handle>,
}

#[derive(Debug, Default)]
pub struct State {
    client: Option<Arc<Client>>,
    connection: Option<zbus::Connection>,
    /// Items in the order they registered, which is the order waybar used.
    items: Vec<Item>,
    /// Address of the item whose menu the popup shows.
    selected: Option<Arc<str>>,
    /// Submenu ids the popup has expanded.
    expanded: HashSet<i32>,
    error: Option<String>,
}

impl State {
    /// One host for the whole session, popup or not: an item that registers
    /// while the popup is closed still has to appear in the bar. The popup
    /// being open changes nothing about what has to be watched, so `open` is
    /// unused here.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::run(stream).map(event_message)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Connected(Host { client, connection }) => {
                // A fresh host means a fresh watcher registration: drop items
                // from the previous session and let the replay refill them.
                self.items.clear();
                self.selected = None;
                self.error = None;
                self.client = Some(client);
                self.connection = Some(connection);
                Task::none()
            }
            Event::Disconnected => {
                self.client = None;
                self.connection = None;
                self.items.clear();
                self.selected = None;
                self.expanded.clear();
                Task::none()
            }
            Event::Added(address, item) => {
                let mut item = Item::new(address, *item);
                item.resolve(ICON_TARGET.load(std::sync::atomic::Ordering::Relaxed));
                match self.index_of(&item.address) {
                    // Re-registration on the same bus name: keep the slot.
                    Some(index) => self.items[index] = item,
                    None => self.items.push(item),
                }
                Task::none()
            }
            Event::Updated(address, update) => {
                let target = ICON_TARGET.load(std::sync::atomic::Ordering::Relaxed);
                if let Some(index) = self.index_of(&address) {
                    self.items[index].merge(update, target);
                }
                Task::none()
            }
            Event::Removed(address) => {
                self.items.retain(|item| item.address != address);
                if self.selected.as_deref() == Some(&*address) {
                    self.selected = None;
                    self.expanded.clear();
                    // The popup was showing a menu that no longer exists.
                    return Task::done(cosmic::Action::App(Message::ClosePopup));
                }
                Task::none()
            }
            Event::Activate(address) => match self.item(&address) {
                // "The item only support the context menu": the spec asks a
                // host to show the menu rather than activate. With a DBusMenu
                // that is our popup; without one, all we can do is ask the app
                // to put its own menu up.
                Some(item) if item.item_is_menu => match item.menu_path.is_some() {
                    true => self.open_menu(address),
                    false => self.item_call(address, ItemCall::ContextMenu),
                },
                Some(_) => self.item_call(address, ItemCall::Activate),
                None => Task::none(),
            },
            Event::Secondary(address) => self.item_call(address, ItemCall::Secondary),
            Event::OpenMenu(address) => self.open_menu(address),
            Event::Select(address) => {
                // The active selector stays pressable so its accent style is
                // not replaced by iced's disabled style. Treating that styling
                // click as a real selection would collapse open submenus.
                if self
                    .target()
                    .is_some_and(|item| item.address == address)
                {
                    return Task::none();
                }
                self.expanded.clear();
                self.selected = Some(address.clone());
                self.about_to_show(address, 0)
            }
            Event::ToggleSubmenu(id) => {
                if !self.expanded.insert(id) {
                    self.expanded.remove(&id);
                    return Task::none();
                }
                match self.selected.clone() {
                    Some(address) => self.about_to_show(address, id),
                    None => Task::none(),
                }
            }
            Event::MenuClick { address, id } => {
                let Some(menu_path) = self.menu_path(&address) else {
                    return Task::none();
                };
                // The menu acted; leaving it open would be a stale menu.
                self.request(ActivateRequest::MenuItem {
                    address: address.to_string(),
                    menu_path: menu_path.to_string(),
                    submenu_id: id,
                })
                .chain(Task::done(cosmic::Action::App(Message::ClosePopup)))
            }
            Event::Requested(result) => {
                self.error = result.err();
                Task::none()
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if self.items.is_empty() {
            return None;
        }
        let size = icon_size(ctx);
        // The lookups inside `Item::resolve` are sized, so tell the resolver
        // what the bar actually renders at before the next item arrives.
        ICON_TARGET.store(u32::from(size), std::sync::atomic::Ordering::Relaxed);

        let mut row = widget::Row::new()
            .spacing(8)
            .align_y(Alignment::Center)
            .height(Length::Fixed(f32::from(size)));
        for item in &self.items {
            row = row.push(item.bar_view(size, ctx));
        }
        Some(row.into())
    }

    /// The popup is the selected item's menu. It exists whenever an item does,
    /// so the bar cell is consistently clickable.
    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        self.target().is_some()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let item = self.target()?;
        let mut card = Card::new();

        // One popup walks every item's menu, so settle the active item before
        // rendering any item-specific content. Keeping every chip enabled is
        // intentional: iced's disabled styling would otherwise mute the
        // accent on the current item and make the selection ambiguous.
        if self.items.len() > 1 {
            let mut selector = widget::Row::new().spacing(popup::ROW_GAP);
            for candidate in &self.items {
                let current = candidate.address == item.address;
                selector = selector.push(popup::chip(
                    candidate.selector_label(),
                    match current {
                        true => Chip::Accent,
                        false => Chip::Plain,
                    },
                    ctx,
                    Some(event_message(Event::Select(candidate.address.clone()))),
                ));
            }
            // Wrapping retains access to every item without letting the picker
            // determine the card's width.
            card = card.block(selector.wrap());
        }

        let mut header = popup::lines().push(popup::title(item.label(), ctx));
        if let Some(tooltip) = &item.tooltip {
            header = header.push(popup::detail(tooltip.as_str(), ctx));
        }
        card = card.block(header);

        let entries: &[MenuItem] = item
            .menu
            .as_ref()
            .map(|menu| menu.submenus.as_slice())
            .unwrap_or_default();
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        self.menu_rows(&item.address, entries, 0, ctx, &mut rows);
        let has_menu = !rows.is_empty();
        if has_menu {
            // An app's menu is as long as the app decided; scrolling it is what
            // keeps the header and the verbs below on screen instead of letting
            // a twenty-entry menu run past the popup's edge and be clipped.
            card = card.list(
                rows.into_iter()
                    .fold(popup::column(), |list, row| list.push(row)),
            );
        }

        // Naming the active item in the footer keeps these item-level methods
        // attached to their target even below a long, scrolling menu.
        let mut context = popup::lines().push(popup::detail(item.label(), ctx));
        if !has_menu {
            context = context.push(popup::detail("no menu", ctx));
        }
        card = card.block(popup::split(
            context,
            [
                popup::chip(
                    "activate",
                    Chip::Accent,
                    ctx,
                    Some(event_message(Event::Activate(item.address.clone()))),
                ),
                popup::chip(
                    "secondary",
                    Chip::Plain,
                    ctx,
                    Some(event_message(Event::Secondary(item.address.clone()))),
                ),
            ],
        ));

        Some(
            card.maybe(self.error.as_ref().map(|error| {
                popup::detail(error.as_str(), ctx)
                    .class(cosmic::theme::Text::Color(ctx.palette.red))
            }))
            .build(),
        )
    }

    /// Nothing here changes per second.
    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    fn index_of(&self, address: &str) -> Option<usize> {
        self.items.iter().position(|item| &*item.address == address)
    }

    fn item(&self, address: &str) -> Option<&Item> {
        self.items.iter().find(|item| &*item.address == address)
    }

    /// The item the popup is about: the selected one, else the first.
    fn target(&self) -> Option<&Item> {
        self.selected
            .as_deref()
            .and_then(|address| self.item(address))
            .or_else(|| self.items.first())
    }

    fn menu_path(&self, address: &str) -> Option<Arc<str>> {
        self.item(address).and_then(|item| item.menu_path.clone())
    }

    /// Select an item and put its menu on screen. The bar owns the popup, so
    /// the surface is closed first and reopened: that is correct whether or
    /// not one was already up, and never leaves the two out of step.
    fn open_menu(&mut self, address: Arc<str>) -> Task<Message> {
        self.expanded.clear();
        self.selected = Some(address.clone());
        self.about_to_show(address, 0)
            .chain(Task::done(cosmic::Action::App(Message::ClosePopup)))
            .chain(Task::done(cosmic::Action::App(Message::Control(
                crate::control::Command::Toggle(crate::modules::ModuleId::Tray),
            ))))
    }

    /// `AboutToShow` lets an app fill a menu lazily; the layout it publishes
    /// afterwards arrives as a normal `Menu` update.
    fn about_to_show(&self, address: Arc<str>, id: i32) -> Task<Message> {
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        let Some(menu_path) = self.menu_path(&address) else {
            return Task::none();
        };
        Task::future(async move {
            let result = client
                .about_to_show_menuitem(address.to_string(), menu_path.to_string(), id)
                .await;
            cosmic::Action::App(event_message(Event::Requested(
                result.map(|_| ()).map_err(|error| error.to_string()),
            )))
        })
    }

    fn request(&self, request: ActivateRequest) -> Task<Message> {
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        Task::future(async move {
            cosmic::Action::App(event_message(Event::Requested(
                client
                    .activate(request)
                    .await
                    .map_err(|error| error.to_string()),
            )))
        })
    }

    /// Call one of the item's own methods.
    ///
    /// `system-tray` cannot do this for us: it hands out the item's bus name
    /// but drops the object path, and its own activate helper then assumes
    /// `/StatusNotifierItem`. Every libappindicator/ayatana item — which is
    /// most of them — actually lives at
    /// `/org/ayatana/NotificationItem/<id>`, so that call fails with
    /// `UnknownObject`. The watcher knows the real path, so ask it.
    fn item_call(&self, address: Arc<str>, call: ItemCall) -> Task<Message> {
        let Some(connection) = self.connection.clone() else {
            return Task::none();
        };
        Task::future(async move {
            let result = async {
                let path = item_path(&connection, &address).await;
                let item = TrayItemProxy::builder(&connection)
                    .destination(address.to_string())?
                    .path(path)?
                    .build()
                    .await?;
                // Coordinates are a hint for where the app should put a window
                // of its own. A layer-shell bar has no screen coordinates to
                // offer, and every implementation treats them as optional.
                match call {
                    ItemCall::Activate => item.activate(0, 0).await,
                    ItemCall::Secondary => item.secondary_activate(0, 0).await,
                    ItemCall::ContextMenu => item.context_menu(0, 0).await,
                }
            }
            .await;
            cosmic::Action::App(event_message(Event::Requested(
                result.map_err(|error: zbus::Error| error.to_string()),
            )))
        })
    }

    /// Flatten one level of the `DBusMenu` tree into rows, recursing into the
    /// submenus the user has expanded.
    fn menu_rows<'a>(
        &'a self,
        address: &Arc<str>,
        entries: &'a [MenuItem],
        depth: usize,
        ctx: &Ctx,
        rows: &mut Vec<Element<'a, Message>>,
    ) {
        let palette = ctx.palette;
        let indent = MENU_INDENT * depth as f32;

        for entry in entries {
            if !entry.visible {
                continue;
            }
            // A separator divides one group of entries from the next within the
            // menu, which is a single block of the card: the card's own
            // hairlines mark where the menu ends, and cannot say this.
            if entry.menu_type == MenuType::Separator {
                rows.push(
                    widget::divider::horizontal::default()
                        .apply(widget::container)
                        .padding([0.0, indent])
                        .into(),
                );
                continue;
            }

            let has_submenu = !entry.submenu.is_empty()
                || entry.children_display.as_deref() == Some("submenu");
            let expanded = has_submenu && self.expanded.contains(&entry.id);
            let color = match (entry.enabled, entry.disposition) {
                (false, _) => palette.overlay0,
                (true, Disposition::Alert) => palette.red,
                (true, Disposition::Warning) => palette.yellow,
                (true, Disposition::Informative) => palette.blue,
                (true, Disposition::Normal) => palette.fg(),
            };

            let mut label = widget::Row::new()
                .spacing(popup::ROW_GAP)
                .align_y(Alignment::Center);
            if indent > 0.0 {
                label = label.push(widget::space::horizontal().width(Length::Fixed(indent)));
            }
            if let Some(mark) = toggle_mark(entry) {
                label = label.push(
                    crate::theme::icon_text(mark)
                        .size(ctx.body())
                        .class(cosmic::theme::Text::Color(color)),
                );
            }
            label = label.push(
                popup::item(mnemonic(entry.label.as_deref().unwrap_or("")), ctx)
                    .class(cosmic::theme::Text::Color(color)),
            );

            let mut actions: Vec<Element<'a, Message>> = Vec::new();
            if let Some(shortcut) = shortcut(entry) {
                actions.push(popup::detail(shortcut, ctx).into());
            }
            if has_submenu {
                actions.push(
                    crate::theme::icon_text(match expanded {
                        true => SUBMENU_OPEN,
                        false => SUBMENU_CLOSED,
                    })
                    .size(ctx.body())
                    .class(cosmic::theme::Text::Color(color))
                    .into(),
                );
            }

            let press = match (entry.enabled, has_submenu) {
                (false, _) => None,
                (true, true) => Some(event_message(Event::ToggleSubmenu(entry.id))),
                (true, false) => Some(event_message(Event::MenuClick {
                    address: address.clone(),
                    id: entry.id,
                })),
            };
            rows.push(popup::row(popup::split(label, actions), palette, press));

            if expanded {
                self.menu_rows(address, &entry.submenu, depth + 1, ctx, rows);
            }
        }
    }
}

impl Item {
    fn new(address: Arc<str>, item: StatusNotifierItem) -> Self {
        Self {
            address,
            id: item.id,
            title: item.title,
            status: item.status,
            icon_name: item.icon_name,
            icon_pixmap: item.icon_pixmap,
            attention_icon_name: item.attention_icon_name,
            attention_icon_pixmap: item.attention_icon_pixmap,
            overlay_icon_name: item.overlay_icon_name,
            overlay_icon_pixmap: item.overlay_icon_pixmap,
            theme_path: item.icon_theme_path,
            tooltip: item.tool_tip.as_ref().and_then(tooltip_text),
            item_is_menu: item.item_is_menu,
            menu_path: item.menu.map(Arc::from),
            menu: None,
            handle: None,
            overlay: None,
        }
    }

    /// A property update. Only the icon-bearing ones re-resolve handles.
    fn merge(&mut self, update: UpdateEvent, target: u32) {
        match update {
            UpdateEvent::Title(title) => self.title = title,
            UpdateEvent::Tooltip(tooltip) => {
                self.tooltip = tooltip.as_ref().and_then(tooltip_text);
            }
            UpdateEvent::Status(status) => {
                self.status = status;
                self.resolve(target);
            }
            UpdateEvent::Icon {
                icon_name,
                icon_pixmap,
            } => {
                self.icon_name = icon_name;
                self.icon_pixmap = icon_pixmap;
                self.resolve(target);
            }
            UpdateEvent::AttentionIcon(name) => {
                self.attention_icon_name = name;
                self.resolve(target);
            }
            UpdateEvent::OverlayIcon(name) => {
                self.overlay_icon_name = name;
                self.resolve(target);
            }
            UpdateEvent::Menu(menu) => self.menu = Some(menu),
            UpdateEvent::MenuDiff(diffs) => self.apply_menu_diff(&diffs),
            // The menu moved to a different bus name; the layout follows.
            UpdateEvent::MenuConnect(_) => {}
        }
    }

    /// Apply property updates to the menu tree.
    ///
    /// Not `system_tray::data::apply_menu_diffs`: that walks the top-level
    /// items and the diff list in lockstep and drops every diff whose order
    /// disagrees with the menu's own - a radio group turning its previous
    /// member off arrives as exactly that, `[7 -> on, 6 -> off]` - and it never
    /// descends into submenus. Match on ids over the whole tree instead.
    fn apply_menu_diff(&mut self, diffs: &[MenuDiff]) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        for diff in diffs {
            if let Some(item) = find_menu_item(&mut menu.submenus, diff.id) {
                let update = &diff.update;
                if let Some(label) = &update.label {
                    item.label.clone_from(label);
                }
                if let Some(enabled) = update.enabled {
                    item.enabled = enabled;
                }
                if let Some(visible) = update.visible {
                    item.visible = visible;
                }
                if let Some(icon_name) = &update.icon_name {
                    item.icon_name.clone_from(icon_name);
                }
                if let Some(icon_data) = &update.icon_data {
                    item.icon_data.clone_from(icon_data);
                }
                if let Some(toggle_state) = update.toggle_state {
                    item.toggle_state = toggle_state;
                }
                if let Some(disposition) = update.disposition {
                    item.disposition = disposition;
                }
                // A removed property means "back to the default", which for
                // everything the bar draws is the item's initial value.
                for property in &diff.remove {
                    match property.as_str() {
                        "label" => item.label = None,
                        "enabled" => item.enabled = true,
                        "visible" => item.visible = true,
                        "icon-name" => item.icon_name = None,
                        "icon-data" => item.icon_data = None,
                        "toggle-state" => item.toggle_state = ToggleState::Indeterminate,
                        "disposition" => item.disposition = Disposition::Normal,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Rebuild the icon handles from the current properties.
    fn resolve(&mut self, target: u32) {
        let attention = self.status == Status::NeedsAttention;
        self.handle = None;
        if attention {
            self.handle = self
                .named(self.attention_icon_name.as_deref(), target)
                .or_else(|| pixmap_handle(self.attention_icon_pixmap.as_deref(), target));
        }
        if self.handle.is_none() {
            self.handle = self
                .named(self.icon_name.as_deref(), target)
                .or_else(|| pixmap_handle(self.icon_pixmap.as_deref(), target));
        }
        self.overlay = self
            .named(self.overlay_icon_name.as_deref(), target)
            .or_else(|| pixmap_handle(self.overlay_icon_pixmap.as_deref(), target));
    }

    /// Resolve an XDG icon name, honouring the item's own `IconThemePath`.
    /// `None` when nothing was found, so the caller can fall back to a pixmap
    /// instead of rendering libcosmic's empty placeholder.
    fn named(&self, name: Option<&str>, target: u32) -> Option<icon::Handle> {
        let name = name.filter(|name| !name.is_empty())?;
        // Some apps put an absolute path in IconName.
        if name.starts_with('/') {
            let path = Path::new(name);
            return path.is_file().then(|| icon::from_path(path.to_owned()));
        }
        let size = u16::try_from(target).unwrap_or(u16::MAX);
        let mut extra: Vec<PathBuf> = Vec::new();
        if let Some(dir) = self.theme_path.as_deref().filter(|dir| !dir.is_empty()) {
            // IconThemePath is a theme root, but plenty of toolkits point it at
            // a flat directory of files; try both.
            extra.push(PathBuf::from(dir));
            for ext in ["png", "svg"] {
                let flat = Path::new(dir).join(format!("{name}.{ext}"));
                if flat.is_file() {
                    return Some(icon::from_path(flat));
                }
            }
        }

        // Resolve the name ourselves rather than through `icon::from_name`:
        // that goes via one process-global theme which libcosmic resets to
        // COSMIC's own on every toolkit-config update, so outside a COSMIC
        // session every tray icon silently becomes a COSMIC stand-in. This is
        // the same lookup crate underneath, and it handles `Inherits`,
        // hicolor and `/usr/share/pixmaps` itself.
        let mut lookup = freedesktop_icons::lookup(name)
            .with_theme(ICON_THEME.as_str())
            .with_size(size)
            .with_cache();
        if !extra.is_empty() {
            lookup = lookup.with_extra_paths(&extra);
        }
        let path = lookup.find()?;
        log::debug!("tray icon {name} -> {}", path.display());
        Some(icon::from_path(path))
    }

    /// Prefer application-authored, human-facing metadata over the stable id.
    /// The tooltip's first segment is its plain-text title; the remaining
    /// segments are useful detail in the header, but too verbose for a picker.
    fn label(&self) -> &str {
        let title = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let tooltip_title = self
            .tooltip
            .as_deref()
            .and_then(|tooltip| tooltip.split(" · ").next())
            .map(str::trim)
            .filter(|title| !title.is_empty());

        if let Some(title) = title.filter(|title| !looks_like_identifier(title)) {
            return title;
        }
        if let Some(title) = tooltip_title {
            return title;
        }
        if let Some(title) = title {
            return title;
        }

        let id = self.id.trim();
        id.rsplit(['.', '/'])
            .find(|part| !part.is_empty())
            .unwrap_or("tray item")
    }

    fn selector_label(&self) -> Cow<'_, str> {
        elide(self.label(), SELECTOR_LABEL_CHARS)
    }

    /// Just the icon, with the passive dimming and attention badge waybar
    /// expressed as `-gtk-icon-effect`.
    fn visual(&self, size: u16, ctx: &Ctx) -> Element<'_, Message> {
        let passive = self.status == Status::Passive;
        let base: Element<'_, Message> = match &self.handle {
            // Handles are reference counted, so this clone is a refcount bump.
            Some(handle) => icon::icon(handle.clone())
                .size(size)
                .opacity(if passive { 0.45 } else { 1.0 })
                .into(),
            None => crate::theme::icon_text(UNKNOWN_ICON)
                .class(cosmic::theme::Text::Color(if passive {
                    ctx.palette.overlay0
                } else {
                    ctx.palette.muted()
                }))
                .into(),
        };

        let badge: Option<Element<'_, Message>> = match (&self.overlay, self.status) {
            (Some(overlay), _) => Some(
                icon::icon(overlay.clone())
                    .size((size * 2 / 3).max(8))
                    .into(),
            ),
            (None, Status::NeedsAttention) => Some(
                crate::theme::icon_text(ATTENTION_BADGE)
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(ctx.palette.red))
                    .into(),
            ),
            (None, _) => None,
        };

        let Some(badge) = badge else {
            return base;
        };
        cosmic::iced::widget::Stack::new()
            .push(base)
            .push(
                badge
                    .apply(widget::container)
                    .width(Length::Fixed(f32::from(size)))
                    .height(Length::Fixed(f32::from(size)))
                    .align_x(Alignment::End)
                    .align_y(Alignment::End),
            )
            .into()
    }

    /// The bar cell for this item. The mouse area sits inside whatever the bar
    /// wraps the module in, and iced offers events to children first, so these
    /// clicks win over the cell's own popup toggle.
    fn bar_view(&self, size: u16, ctx: &Ctx) -> Element<'_, Message> {
        widget::mouse_area(self.visual(size, ctx))
            .on_press(event_message(Event::Activate(self.address.clone())))
            .on_middle_press(event_message(Event::Secondary(self.address.clone())))
            .on_right_press(event_message(Event::OpenMenu(self.address.clone())))
            .into()
    }
}

/// Icon edge the bar currently renders at. `Item::resolve` runs in `update`,
/// which has no `Ctx`, and a sized lookup picks a better file out of the
/// theme, so `view` publishes the size it is drawing at.
static ICON_TARGET: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(18);

fn icon_size(ctx: &Ctx) -> u16 {
    ((ctx.height as f32 * ICON_FRACTION).round() as u16).clamp(8, 64)
}

/// The item with this id anywhere in the tree. Menus are a handful of entries
/// deep, so a walk is cheaper than keeping an index in sync with every diff.
fn find_menu_item(items: &mut [MenuItem], id: i32) -> Option<&mut MenuItem> {
    for item in items {
        if item.id == id {
            return Some(item);
        }
        if let Some(found) = find_menu_item(&mut item.submenu, id) {
            return Some(found);
        }
    }
    None
}

/// Pick the pixmap closest to the size we draw at, preferring one at least
/// that big, and turn ARGB32 in network byte order into the RGBA iced wants.
fn pixmap_handle(pixmaps: Option<&[IconPixmap]>, target: u32) -> Option<icon::Handle> {
    let best = pixmaps?
        .iter()
        .filter(|pixmap| {
            pixmap.width > 0
                && pixmap.height > 0
                && pixmap.pixels.len()
                    >= (pixmap.width as usize).saturating_mul(pixmap.height as usize) * 4
        })
        .min_by_key(|pixmap| {
            let width = pixmap.width as u32;
            (width < target, width.abs_diff(target))
        })?;

    let width = best.width as u32;
    let height = best.height as u32;
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for argb in best.pixels.chunks_exact(4).take((width * height) as usize) {
        rgba.extend_from_slice(&[argb[1], argb[2], argb[3], argb[0]]);
    }
    Some(icon::from_raster_pixels(width, height, rgba))
}

/// `DBusMenu` labels carry GTK-style mnemonics: `__` is a literal underscore,
/// any other underscore marks the following character and is not drawn.
fn mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(ch) = chars.next() {
        if ch != '_' {
            out.push(ch);
        } else if chars.clone().next() == Some('_') {
            chars.next();
            out.push('_');
        }
    }
    out
}

fn toggle_mark(entry: &MenuItem) -> Option<&'static str> {
    let on = entry.toggle_state == ToggleState::On;
    match entry.toggle_type {
        ToggleType::Checkmark => Some(if on { CHECK_ON } else { CHECK_OFF }),
        ToggleType::Radio => Some(if on { RADIO_ON } else { RADIO_OFF }),
        ToggleType::CannotBeToggled => None,
    }
}

/// `[["Control", "S"]]` reads as `Control+S`; several chords join with a comma.
fn shortcut(entry: &MenuItem) -> Option<String> {
    let chords = entry.shortcut.as_ref()?;
    let text = chords
        .iter()
        .filter(|keys| !keys.is_empty())
        .map(|keys| keys.join("+"))
        .collect::<Vec<_>>()
        .join(", ");
    (!text.is_empty()).then_some(text)
}

/// The tooltip structure carries a title and a body in Pango markup; the bar
/// shows plain text, and the title alone is usually the useful half.
fn tooltip_text(tooltip: &Tooltip) -> Option<String> {
    let mut parts = Vec::new();
    if !tooltip.title.is_empty() {
        parts.push(tooltip.title.clone());
    }
    let body = strip_markup(&tooltip.description);
    if !body.is_empty() && Some(&body) != parts.first() {
        parts.push(body);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Drop Pango tags and unescape the entities Pango requires. Tooltips are the
/// one SNI field that is markup, and the bar renders plain text.
fn strip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .trim()
        .replace('\n', " ")
}

/// Titles without word separators that look like protocol identifiers are less
/// useful than an item's tooltip title. Human titles, including single words,
/// remain authoritative.
fn looks_like_identifier(text: &str) -> bool {
    if text.chars().any(char::is_whitespace) {
        return false;
    }
    text.starts_with('/')
        || text.starts_with(':')
        || text.contains('/')
        || text.matches('_').count() >= 2
        || text.matches('.').count() >= 2
}

/// Cap a selector label by characters rather than bytes, preserving UTF-8.
fn elide(text: &str, limit: usize) -> Cow<'_, str> {
    if text.chars().count() <= limit {
        return Cow::Borrowed(text);
    }
    let end = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(index, _)| index);
    Cow::Owned(format!("{}…", text[..end].trim_end()))
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Tray(event))
}

/// Which of an item's own methods to call.
#[derive(Debug, Clone, Copy)]
enum ItemCall {
    Activate,
    Secondary,
    ContextMenu,
}

/// The item side of the `StatusNotifierItem` protocol. Only the three methods
/// a bar invokes; every property already comes through `system-tray`.
#[zbus::proxy(interface = "org.kde.StatusNotifierItem", assume_defaults = false)]
trait TrayItem {
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;
}

/// The watcher's registry, which is the only authority on where an item's
/// object actually lives.
#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait Watcher {
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;
}

/// The object path an item registered under, defaulting to the path the spec
/// asks for when the watcher cannot be reached.
///
/// Registered entries are `<bus name>` or the non-conforming
/// `<bus name><object path>` that libappindicator sends. One call per click is
/// cheaper than mirroring the registry, and cannot go stale.
async fn item_path(connection: &zbus::Connection, address: &str) -> String {
    let registered = async {
        WatcherProxy::builder(connection)
            // Read it once; a cache here would add a match rule for a value
            // we only look at on a click.
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?
            .registered_status_notifier_items()
            .await
    }
    .await;
    let Ok(registered) = registered else {
        return ITEM_OBJECT.to_owned();
    };
    registered
        .iter()
        .filter_map(|entry| match entry.split_once('/') {
            Some((name, path)) if name == address => Some(format!("/{path}")),
            None if entry == address => Some(ITEM_OBJECT.to_owned()),
            _ => None,
        })
        .next()
        .unwrap_or_else(|| ITEM_OBJECT.to_owned())
}

/// A host that re-registers itself if the session bus or the watcher restarts.
fn stream() -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(16, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            if let Err(error) = session(&mut sender).await {
                log::debug!("tray host ended: {error:#}");
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

/// One host's worth of events. `Client::new` publishes our own
/// `StatusNotifierWatcher` (deferring to an existing owner) and registers us
/// as a host on it.
async fn session(
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Event>,
) -> anyhow::Result<()> {
    let client = Arc::new(Client::new().await?);
    // `Client` keeps its own connection private, and the bar needs one to call
    // items back on.
    let connection = zbus::Connection::session().await?;
    // Subscribe before reading the current items, so an item that registers
    // during the handover is seen exactly once.
    let mut events = client.subscribe();
    if sender
        .send(Event::Connected(Host {
            client: client.clone(),
            connection,
        }))
        .await
        .is_err()
    {
        return Ok(());
    }
    let known: Vec<(Arc<str>, StatusNotifierItem, Option<TrayMenu>)> = client
        .items()
        .lock()
        .map(|items| {
            items
                .iter()
                .map(|(address, (item, menu))| {
                    (Arc::from(address.as_str()), item.clone(), menu.clone())
                })
                .collect()
        })
        .unwrap_or_default();
    for (address, item, menu) in known {
        if sender
            .send(Event::Added(address.clone(), Box::new(item)))
            .await
            .is_err()
        {
            return Ok(());
        }
        if let Some(menu) = menu {
            let _ = sender
                .send(Event::Updated(address, UpdateEvent::Menu(menu)))
                .await;
        }
    }

    loop {
        match events.recv().await {
            Ok(ClientEvent::Add(address, item)) => {
                if sender
                    .send(Event::Added(address.into(), item))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            Ok(ClientEvent::Update(address, update)) => {
                if sender
                    .send(Event::Updated(address.into(), update))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            Ok(ClientEvent::Remove(address)) => {
                if sender.send(Event::Removed(address.into())).await.is_err() {
                    return Ok(());
                }
            }
            // A slow frame let the broadcast buffer overflow; the items we
            // still hold are stale, so start the session over.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                anyhow::bail!("missed {missed} tray events");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

/// The icon theme tray icons are resolved against, read once from the GTK
/// settings the rest of this desktop uses. Falls back to `hicolor`, which the
/// lookup treats as the end of every theme's inheritance chain anyway.
static ICON_THEME: LazyLock<String> = LazyLock::new(|| {
    let theme = gtk_icon_theme().unwrap_or_else(|| "hicolor".to_owned());
    log::debug!("tray icons: resolving against icon theme {theme}");
    theme
});

fn gtk_icon_theme() -> Option<String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    for version in ["gtk-4.0", "gtk-3.0"] {
        let path = base.join(version).join("settings.ini");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("gtk-icon-theme-name") {
                let value = value.trim_start().strip_prefix('=')?.trim();
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}
