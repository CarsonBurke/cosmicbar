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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Background, Color, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};
use niri_ipc::socket::Socket;
use niri_ipc::{
    Action, Event as NiriEvent, Request, Window, WindowLayout, Workspace, WorkspaceReferenceArg,
};

use crate::bar::Message;
use crate::config::TaskbarScope;
use crate::modules::{Ctx, ModuleEvent};
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
    /// Every open window, keyed by id; ordering is derived in `view`.
    windows: HashMap<u64, Window>,
    /// In niri's own order: grouped per output, by index.
    workspaces: Vec<Workspace>,
    /// Which windows the strip lists. Reloading the config replaces the bar's
    /// `Config` without rebuilding module state, so this arrives as an event.
    /// app_id -> resolved icon file. Resolved once per app_id when the window
    /// arrives, never per frame: the freedesktop lookup touches the disk.
    icons: HashMap<String, Option<PathBuf>>,
    connected: bool,
}

impl State {
    /// The compositor's event stream already carries every window on every
    /// workspace, which is exactly what the popup lists: there is no extra
    /// source to gate on `open`.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::run(stream)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
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
        Task::none()
    }

    /// The strip's windows for this frame's output, in `taskbar_scope`'s
    /// scope. `None` hides the module, so an output with nothing open on it
    /// costs no bar space.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let palette = ctx.palette;
        let windows = self.strip(ctx.output.as_deref(), ctx.taskbar_scope);
        if windows.is_empty() {
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
            None => windows.len(),
        };
        let hidden = windows.len().saturating_sub(shown);

        let mut row = widget::Row::new()
            .spacing(ITEM_SPACING)
            .align_y(Alignment::Center);

        for window in windows.into_iter().take(shown) {
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

    /// Every window on this output, grouped by workspace: the switcher waybar
    /// could not draw. Rows focus, and each has its own close button.
    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        true
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let palette = ctx.palette;
        let icon_size = icon_size(ctx);
        let mut body = widget::Column::new().spacing(6).width(Length::Fill);
        let mut total = 0usize;

        for workspace in self.row(ctx.output.as_deref()) {
            let windows = self.ordered(workspace.id);
            if windows.is_empty() {
                continue;
            }
            total += windows.len();

            let heading = match &workspace.name {
                Some(name) => format!("{} · {name}", workspace.idx),
                None => format!("workspace {}", workspace.idx),
            };
            let heading_color = if workspace.is_active {
                palette.accent()
            } else {
                palette.muted()
            };
            body = body.push(
                crate::theme::text(heading)
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(heading_color))
                    .align_y(Alignment::Center)
                    .apply(widget::button::custom)
                    .width(Length::Fill)
                    .padding([2.0, 4.0])
                    .class(ghost_class(palette, heading_color, palette.accent()))
                    .on_press(event_message(Event::FocusWorkspace(workspace.id))),
            );

            for window in windows {
                let mut lines = widget::Column::new()
                    .push(
                        crate::theme::text(elide(title_of(window), POPUP_TITLE_LIMIT))
                            .size(ctx.font_size),
                    )
                    .spacing(1)
                    .width(Length::Fill);
                if let Some(app_id) = window.app_id.as_deref() {
                    lines = lines.push(
                        crate::theme::text(app_id.to_string())
                            .size(ctx.small())
                            .class(cosmic::theme::Text::Color(palette.overlay0)),
                    );
                }

                let entry = widget::Row::new()
                    .push(self.icon(window, icon_size))
                    .push(lines)
                    .push_maybe(window.is_urgent.then(|| {
                        crate::theme::text(URGENT_ICON)
                            .size(ctx.font_size)
                            .class(cosmic::theme::Text::Color(palette.red))
                    }))
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .apply(widget::button::custom)
                    .width(Length::Fill)
                    .padding([3.0, 6.0])
                    // A popup row is a menu item: it highlights at once, the
                    // way every menu does, so only the bar's own strip fades.
                    .class({
                        let (text_color, background) = item_colors(
                            palette,
                            window.is_focused,
                            window.is_urgent,
                            self.connected,
                        );
                        button_class(palette, text_color, background, text_color)
                    })
                    .on_press(event_message(Event::Focus {
                        id: window.id,
                        from_popup: true,
                    }));

                body = body.push(
                    widget::Row::new()
                        .push(entry)
                        .push(
                            crate::theme::text(CLOSE_ICON)
                                .size(ctx.font_size)
                                .apply(widget::button::custom)
                                .padding([3.0, 7.0])
                                .class(ghost_class(palette, palette.overlay0, palette.red))
                                .on_press(event_message(Event::Close(window.id))),
                        )
                        .spacing(4)
                        .align_y(Alignment::Center),
                );
            }
        }

        if total == 0 {
            body = body.push(
                crate::theme::text("no windows on this output")
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(palette.muted())),
            );
        }
        if !self.connected {
            body = body.push(
                crate::theme::text("reconnecting to niri…")
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(palette.peach)),
            );
        }

        Some(body.apply(widget::container).padding(12).into())
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// The workspaces of one output, top to bottom by index. niri's table
    /// order is unspecified — `Request::Workspaces` really does hand back
    /// 3, 2, 1 — so the order is imposed here. An unknown output name means
    /// "everything", so a compositor that never told us an output name still
    /// gets a usable bar.
    fn row(&self, output: Option<&str>) -> Vec<&Workspace> {
        let mut row: Vec<&Workspace> = self
            .workspaces
            .iter()
            .filter(|workspace| {
                output.is_none_or(|output| workspace.output.as_deref() == Some(output))
            })
            .collect();
        row.sort_unstable_by(|a, b| a.output.cmp(&b.output).then(a.idx.cmp(&b.idx)));
        row
    }

    /// The workspace currently on screen for this output. With no output name
    /// the focused workspace is the only sensible answer.
    fn visible_workspace(&self, output: Option<&str>) -> Option<&Workspace> {
        match output {
            Some(_) => {
                let row = self.row(output);
                row.iter()
                    .find(|workspace| workspace.is_active)
                    .or_else(|| row.iter().find(|workspace| workspace.is_focused))
                    .copied()
            }
            None => self.workspaces.iter().find(|workspace| workspace.is_focused),
        }
    }

    /// The windows the strip lists, in the order it lists them: the whole
    /// output workspace by workspace, or only what that output is showing.
    /// An empty workspace contributes nothing, so a gap in the workspace
    /// numbering costs nothing either.
    fn strip(&self, output: Option<&str>, scope: TaskbarScope) -> Vec<&Window> {
        match scope {
            TaskbarScope::Output => self
                .row(output)
                .into_iter()
                .flat_map(|workspace| self.ordered(workspace.id))
                .collect(),
            TaskbarScope::Workspace => self
                .visible_workspace(output)
                .map(|workspace| self.ordered(workspace.id))
                .unwrap_or_default(),
        }
    }

    /// The windows of one workspace, left to right as the compositor lays them
    /// out: by column then row, with floating windows trailing.
    fn ordered(&self, workspace: u64) -> Vec<&Window> {
        let mut windows: Vec<&Window> = self
            .windows
            .values()
            .filter(|window| window.workspace_id == Some(workspace))
            .collect();
        windows.sort_unstable_by_key(|window| {
            let (column, tile) = window
                .layout
                .pos_in_scrolling_layout
                // Floating windows have no slot in the scrolling layout; they
                // go after the tiled ones instead of jumping to the front.
                .unwrap_or((usize::MAX, usize::MAX));
            (column, tile, window.id)
        });
        windows
    }

    /// Resolve and remember this window's app icon. Called when window data
    /// arrives, so `view` never touches the filesystem.
    fn learn_icon(&mut self, window: &Window) {
        let Some(app_id) = window.app_id.as_deref() else {
            return;
        };
        if self.icons.contains_key(app_id) {
            return;
        }
        self.icons.insert(app_id.to_string(), lookup_icon(app_id));
    }

    /// The themed app icon, or a letter tile when the app has no icon.
    /// Now that the strip is icons only, this is the whole item: it must always
    /// produce something clickable, even for a window with no app_id at all.
    fn icon(&self, window: &Window, size: u16) -> Element<'_, Message> {
        let app_id = window.app_id.as_deref().unwrap_or_default();
        match self.icons.get(app_id) {
            Some(Some(path)) => widget::icon::icon(widget::icon::from_path(path.clone()))
                .size(size)
                .into(),
            _ => {
                let letter = app_id
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();
                crate::theme::text(letter)
                    .size(f32::from(size) * 0.75)
                    .apply(widget::container)
                    .width(Length::Fixed(f32::from(size)))
                    .height(Length::Fixed(f32::from(size)))
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center)
                    .into()
            }
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
        } else if in_entry {
            if let Some(icon) = line.strip_prefix("Icon=") {
                let icon = icon.trim();
                if !icon.is_empty() {
                    return Some(icon.to_string());
                }
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
        if claims {
            if let Some(icon) = entry_icon(&path) {
                return Some(icon);
            }
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

/// A borderless button that only colours its label: workspace headings and the
/// close buttons in the popup.
fn ghost_class(palette: Palette, text_color: Color, hover_color: Color) -> cosmic::theme::Button {
    button_class(palette, text_color, None, hover_color)
}

fn button_class(
    palette: Palette,
    text_color: Color,
    background: Option<Color>,
    hover_color: Color,
) -> cosmic::theme::Button {
    let style = move |background: Option<Color>, text_color: Color| cosmic::widget::button::Style {
        shadow_offset: cosmic::iced::Vector::ZERO,
        background: background.map(Background::Color),
        overlay: None,
        border_radius: ITEM_RADIUS.into(),
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: Some(text_color),
        text_color: Some(text_color),
    };
    // A filled item keeps its own colour under the pointer; an unfilled one
    // lights up, which is the affordance the waybar `:hover` rules gave. The
    // lift is measured from the island the strip sits on.
    let island = ISLAND.color(&palette).unwrap_or_else(|| palette.bar_bg());
    let hovered = background.or(Some(palette.hover_over(island)));
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| style(background, text_color)),
        hovered: Box::new(move |_focused, _theme| style(hovered, hover_color)),
        pressed: Box::new(move |_focused, _theme| style(
            background.or(Some(palette.press_over(island))),
            hover_color,
        )),
        disabled: Box::new(move |_theme| style(background, text_color)),
    }
}

fn elide(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
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
