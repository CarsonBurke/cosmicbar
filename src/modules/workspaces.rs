//! Per-output workspace pills, driven by niri's IPC event stream.
//!
//! waybar could only draw `●`/`○` and had no idea which workspaces held
//! windows. Here the compositor pushes the whole workspace table plus window
//! placement, so a pill can say *empty*, *occupied*, *visible*, *focused* and
//! *urgent* apart, a click focuses that workspace, and scrolling over the
//! module walks the workspaces of the output the pointer is on.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Background, Border, Color, Shadow, Subscription, mouse};
use cosmic::widget;
use cosmic::{Apply, Element};
use niri_ipc::socket::Socket;
use niri_ipc::{Action, Event as NiriEvent, Request, Workspace, WorkspaceReferenceArg};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::theme::{Island, Palette};

/// mantle, the same role the waybar `#workspaces` island had.
pub const ISLAND: Island = Island::Start;

/// Reconnect ladder for a compositor that is restarting or not there yet.
const RECONNECT_BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// A stream that lasted this long was healthy: the next failure restarts the
/// ladder instead of inheriting an old outage's delay.
const STABLE_SESSION: Duration = Duration::from_secs(60);
/// Pill corner radius; the pills sit inside the island so they stay tighter.
const PILL_RADIUS: f32 = 9.0;
/// The waybar `format-icons` this module inherits (`●` for the workspace you
/// are on, `○` for the rest), drawn instead of typed: a glyph sits where its
/// font's metrics put it, which at bar size is visibly off-centre. Sized from
/// the bar's text so it tracks `font_size` the way the glyph did.
const DOT_RATIO: f32 = 0.42;
const DOT_MIN: f32 = 5.0;
/// Ring thickness for a workspace you are not on.
const DOT_RING: f32 = 1.5;
/// Space around a dot inside its own cell, so the hover and active fills read as
/// a circle centred in the bar instead of a bar-tall oval.
const DOT_PAD: f32 = 5.0;
/// A named workspace keeps waybar's capsule: room for text on both sides.
const NAME_PAD_Y: f32 = 2.0;
const NAME_PAD_X: f32 = 6.0;
/// A touchpad can emit dozens of scroll events per flick; one workspace step
/// per this window keeps a flick from crossing the whole monitor.
const SCROLL_INTERVAL: Duration = Duration::from_millis(180);

#[derive(Debug, Clone)]
pub enum Event {
    /// Full workspace table: this replaces everything we knew.
    Workspaces(Arc<Vec<Workspace>>),
    /// A workspace became the visible one on its output.
    Activated { id: u64, focused: bool },
    Urgency { id: u64, urgent: bool },
    /// Window placements, `(window, workspace)`: all a pill needs of the
    /// window table.
    Windows(Arc<Vec<(u64, u64)>>),
    /// A window opened or moved; only its workspace matters here.
    WindowPlaced { id: u64, workspace: Option<u64> },
    WindowClosed(u64),
    /// The event stream went away; the subscription is retrying.
    Disconnected,
    /// A pill was clicked.
    Focus(u64),
    /// The pointer scrolled over the module on `output`. The name is the one
    /// the pill row was keyed by, so the frame that built the closure did not
    /// have to copy it.
    Cycle {
        output: Option<Arc<str>>,
        forward: bool,
    },
    /// Result of an action, so a refusal reaches the log instead of vanishing.
    Acted(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    /// As niri sends them; `rebuild` imposes the per-output display order.
    workspaces: Vec<Workspace>,
    /// window id -> workspace id, the only window fact a pill needs.
    placement: HashMap<u64, u64>,
    /// How many windows each workspace holds, folded out of `placement` as it
    /// changes. A workspace drops out when its last window does, so a pill
    /// asks whether its id is present rather than scanning the window table:
    /// a frame happens on every message the bar handles, not just on a niri
    /// event, and there can be a few dozen windows.
    occupancy: HashMap<u64, usize>,
    /// Every pill in display order, one output's after another's. Derived in
    /// `update` for the same reason: a cpu sample and a clock tick both draw a
    /// frame, and neither of them can have changed this.
    pills: Vec<Pill>,
    /// Where each output's pills sit in `pills`. The key is also the name the
    /// scroll closure has to send back, so the closure shares it instead of
    /// being handed a copy every frame.
    outputs: HashMap<Arc<str>, Range<usize>>,
    connected: bool,
    /// Rate limit for pointer scrolling, see [`SCROLL_INTERVAL`].
    last_cycle: Option<Instant>,
}

impl State {
    /// The compositor's event stream is the module's only source and it is
    /// cheap: one connection, no polling, so nothing is gated on the popup
    /// (there isn't one).
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::run(stream)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Workspaces(workspaces) => {
                self.workspaces = Arc::unwrap_or_clone(workspaces);
                self.connected = true;
                self.rebuild();
            }
            Event::Activated { id, focused } => {
                // niri's contract: activating a workspace deactivates every
                // other workspace on the same output, and a focused workspace
                // is the only focused one anywhere.
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
                self.rebuild();
            }
            Event::Urgency { id, urgent } => {
                if let Some(workspace) = self.workspaces.iter_mut().find(|w| w.id == id) {
                    workspace.is_urgent = urgent;
                }
                self.rebuild();
            }
            Event::Windows(placement) => {
                self.placement = placement.iter().copied().collect();
                self.occupancy.clear();
                for workspace in self.placement.values() {
                    *self.occupancy.entry(*workspace).or_default() += 1;
                }
            }
            Event::WindowPlaced { id, workspace } => self.place(id, workspace),
            Event::WindowClosed(id) => self.place(id, None),
            Event::Disconnected => self.connected = false,
            Event::Focus(id) => {
                return focus(WorkspaceReferenceArg::Id(id));
            }
            Event::Cycle { output, forward } => {
                let now = Instant::now();
                if self
                    .last_cycle
                    .is_some_and(|last| now.duration_since(last) < SCROLL_INTERVAL)
                {
                    return Task::none();
                }
                // Scrolling walks the output under the pointer, which is not
                // necessarily the focused one, so the step is resolved here
                // and sent as an absolute id.
                let focused = self
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.is_focused)
                    .and_then(|workspace| workspace.output.as_deref());
                let row = self.row(output.as_deref().or(focused));
                let Some(current) = row.iter().position(|pill| pill.is_active) else {
                    return Task::none();
                };
                // No wrap-around, matching niri's own focus-workspace-up and
                // -down binds.
                let step = if forward {
                    current.saturating_add(1).min(row.len().saturating_sub(1))
                } else {
                    current.saturating_sub(1)
                };
                if step == current {
                    return Task::none();
                }
                let target = row[step].id;
                self.last_cycle = Some(now);
                return focus(WorkspaceReferenceArg::Id(target));
            }
            // No popup to show it in, so a refusal goes to the log and the
            // next event stream snapshot restores the truth on screen.
            Event::Acted(Err(error)) => log::warn!("workspaces: {error}"),
            Event::Acted(Ok(())) => {}
        }
        Task::none()
    }

    /// One pill per workspace on this frame's output. `None` hides the module
    /// while niri has not told us anything yet, so no empty island shows up.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let palette = ctx.palette;
        // One lookup yields both the row and the output name the scroll
        // closure sends back, so a frame costs a refcount bump rather than a
        // copy of the name and a sort of the table.
        let (output, pills) = match ctx.output.as_deref() {
            Some(name) => {
                let (name, range) = self.outputs.get_key_value(name)?;
                (Some(name.clone()), &self.pills[range.start..range.end])
            }
            None => (None, self.pills.as_slice()),
        };
        if pills.is_empty() {
            return None;
        }

        let mut row = widget::Row::new().spacing(3).align_y(Alignment::Center);
        for pill in pills {
            let style = Style::of(pill, self.occupancy.contains_key(&pill.id), self.connected);
            // Dots, like the waybar module this replaces (`format-icons`
            // active `●` / default `○`), but drawn rather than typed: a glyph
            // sits wherever its font puts it, and at bar size that is visibly
            // off-centre. A named workspace still shows its name — that is why
            // it was named.
            let text_color = style.colors(palette).0;
            let cell = match &pill.name {
                // A named workspace is a capsule around its text, so it keeps
                // waybar's pill shape and fills the island's height.
                Some(name) => crate::fill::fill(
                    crate::theme::text(name.as_str())
                        .size(ctx.font_size)
                        .align_y(Alignment::Center)
                        .apply(widget::button::custom)
                        .padding([NAME_PAD_Y, NAME_PAD_X])
                        .class(crate::theme::cell(text_color, [PILL_RADIUS; 4]))
                        .on_press(event_message(Event::Focus(pill.id))),
                    style.fill(palette),
                    [PILL_RADIUS; 4],
                ),
                // A dot is lit by a circle centred in the bar, not by its own
                // cell: the cell is as tall as the bar, so filling it would
                // draw an oval around a round dot. Clicks still belong to the
                // whole cell, which is why the dot is not simply padded.
                None => crate::fill::spot(
                    dot(style, palette, ctx)
                        .apply(widget::button::custom)
                        .padding([0.0, DOT_PAD])
                        .class(crate::theme::cell(text_color, [0.0; 4]))
                        .on_press(event_message(Event::Focus(pill.id))),
                    style.fill(palette),
                    dot_size(ctx) + 2.0 * DOT_PAD,
                ),
            };
            row = row.push(cell);
        }

        // Scrolling belongs to the whole strip, not to one pill; the pills
        // capture clicks before this ever sees them.
        Some(
            widget::mouse_area(row)
                .on_scroll(move |delta| {
                    event_message(Event::Cycle {
                        output: output.clone(),
                        // Wheel up goes to the previous workspace, matching
                        // the direction niri's own scroll binds use.
                        forward: scrolled_down(delta),
                    })
                })
                .into(),
        )
    }

    /// Workspaces are pills, so there is nothing left for a popup to add.
    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        false
    }

    pub fn popup(&self, _ctx: &Ctx) -> Option<Element<'_, Message>> {
        None
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// Fold the workspace table into the row each output draws. niri's table
    /// order is unspecified — `Request::Workspaces` really does hand back 3,
    /// 2, 1 — so the order is imposed here, once per event that can change it,
    /// instead of by every frame.
    fn rebuild(&mut self) {
        let mut sorted: Vec<&Workspace> = self.workspaces.iter().collect();
        sorted.sort_unstable_by(|a, b| a.output.cmp(&b.output).then(a.idx.cmp(&b.idx)));
        // That order leaves each output's workspaces contiguous, so a row is a
        // slice of the whole thing rather than a second copy of its pills.
        self.outputs.clear();
        let mut start = 0;
        for group in sorted.chunk_by(|a, b| a.output == b.output) {
            let end = start + group.len();
            // A workspace niri has not put on an output has no row of its own;
            // only a bar with no output name ever draws it.
            if let Some(output) = group[0].output.as_deref() {
                self.outputs.insert(Arc::from(output), start..end);
            }
            start = end;
        }
        self.pills = sorted.into_iter().map(Pill::of).collect();
    }

    /// Move one window between workspaces. `placement` still says where it
    /// was, so keeping `occupancy` true costs a decrement and an increment.
    fn place(&mut self, window: u64, workspace: Option<u64>) {
        let previous = match workspace {
            Some(workspace) => self.placement.insert(window, workspace),
            None => self.placement.remove(&window),
        };
        if let Some(previous) = previous
            && let Some(count) = self.occupancy.get_mut(&previous)
        {
            *count -= 1;
            if *count == 0 {
                self.occupancy.remove(&previous);
            }
        }
        if let Some(workspace) = workspace {
            *self.occupancy.entry(workspace).or_default() += 1;
        }
    }

    /// The pills one bar surface draws, left to right. An unknown output name
    /// means "everything", so a compositor that never told us an output name
    /// still gets a usable bar.
    fn row(&self, output: Option<&str>) -> &[Pill] {
        match output {
            Some(output) => self
                .outputs
                .get(output)
                .map_or(&[][..], |range| &self.pills[range.start..range.end]),
            None => &self.pills,
        }
    }
}

/// One workspace as a pill: everything drawing it needs, and nothing that
/// would make a frame go back to the workspace table for it.
#[derive(Debug)]
struct Pill {
    id: u64,
    name: Option<String>,
    is_active: bool,
    is_focused: bool,
    is_urgent: bool,
}

impl Pill {
    fn of(workspace: &Workspace) -> Self {
        Self {
            id: workspace.id,
            name: workspace.name.clone(),
            is_active: workspace.is_active,
            is_focused: workspace.is_focused,
            is_urgent: workspace.is_urgent,
        }
    }
}

/// How a pill is painted. Ordered by precedence: urgency wins over focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Urgent,
    Focused,
    Visible,
    Occupied,
    Empty,
    /// The compositor is unreachable: keep the shape, drop the colour.
    Stale,
}

impl Style {
    fn of(pill: &Pill, occupied: bool, connected: bool) -> Self {
        if !connected {
            Self::Stale
        } else if pill.is_urgent {
            Self::Urgent
        } else if pill.is_focused {
            Self::Focused
        } else if pill.is_active {
            Self::Visible
        } else if occupied {
            Self::Occupied
        } else {
            Self::Empty
        }
    }

    /// Foreground and background. The island already paints mantle, so an
    /// unfilled pill is transparent rather than mantle-coloured.
    fn colors(self, palette: Palette) -> (Color, Option<Color>) {
        match self {
            // waybar's CSS could only recolour the label; a filled pill reads
            // at a glance from across the screen.
            Self::Urgent => (palette.crust, Some(palette.red)),
            Self::Focused => (palette.crust, Some(palette.accent())),
            Self::Visible => (palette.accent(), Some(palette.surface1)),
            Self::Occupied => (palette.fg(), Some(palette.surface0)),
            Self::Empty => (palette.muted(), None),
            Self::Stale => (palette.overlay0, None),
        }
    }

    /// A solid dot means "there is something here": the workspace you are on,
    /// one visible on another output, or one holding windows. An empty
    /// workspace is a ring, which is what waybar's `○` said.
    fn filled(self) -> bool {
        !matches!(self, Self::Empty | Self::Stale)
    }

    /// A filled pill keeps its own colour under the pointer; an unfilled one
    /// lights up, which is the affordance waybar's `:hover` gave. The lift is
    /// measured from the island the pills sit on.
    fn fill(self, palette: Palette) -> crate::fill::Fill {
        let background = self.colors(palette).1;
        let island = ISLAND.color(&palette).unwrap_or_else(|| palette.bar_bg());
        crate::fill::Fill {
            base: background,
            over: background.or(Some(palette.hover_over(island))),
            pressed: background.or(Some(palette.press_over(island))),
        }
    }
}

/// One workspace dot: a circle the bar draws itself, so it is centred on the
/// pill rather than wherever the font's baseline happens to fall.
/// The dot's diameter, from the bar's text size: it tracks `font_size` the way
/// the glyph it replaces did.
fn dot_size(ctx: &Ctx) -> f32 {
    (ctx.font_size * DOT_RATIO).round().max(DOT_MIN)
}

fn dot<'a>(style: Style, palette: Palette, ctx: &Ctx) -> Element<'a, Message> {
    let color = style.colors(palette).0;
    let size = dot_size(ctx);
    let filled = style.filled();
    widget::space::horizontal()
        .width(size)
        .height(size)
        .apply(widget::container)
        .class(cosmic::theme::Container::custom(move |_theme| {
            cosmic::widget::container::Style {
                text_color: None,
                background: filled.then_some(Background::Color(color)),
                border: Border {
                    // Half the box is a circle for any renderer that clamps the
                    // radius, which is every one of them.
                    radius: (size / 2.0).into(),
                    width: if filled { 0.0 } else { DOT_RING },
                    color,
                },
                shadow: Shadow::default(),
                icon_color: None,
                snap: false,
            }
        }))
        .into()
}

fn scrolled_down(delta: mouse::ScrollDelta) -> bool {
    let y = match delta {
        mouse::ScrollDelta::Lines { y, .. } => y,
        // Pixel deltas come from touchpads; the sign is all we need.
        mouse::ScrollDelta::Pixels { y, .. } => y,
    };
    y < 0.0
}

/// Ask niri to focus a workspace. Runs off the UI thread and reports back.
fn focus(reference: WorkspaceReferenceArg) -> Task<Message> {
    Task::future(async move {
        cosmic::Action::App(event_message(Event::Acted(
            act(Action::FocusWorkspace { reference }).await,
        )))
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
/// the full workspace and window state, so there is nothing to request first.
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

/// Keep only the events a workspace pill can change on.
fn project(event: NiriEvent) -> Option<Event> {
    match event {
        NiriEvent::WorkspacesChanged { workspaces } => Some(Event::Workspaces(Arc::new(workspaces))),
        NiriEvent::WorkspaceActivated { id, focused } => Some(Event::Activated { id, focused }),
        NiriEvent::WorkspaceUrgencyChanged { id, urgent } => Some(Event::Urgency { id, urgent }),
        NiriEvent::WindowsChanged { windows } => Some(Event::Windows(Arc::new(
            windows
                .into_iter()
                .filter_map(|window| Some((window.id, window.workspace_id?)))
                .collect(),
        ))),
        NiriEvent::WindowOpenedOrChanged { window } => Some(Event::WindowPlaced {
            id: window.id,
            workspace: window.workspace_id,
        }),
        NiriEvent::WindowClosed { id } => Some(Event::WindowClosed(id)),
        _ => None,
    }
}

/// Wrap a module event for the bar's message type.
fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Workspaces(event))
}
