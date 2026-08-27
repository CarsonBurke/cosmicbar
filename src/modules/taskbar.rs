//! Taskbar: this output's windows, from niri's event stream.
//!
//! waybar's `wlr/taskbar` could activate a window and nothing else, and it only
//! ever knew about the toplevels the foreign-toplevel protocol handed it. niri
//! pushes the whole window table with workspace placement and focus, so the
//! strip lists every window on this output in workspace order — or only the
//! visible workspace's, under `taskbar_scope = "workspace"` — with left click
//! to focus and middle click to close, while the popup always lists *every*
//! window on this output grouped by workspace: a window switcher waybar has no
//! way to draw.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Color, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};
use niri_ipc::socket::Socket;
use niri_ipc::{
    Action, Event as NiriEvent, Request, Window, WindowLayout, Workspace, WorkspaceReferenceArg,
};

use crate::bar::Message;
use crate::config::TaskbarScope;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::{Island, Palette};

/// base, the role the waybar `#taskbar` island borrowed from `@backlight`.
pub const ISLAND: Island = Island::Start;

/// Reconnect ladder for a compositor that is restarting or not there yet.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A stream that lasted this long was healthy: the next failure restarts the
/// ladder instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// `fa-xmark`, for the per-window close buttons in the popup.
const CLOSE_ICON: &str = "\u{f00d}";
/// `md-bell_outline`, for a window asking for attention.
const URGENT_ICON: &str = "\u{f009c}";
/// The letter tile for a window that has no app_id at all, and so no letter of
/// its own to draw.
const UNKNOWN_LETTER: &str = "?";
/// Longest window title in the popup, which is much wider.
const POPUP_TITLE_LIMIT: usize = 44;
/// Item corner radius; items sit inside the island so they stay tighter.
const ITEM_RADIUS: f32 = 9.0;
/// Horizontal padding inside one item, and the gap between two.
const ITEM_PAD_X: f32 = 4.0;
const ITEM_SPACING: f32 = 6.0;
/// The largest share of the bar's width the strip may take before the rest of
/// its windows collapse into a `+N`.
const STRIP_SHARE: f32 = 0.3;

#[derive(Debug, Clone)]
pub enum Event {
    /// Full window table: this replaces everything we knew.
    Windows(Arc<Vec<Window>>),
    /// A window opened, or any of its properties changed.
    Changed(Arc<Window>),
    Closed(u64),
    /// Focus moved; every other window is no longer focused.
    FocusChanged(Option<u64>),
    Urgency { id: u64, urgent: bool },
    /// Tile positions moved, which is the taskbar's left-to-right order.
    Layouts(Arc<Vec<(u64, WindowLayout)>>),
    /// Full workspace table, for the output/workspace grouping.
    Workspaces(Arc<Vec<Workspace>>),
    WorkspaceActivated { id: u64, focused: bool },
    /// The event stream went away; the subscription is retrying.
    Disconnected,
    /// A window was clicked: focus it (and leave the popup, if we are in one).
    Focus { id: u64, from_popup: bool },
    Close(u64),
    /// A workspace heading in the popup was clicked.
    FocusWorkspace(u64),
    /// Result of an action, so a refusal is visible instead of silent.
    Acted(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    /// Every open window, keyed by id. `order` holds the sequence they draw in.
    windows: HashMap<u64, Window>,
    /// In niri's own order: grouped per output, by index.
    workspaces: Vec<Workspace>,
    /// The strip's and the popup's draw order, one entry per output.
    order: Vec<OutputOrder>,
    /// app_id -> what its windows draw as. Resolved once per app_id when the
    /// window arrives, never per frame: the freedesktop lookup touches the disk.
    icons: HashMap<String, AppIcon>,
    connected: bool,
}

/// One output's workspaces, in the order the bar lists them.
///
/// The order is derived from both niri tables and kept rather than recomputed,
/// because a bar surface redraws on every message the bar handles — a cpu
/// sample, a clock tick, a pointer leaving a cell — and none of those can move
/// a window.
#[derive(Debug)]
struct OutputOrder {
    /// `None` from a compositor that never named its outputs.
    name: Option<String>,
    workspaces: Vec<WorkspaceOrder>,
}

#[derive(Debug)]
struct WorkspaceOrder {
    id: u64,
    /// The popup's heading for it, formatted here because it follows the
    /// workspace table and not the frame.
    heading: String,
    /// Which workspace its output is showing, and which one has the keyboard:
    /// what `taskbar_scope = "workspace"` picks with.
    active: bool,
    focused: bool,
    /// Its windows by id, left to right as the compositor lays them out.
    windows: Vec<u64>,
}

/// What one app_id draws as, in the form the widget takes.
///
/// A frame draws every visible item, so anything left undone here — rebuilding
/// a handle from a path, uppercasing a letter — is work per item per frame.
#[derive(Debug)]
enum AppIcon {
    /// A theme or desktop-entry icon file. `icon::Handle` is what
    /// `widget::icon::icon` consumes, and for the SVGs an icon theme is made
    /// of, cloning one is a refcount bump.
    File(widget::icon::Handle),
    /// The app has no icon anywhere, so its windows are letter tiles instead.
    Letter(Box<str>),
}

impl State {
    /// The compositor's event stream already carries every window on every
    /// workspace, which is exactly what the popup lists: there is no extra
    /// source to gate on `open`.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::run(stream)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        // Which events can move a window or a workspace, and so invalidate the
        // derived order. Focus and urgency only repaint what is already placed.
        let reorder = matches!(
            event,
            Event::Windows(_)
                | Event::Changed(_)
                | Event::Closed(_)
                | Event::Layouts(_)
                | Event::Workspaces(_)
                | Event::WorkspaceActivated { .. }
        );
        match event {
            Event::Windows(windows) => {
                self.windows.clear();
                for window in windows.iter() {
                    self.learn_icon(window);
                    self.windows.insert(window.id, window.clone());
                }
                self.connected = true;
            }
            Event::Changed(window) => {
                self.learn_icon(&window);
                let window = Arc::unwrap_or_clone(window);
                if window.is_focused {
                    // niri's contract: a focused window unfocuses every other.
                    for other in self.windows.values_mut() {
                        other.is_focused = false;
                    }
                }
                self.windows.insert(window.id, window);
                self.connected = true;
            }
            Event::Closed(id) => {
                self.windows.remove(&id);
            }
            Event::FocusChanged(id) => {
                for window in self.windows.values_mut() {
                    window.is_focused = Some(window.id) == id;
                }
            }
            Event::Urgency { id, urgent } => {
                if let Some(window) = self.windows.get_mut(&id) {
                    window.is_urgent = urgent;
                }
            }
            Event::Layouts(changes) => {
                for (id, layout) in changes.iter() {
                    if let Some(window) = self.windows.get_mut(id) {
                        window.layout = layout.clone();
                    }
                }
            }
            Event::Workspaces(workspaces) => {
                self.workspaces = Arc::unwrap_or_clone(workspaces);
                self.connected = true;
            }
            Event::WorkspaceActivated { id, focused } => {
                let output = self
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == id)
                    .and_then(|workspace| workspace.output.clone());
                for workspace in &mut self.workspaces {
                    if workspace.output == output {
                        workspace.is_active = workspace.id == id;
                    }
                    if focused {
                        workspace.is_focused = workspace.id == id;
                    }
                }
            }
            Event::Disconnected => self.connected = false,
            Event::Focus { id, from_popup } => {
                let focus = act_task(Action::FocusWindow { id });
                // Picking a window out of the list is the end of that
                // interaction; leaving the list open over the window you just
                // raised would be wrong.
                return if from_popup {
                    Task::batch([
                        focus,
                        Task::done(cosmic::Action::App(Message::ClosePopup)),
                    ])
                } else {
                    focus
                };
            }
            // The popup stays open: closing several windows in a row is the
            // point, and the list reflows from the event stream as they go.
            Event::Close(id) => return act_task(Action::CloseWindow { id: Some(id) }),
            Event::FocusWorkspace(id) => {
                return Task::batch([
                    act_task(Action::FocusWorkspace {
                        reference: WorkspaceReferenceArg::Id(id),
                    }),
                    Task::done(cosmic::Action::App(Message::ClosePopup)),
                ]);
            }
            // The popup has no room for an error line, and the next event
            // stream update restores the truth on screen anyway.
            Event::Acted(Err(error)) => log::warn!("taskbar: {error}"),
            Event::Acted(Ok(())) => {}
        }
        if reorder {
            self.reorder();
        }
        Task::none()
    }

    /// The strip's windows for this frame's output, in `taskbar_scope`'s
    /// scope. `None` hides the module, so an output with nothing open on it
    /// costs no bar space.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let palette = ctx.palette;
        let windows = self.strip(ctx.output.as_deref(), ctx.taskbar_scope);
        let total = windows.clone().count();
        if total == 0 {
            return None;
        }

        let icon_size = icon_size(ctx);
        // The strip is the one module with no upper bound on its content: a
        // display-wide scope on a busy session is dozens of windows. Left to
        // grow it takes the width every other module needs, and iced answers an
        // over-full row by shortening text - so the clock keeps its glyph and
        // loses its digits. A share of the bar, and the rest is a `+N` that
        // opens the window list.
        let per_item = f32::from(icon_size) + 2.0 * ITEM_PAD_X + ITEM_SPACING;
        let shown = match ctx.width {
            Some(width) => ((width * STRIP_SHARE / per_item).floor() as usize).max(1),
            None => total,
        };
        let hidden = total.saturating_sub(shown);

        let mut row = widget::Row::new()
            .spacing(ITEM_SPACING)
            .align_y(Alignment::Center);

        for window in windows.take(shown).filter_map(|id| self.windows.get(&id)) {
            // Icons only: a title per window is what made the waybar taskbar
            // eat the whole left third of the bar. The title lives in the
            // tooltip and in the popup, where there is room for it.
            let (text_color, background) = item_colors(
                palette,
                window.is_focused,
                window.is_urgent,
                self.connected,
            );
            let item = crate::fill::fill(
                self.icon(window, icon_size)
                    .apply(widget::button::custom)
                    .padding([1.0, ITEM_PAD_X])
                    .class(crate::theme::cell(text_color, [ITEM_RADIUS; 4]))
                    .on_press(event_message(Event::Focus {
                        id: window.id,
                        from_popup: false,
                    })),
                item_fill(palette, background),
                [ITEM_RADIUS; 4],
            );
            // No tooltip: an iced overlay cannot paint outside a 40px-tall
            // layer surface, so the title lives in the popup instead.
            // waybar could only bind middle-click to a shell command; here it
            // is a compositor request with the exact window id.
            row = row.push(
                widget::mouse_area(item)
                    .on_middle_press(event_message(Event::Close(window.id))),
            );
        }

        if hidden > 0 {
            // Not a button: the strip's own cell opens the window list on a
            // click, and this is a label on that cell saying what is missing.
            row = row.push(
                crate::theme::text(format!("+{hidden}"))
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(palette.muted()))
                    .align_y(Alignment::Center),
            );
        }

        Some(row.into())
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        true
    }

    /// Every window on this output, grouped by workspace: the switcher waybar
    /// could not draw. Rows focus, and each has its own close button.
    ///
    /// The list has no upper bound — it is every window on the output — so it
    /// is the card's one scrolling block, headings and all.
    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let palette = ctx.palette;
        let icon_size = icon_size(ctx);
        let mut list = popup::column();
        let mut total = 0usize;

        for workspace in self.workspaces_of(ctx.output.as_deref()) {
            if workspace.windows.is_empty() {
                continue;
            }
            total += workspace.windows.len();
            // A heading is a row like any other because clicking it switches to
            // that workspace.
            list = list.push(popup::row(
                popup::section(workspace.heading.as_str(), ctx),
                palette,
                Some(event_message(Event::FocusWorkspace(workspace.id))),
            ));

            for window in workspace.windows.iter().filter_map(|id| self.windows.get(id)) {
                let mut lines = popup::lines()
                    .push(popup::item(elide(title_of(window), POPUP_TITLE_LIMIT), ctx));
                if let Some(app_id) = window.app_id.as_deref() {
                    lines = lines.push(popup::detail(app_id, ctx));
                }
                let entry = widget::Row::new()
                    .push(self.icon(window, icon_size))
                    .push(lines)
                    .push_maybe(window.is_urgent.then(|| {
                        crate::theme::icon_text(URGENT_ICON)
                            .size(ctx.small())
                            .class(cosmic::theme::Text::Color(palette.red))
                    }))
                    .spacing(popup::ROW_GAP)
                    .align_y(Alignment::Center);
                // Closing sits beside the row rather than in it: a button
                // cannot be nested inside another button's content.
                list = list.push(popup::split(
                    popup::row(
                        entry,
                        palette,
                        Some(event_message(Event::Focus {
                            id: window.id,
                            from_popup: true,
                        })),
                    ),
                    [popup::icon_chip(
                        CLOSE_ICON,
                        Chip::Danger,
                        ctx,
                        Some(event_message(Event::Close(window.id))),
                    )],
                ));
            }
        }

        let mut card = Card::new().block(popup::split(
            popup::title(
                match total {
                    1 => "1 window".to_owned(),
                    total => format!("{total} windows"),
                },
                ctx,
            ),
            [],
        ));
        if total > 0 {
            card = card.list(list);
        }
        Some(
            card.maybe((total == 0).then(|| popup::detail("no windows on this output", ctx)))
                .maybe((!self.connected).then(|| {
                    popup::detail("reconnecting to niri…", ctx)
                        .class(cosmic::theme::Text::Color(palette.peach))
                }))
                .build(),
        )
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// The workspaces a frame draws from, top to bottom by index.
    ///
    /// The output is resolved up front rather than filtered on as the iterator
    /// runs: the popup hands back rows borrowed from `self`, and an iterator
    /// that also held the frame's output name would shorten them to the frame.
    fn workspaces_of<'a>(
        &'a self,
        output: Option<&str>,
    ) -> impl Iterator<Item = &'a WorkspaceOrder> + Clone {
        let groups: &[OutputOrder] = match output {
            // Each output appears once in the order, so its workspaces are one
            // run of it.
            Some(name) => self
                .order
                .iter()
                .position(|group| group.name.as_deref() == Some(name))
                .map_or(&[], |at| &self.order[at..=at]),
            // An unknown output name means "everything", so a compositor that
            // never told us an output name still gets a usable bar.
            None => &self.order,
        };
        groups.iter().flat_map(|group| group.workspaces.iter())
    }

    /// The workspace currently on screen for this output. With no output name
    /// the focused workspace is the only sensible answer.
    fn visible_workspace(&self, output: Option<&str>) -> Option<&WorkspaceOrder> {
        let mut row = self.workspaces_of(output);
        match output {
            // Every output has an active workspace; the focused one is the
            // fallback for a compositor that has not said which.
            Some(_) => row
                .clone()
                .find(|workspace| workspace.active)
                .or_else(|| row.find(|workspace| workspace.focused)),
            None => row.find(|workspace| workspace.focused),
        }
    }

    /// The windows the strip lists, in the order it lists them: the whole
    /// output workspace by workspace, or only what that output is showing.
    /// An empty workspace contributes nothing, so a gap in the workspace
    /// numbering costs nothing either.
    ///
    /// An iterator rather than a `Vec`, because `view` runs on every message
    /// the bar handles and only ever needs this length and its first few ids.
    fn strip(
        &self,
        output: Option<&str>,
        scope: TaskbarScope,
    ) -> impl Iterator<Item = u64> + Clone {
        // `Workspace` scope is `Output` scope narrowed to one workspace, which
        // keeps both scopes on one iterator type. A narrowed scope with no
        // visible workspace matches nothing, which is the empty strip it is.
        let narrow = matches!(scope, TaskbarScope::Workspace);
        let visible = narrow
            .then(|| self.visible_workspace(output).map(|workspace| workspace.id))
            .flatten();
        self.workspaces_of(output)
            .filter(move |workspace| !narrow || Some(workspace.id) == visible)
            .flat_map(|workspace| workspace.windows.iter().copied())
    }

    /// Rebuild the draw order from the window and workspace tables. Called
    /// from `update` for the events that can change it, which is what keeps it
    /// out of every frame.
    fn reorder(&mut self) {
        // One pass over the windows: asking each workspace which windows are on
        // it instead rescans the whole table per workspace.
        let mut placed: HashMap<u64, Vec<(usize, usize, u64)>> = HashMap::new();
        for window in self.windows.values() {
            let Some(workspace) = window.workspace_id else {
                continue;
            };
            let (column, tile) = window
                .layout
                .pos_in_scrolling_layout
                // Floating windows have no slot in the scrolling layout; they
                // go after the tiled ones instead of jumping to the front.
                .unwrap_or((usize::MAX, usize::MAX));
            placed
                .entry(workspace)
                .or_default()
                .push((column, tile, window.id));
        }
        // Left to right as the compositor lays them out: by column, then row,
        // then by id so the order does not wobble between two equal slots.
        for windows in placed.values_mut() {
            windows.sort_unstable();
        }

        // niri's workspace table order is unspecified — `Request::Workspaces`
        // really does hand back 3, 2, 1 — so the row order is imposed here, and
        // the per-output groups then fall out as runs of one output name.
        let mut workspaces: Vec<&Workspace> = self.workspaces.iter().collect();
        workspaces.sort_unstable_by(|a, b| a.output.cmp(&b.output).then(a.idx.cmp(&b.idx)));

        let mut order: Vec<OutputOrder> = Vec::new();
        for workspace in workspaces {
            let group = WorkspaceOrder {
                id: workspace.id,
                heading: match &workspace.name {
                    Some(name) => format!("{} · {name}", workspace.idx),
                    None => format!("workspace {}", workspace.idx),
                },
                active: workspace.is_active,
                focused: workspace.is_focused,
                windows: placed
                    .remove(&workspace.id)
                    .map(|windows| windows.into_iter().map(|(_, _, id)| id).collect())
                    .unwrap_or_default(),
            };
            match order.last_mut() {
                Some(last) if last.name == workspace.output => last.workspaces.push(group),
                _ => order.push(OutputOrder {
                    name: workspace.output.clone(),
                    workspaces: vec![group],
                }),
            }
        }
        self.order = order;
    }

    /// Resolve and remember what this window's app draws as. Called when window
    /// data arrives, so no frame touches the filesystem.
    fn learn_icon(&mut self, window: &Window) {
        let Some(app_id) = window.app_id.as_deref() else {
            return;
        };
        if self.icons.contains_key(app_id) {
            return;
        }
        let icon = match lookup_icon(app_id) {
            Some(path) => AppIcon::File(widget::icon::from_path(path)),
            None => AppIcon::Letter(letter_of(app_id)),
        };
        self.icons.insert(app_id.to_string(), icon);
    }

    /// The themed app icon, or a letter tile when the app has no icon.
    /// Now that the strip is icons only, this is the whole item: it must always
    /// produce something clickable, even for a window with no app_id at all.
    fn icon(&self, window: &Window, size: u16) -> Element<'_, Message> {
        match window
            .app_id
            .as_deref()
            .and_then(|app_id| self.icons.get(app_id))
        {
            Some(AppIcon::File(handle)) => widget::icon::icon(handle.clone()).size(size).into(),
            Some(AppIcon::Letter(letter)) => letter_tile(letter, size),
            // A window with no app_id was never resolved, so it has no letter
            // of its own.
            None => letter_tile(UNKNOWN_LETTER, size),
        }
    }
}

/// Windows without a title still need something to click on.
fn title_of(window: &Window) -> &str {
    match window.title.as_deref() {
        Some(title) if !title.is_empty() => title,
        _ => match window.app_id.as_deref() {
            Some(app_id) if !app_id.is_empty() => app_id,
            _ => "untitled",
        },
    }
}

/// Icons are the item now, so they are sized from the bar's height rather than
/// its text: they fill it, minus the island's own breathing room.
fn icon_size(ctx: &Ctx) -> u16 {
    ((ctx.height as f32 * 0.75).round() as u16).clamp(12, 28)
}

/// A window with no icon still has to be the size of one, so its letter sits in
/// a box as big as the icon it stands in for.
fn letter_tile<'a>(letter: &'a str, size: u16) -> Element<'a, Message> {
    crate::theme::text(letter)
        .size(f32::from(size) * 0.75)
        .apply(widget::container)
        .width(Length::Fixed(f32::from(size)))
        .height(Length::Fixed(f32::from(size)))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
}

/// The letter an app with no icon draws instead. A string rather than a `char`
/// because `char::to_uppercase` is an iterator: one letter can upcase to
/// several.
fn letter_of(app_id: &str) -> Box<str> {
    match app_id.chars().find(|c| c.is_alphanumeric()) {
        Some(first) => first.to_uppercase().collect::<String>().into_boxed_str(),
        None => UNKNOWN_LETTER.into(),
    }
}

/// `Named::path` is the only way to tell a hit from a miss: `handle()` quietly
/// returns an empty SVG for an unknown name, which would render as a hole.
/// app_ids are not icon names, so the obvious rewrites are tried in turn:
/// `org.wezfurlong.wezterm` -> `wezterm`. When no theme icon carries the
/// app_id, the app's own desktop entry is asked: that is the only place an
/// AppImage records its icon, and it records an absolute path
/// (`Icon=/home/…/.icons/orca`, extension optional).
fn lookup_icon(app_id: &str) -> Option<PathBuf> {
    let lower = app_id.to_lowercase();
    let tail = lower.rsplit('.').next().unwrap_or(lower.as_str());
    let mut candidates = vec![app_id.to_string(), lower.clone()];
    if tail != lower {
        candidates.push(tail.to_string());
    }
    candidates.dedup();
    candidates
        .iter()
        .find_map(|name| themed_icon(name))
        .or_else(|| desktop_entry_icon(app_id, &candidates))
}

fn themed_icon(name: &str) -> Option<PathBuf> {
    widget::icon::from_name(name)
        // No fallback: the default one strips `-` segments and can land on
        // a completely unrelated icon.
        .fallback(None)
        .size(24)
        .path()
}

/// The `Icon=` of the desktop entry that claims this app_id. Entries are found
/// by file name first (`orca` -> `orca.desktop`, the common case), then by
/// `StartupWMClass`, which is how an app declares a window class that does not
/// match its file name.
fn desktop_entry_icon(app_id: &str, names: &[String]) -> Option<PathBuf> {
    let icon = desktop_dirs().find_map(|dir| {
        names
            .iter()
            .find_map(|name| entry_icon(&dir.join(format!("{name}.desktop"))))
            .or_else(|| scan_for_wm_class(&dir, app_id))
    })?;
    match icon.starts_with('/') {
        true => icon_file(Path::new(&icon)),
        false => themed_icon(&icon),
    }
}

/// `applications/` under every XDG data directory, in precedence order.
fn desktop_dirs() -> impl Iterator<Item = PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    home.into_iter()
        .chain(
            system
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
        )
        .map(|dir| dir.join("applications"))
}

/// `Icon=` of one desktop entry, from its first (`[Desktop Entry]`) group: a
/// per-action `Icon=` further down belongs to that action, not the app.
fn entry_icon(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            if in_entry {
                return None;
            }
            in_entry = line == "[Desktop Entry]";
        } else if in_entry
            && let Some(icon) = line.strip_prefix("Icon=") {
                let icon = icon.trim();
                if !icon.is_empty() {
                    return Some(icon.to_string());
                }
            }
    }
    None
}

/// One directory scan, only for an app_id no file name matched. The result is
/// cached per app_id by the caller, so this runs once per app.
fn scan_for_wm_class(dir: &Path, app_id: &str) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "desktop") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let claims = text
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("StartupWMClass="))
            .any(|class| class.trim().eq_ignore_ascii_case(app_id));
        if claims
            && let Some(icon) = entry_icon(&path) {
                return Some(icon);
            }
    }
    None
}

/// An absolute `Icon=` may omit the extension, in which case the file next to
/// it decides the format.
fn icon_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    ["png", "svg", "xpm", "jpg"]
        .into_iter()
        .map(|ext| path.with_extension(ext))
        .find(|candidate| candidate.is_file())
}

/// A taskbar item. `#taskbar button.active { background: @hover-bg }` from the
/// waybar CSS, plus an accent label: the bar lifts the whole cell while the
/// pointer is over it, so a fill alone would vanish exactly when you are about
/// to click.
fn item_colors(
    palette: Palette,
    focused: bool,
    urgent: bool,
    connected: bool,
) -> (Color, Option<Color>) {
    if !connected {
        (palette.overlay0, None)
    } else if urgent {
        (palette.crust, Some(palette.red))
    } else if focused {
        (palette.accent(), Some(palette.surface1))
    } else {
        (palette.muted(), None)
    }
}

/// A filled item keeps its own colour under the pointer; an unfilled one lights
/// up, which is the affordance the waybar `:hover` rules gave. The lift is
/// measured from the island the strip sits on.
fn item_fill(palette: Palette, background: Option<Color>) -> crate::fill::Fill {
    let island = ISLAND.color(&palette).unwrap_or_else(|| palette.bar_bg());
    crate::fill::Fill {
        base: background,
        over: background.or(Some(palette.hover_over(island))),
        pressed: background.or(Some(palette.press_over(island))),
    }
}

/// A title as the popup shows it. A `Cow` because the popup is rebuilt on every
/// frame it is open and almost every title already fits: only the long ones pay
/// for a copy.
fn elide(text: &str, limit: usize) -> Cow<'_, str> {
    if text.chars().count() <= limit {
        return Cow::Borrowed(text);
    }
    let mut kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    kept.push('…');
    Cow::Owned(kept)
}

/// Run one niri action off the UI thread and report the outcome back.
fn act_task(action: Action) -> Task<Message> {
    Task::future(async move {
        cosmic::Action::App(event_message(Event::Acted(act(action).await)))
    })
}

/// One short-lived request socket per action. `niri_ipc`'s socket is blocking,
/// so it runs on the blocking pool and never stalls a frame.
async fn act(action: Action) -> Result<(), String> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut socket = Socket::connect().context("connecting to $NIRI_SOCKET")?;
        socket
            .send(Request::Action(action))
            .context("sending action")?
            .map_err(|message| anyhow!("niri refused the action: {message}"))?;
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| format!("{error:#}"))
}

fn stream() -> impl Stream<Item = Message> {
    cosmic::iced::stream::channel(16, async move |mut sender| {
        let mut attempt = 0usize;
        loop {
            let started = Instant::now();
            match session(&mut sender).await {
                Ok(Subscriber::Gone) => return,
                Ok(Subscriber::Live) => log::debug!("niri event stream closed"),
                Err(error) => log::debug!("niri event stream ended: {error:#}"),
            }
            if sender.send(event_message(Event::Disconnected)).await.is_err() {
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

/// Why a session ended: the compositor let go, or the bar did.
enum Subscriber {
    Live,
    Gone,
}

/// One event-stream connection's worth of events. The stream always opens with
/// the full window and workspace state, so there is nothing to request first.
async fn session(
    sender: &mut cosmic::iced::futures::channel::mpsc::Sender<Message>,
) -> anyhow::Result<Subscriber> {
    let (events, mut incoming) = tokio::sync::mpsc::channel::<NiriEvent>(64);
    let reader = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut socket = Socket::connect().context("connecting to $NIRI_SOCKET")?;
        socket
            .send(Request::EventStream)
            .context("requesting the event stream")?
            .map_err(|message| anyhow!("niri refused the event stream: {message}"))?;
        let mut read = socket.read_events();
        while let Ok(event) = read() {
            if events.blocking_send(event).is_err() {
                // The bar stopped listening.
                return Ok(());
            }
        }
        Ok(())
    });

    while let Some(event) = incoming.recv().await {
        let Some(event) = project(event) else { continue };
        if sender.send(event_message(event)).await.is_err() {
            return Ok(Subscriber::Gone);
        }
    }
    reader.await.context("niri reader task")??;
    Ok(Subscriber::Live)
}

/// Keep only the events a window list can change on.
fn project(event: NiriEvent) -> Option<Event> {
    match event {
        NiriEvent::WindowsChanged { windows } => Some(Event::Windows(Arc::new(windows))),
        NiriEvent::WindowOpenedOrChanged { window } => Some(Event::Changed(Arc::new(window))),
        NiriEvent::WindowClosed { id } => Some(Event::Closed(id)),
        NiriEvent::WindowFocusChanged { id } => Some(Event::FocusChanged(id)),
        NiriEvent::WindowUrgencyChanged { id, urgent } => Some(Event::Urgency { id, urgent }),
        NiriEvent::WindowLayoutsChanged { changes } => Some(Event::Layouts(Arc::new(changes))),
        NiriEvent::WorkspacesChanged { workspaces } => Some(Event::Workspaces(Arc::new(workspaces))),
        NiriEvent::WorkspaceActivated { id, focused } => {
            Some(Event::WorkspaceActivated { id, focused })
        }
        _ => None,
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Taskbar(event))
}
