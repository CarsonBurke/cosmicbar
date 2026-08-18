//! The bar application: one wlr-layer-shell surface per output, plus the
//! popup surfaces its modules open.

use std::collections::HashMap;

use cosmic::app::{Core, Task};
use cosmic::cctk::sctk::reexports::client::protocol::wl_output::WlOutput;
use cosmic::cctk::sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use cosmic::iced::event::wayland::{Event as WaylandEvent, OutputEvent};
use cosmic::iced::event::{self, listen_with};
use cosmic::iced::platform_specific::shell::commands::layer_surface::{
    destroy_layer_surface, get_layer_surface,
};
use cosmic::iced::platform_specific::shell::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::runtime::platform_specific::wayland::popup::{SctkPopupSettings, SctkPositioner};
use cosmic::iced::window::Id as SurfaceId;
use cosmic::iced::{Alignment, Length, Limits, Rectangle, Subscription};
use cosmic::widget::rectangle_tracker::{RectangleTracker, RectangleUpdate};
use cosmic::widget::{self, autosize};
use cosmic::{Apply, Element};

use crate::config::Config;
use crate::modules::{self, Ctx, ModuleId, Modules};

/// Popup width bounds; the height follows the content.
const POPUP_MIN_WIDTH: f32 = 280.0;
pub const POPUP_MAX_WIDTH: f32 = 420.0;
const POPUP_MAX_HEIGHT: f32 = 720.0;

static AUTOSIZE_ID: std::sync::LazyLock<cosmic::widget::Id> =
    std::sync::LazyLock::new(|| cosmic::widget::Id::new("cosmicbar-popup"));

#[derive(Debug, Clone)]
pub enum Message {
    /// An output appeared (or its info changed): place a bar on it.
    OutputReady(WlOutput, Option<String>),
    OutputRemoved(WlOutput),
    Rect(RectangleUpdate<TrackedRect>),
    /// A module was clicked on a particular bar surface.
    Toggle(SurfaceId, ModuleId),
    /// The compositor told a bar surface how wide it is. Modules that can grow
    /// without bound - the taskbar - need a budget, or they push the text of
    /// every other module off the bar.
    Sized(SurfaceId, f32),
    ClosePopup,
    Control(crate::control::Command),
    /// Popup lifecycle straight from the compositor.
    PopupEvent(cosmic::iced::event::wayland::PopupEvent, SurfaceId),
    /// A module's own event.
    Module(modules::ModuleEvent),
    /// Wall-clock tick: minute aligned, or per-second while a module asks.
    Tick,
    /// The pointer left a bar surface. iced clears the hovered widget's own
    /// state when it sees this, but a `request_redraw` from that path does not
    /// reach a layer surface, so the stale lit cell stays on screen until
    /// something else repaints. Turning the event into a message is that
    /// something else.
    CursorLeft,
}

impl Message {
    /// Short label for tracing. The full `Debug` of a module event can be a
    /// whole queue snapshot, which makes a debug log unreadable.
    fn label(&self) -> String {
        match self {
            Self::OutputReady(_, name) => {
                format!("OutputReady({})", name.as_deref().unwrap_or("?"))
            }
            Self::OutputRemoved(_) => "OutputRemoved".into(),
            Self::Sized(_, width) => format!("Sized({width})"),
            Self::Rect(RectangleUpdate::Init(_)) => "Rect(Init)".into(),
            Self::Rect(RectangleUpdate::Rectangle(((_, id), rect))) => {
                format!(
                    "Rect({} @ {},{} {}x{})",
                    id.name(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height
                )
            }
            Self::Toggle(_, id) => format!("Toggle({})", id.name()),
            Self::ClosePopup => "ClosePopup".into(),
            Self::Control(command) => format!("Control({command:?})"),
            Self::PopupEvent(event, id) => format!("PopupEvent({event:?}, {id:?})"),
            Self::Module(event) => format!("Module({})", event.label()),
            Self::Tick => "Tick".into(),
            Self::CursorLeft => "CursorLeft".into(),
        }
    }
}

/// Module rects are surface-local, so the surface is part of the key.
pub type TrackedRect = (SurfaceId, ModuleId);

struct BarSurface {
    output: WlOutput,
    name: Option<String>,
    surface: SurfaceId,
    /// Logical width, from the compositor's configure.
    width: Option<f32>,
}

struct Popup {
    id: SurfaceId,
    parent: SurfaceId,
    module: ModuleId,
}

pub struct Bar {
    core: Core,
    now: jiff::Zoned,
    config: Config,
    modules: Modules,
    bars: Vec<BarSurface>,
    popup: Option<Popup>,
    tracker: Option<RectangleTracker<TrackedRect>>,
    rects: HashMap<TrackedRect, Rectangle>,
}

impl cosmic::Application for Bar {
    type Executor = cosmic::executor::Default;
    type Flags = Config;
    type Message = Message;
    const APP_ID: &'static str = "dev.cosmicbar.Bar";

    fn init(core: Core, config: Config) -> (Self, Task<Message>) {
        let mut modules = Modules::default();
        modules.sync_extensions(&config);
        (
            Self {
                core,
                now: jiff::Zoned::now(),
                config,
                modules,
                bars: Vec::new(),
                popup: None,
                tracker: None,
                rects: HashMap::new(),
            },
            Task::none(),
        )
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        log::debug!("update {}", message.label());
        let task = match message {
            Message::OutputReady(output, name) => self.add_bar(output, name),
            Message::OutputRemoved(output) => self.remove_bar(&output),
            Message::Rect(RectangleUpdate::Init(tracker)) => {
                self.tracker = Some(tracker);
                Task::none()
            }
            Message::Rect(RectangleUpdate::Rectangle((key, rect))) => {
                self.rects.insert(key, rect);
                Task::none()
            }
            Message::Toggle(parent, module) => self.toggle_popup(parent, module, true),
            Message::Control(crate::control::Command::Toggle(module)) => {
                // Keybinds have no pointer, so the popup opens on the first bar.
                match self.bars.first().map(|bar| bar.surface) {
                    // No input serial behind a keybind, so no pointer grab:
                    // the compositor would dismiss a grabbing popup at once.
                    Some(parent) => self.toggle_popup(parent, module, false),
                    None => Task::none(),
                }
            }
            Message::Control(crate::control::Command::Close) => self.close_popup(),
            Message::Control(crate::control::Command::Reload) => self.reload(),
            Message::ClosePopup => self.close_popup(),
            Message::PopupEvent(cosmic::iced::event::wayland::PopupEvent::Done, id) => {
                // The compositor already destroyed it; only our state is stale.
                if self.popup.as_ref().is_some_and(|popup| popup.id == id) {
                    self.popup = None;
                }
                Task::none()
            }
            Message::PopupEvent(..) => Task::none(),
            Message::Module(event) => self.modules.update(event),
            Message::Tick => {
                self.now = jiff::Zoned::now();
                Task::none()
            }
            // Nothing to change: the point is the frame this message causes.
            Message::CursorLeft => Task::none(),
            Message::Sized(surface, width) => {
                if let Some(bar) = self.bars.iter_mut().find(|bar| bar.surface == surface) {
                    bar.width = Some(width);
                }
                Task::none()
            }
        };
        // A module's popup can empty out under it — an extension restarting
        // sends a frame with nothing in it — and a module with no popup has no
        // clickable cell either, so nothing on the bar would dismiss the empty
        // card left behind.
        let stale = self
            .popup
            .as_ref()
            .is_some_and(|popup| !self.modules.has_popup(popup.module));
        let task = match stale {
            true => Task::batch([task, self.close_popup()]),
            false => task,
        };
        // Extensions are told which popup is open here rather than at each site
        // that opens or closes one: the compositor dismisses popups on its own,
        // so the only reliable moment is after whatever just happened.
        self.modules
            .set_popup(self.popup.as_ref().map(|popup| popup.module));
        task
    }

    fn subscription(&self) -> Subscription<Message> {
        let open = self.popup.as_ref().map(|popup| popup.module);
        let mut subscriptions = self.modules.subscriptions(&self.config, open);

        subscriptions.push(tick(self.modules.fast_tick(&self.config, open)));

        subscriptions.push(crate::control::subscription());
        subscriptions.push(Config::watch());

        subscriptions.push(
            cosmic::widget::rectangle_tracker::subscription::<_, TrackedRect>("cosmicbar")
                .map(|(_, update)| Message::Rect(update)),
        );

        subscriptions.push(listen_with(|event, _status, id| match event {
            // A layer surface learns its width from the compositor's configure,
            // as `Opened` first and `Resized` on every change after.
            event::Event::Window(
                cosmic::iced::window::Event::Opened { size, .. }
                | cosmic::iced::window::Event::Resized(size),
            ) => Some(Message::Sized(id, size.width)),
            event::Event::PlatformSpecific(event::PlatformSpecific::Wayland(
                WaylandEvent::Output(output_event, output),
            )) => match output_event {
                OutputEvent::Created(info) => Some(Message::OutputReady(
                    output,
                    info.and_then(|info| info.name),
                )),
                OutputEvent::InfoUpdate(info) => Some(Message::OutputReady(output, info.name)),
                OutputEvent::Removed => Some(Message::OutputRemoved(output)),
            },
            // The compositor dismisses a popup on its own when the grab is
            // broken (a click elsewhere, a keybind, focus loss). Without this
            // the bar would still believe the popup is open and the next click
            // on the module would only "close" it.
            event::Event::PlatformSpecific(event::PlatformSpecific::Wayland(
                WaylandEvent::Popup(popup_event, _surface, id),
            )) => Some(Message::PopupEvent(popup_event, id)),
            // A cell lit under the pointer stays lit after the pointer leaves
            // the bar: iced's own `request_redraw` does not reach a layer
            // surface, so the bar repaints on the message instead.
            event::Event::Mouse(cosmic::iced::mouse::Event::CursorLeft) => {
                Some(Message::CursorLeft)
            }
            _ => None,
        }));

        Subscription::batch(subscriptions)
    }

    /// Unused: the bar has no main window.
    fn view(&self) -> Element<'_, Message> {
        widget::text("").into()
    }

    fn view_window(&self, id: SurfaceId) -> Element<'_, Message> {
        if let Some(popup) = &self.popup
            && popup.id == id
        {
            return self.view_popup(popup);
        }
        if self.bars.iter().any(|bar| bar.surface == id) {
            return self.view_bar(id);
        }
        log::debug!("view_window for unknown surface {id:?}");
        widget::text("").into()
    }
}

impl Bar {
    fn ctx_for(&self, surface: Option<SurfaceId>) -> Ctx {
        let output = surface
            .or_else(|| self.popup.as_ref().map(|popup| popup.parent))
            .and_then(|surface| {
                self.bars
                    .iter()
                    .find(|bar| bar.surface == surface)
                    .and_then(|bar| bar.name.clone())
            });
        Ctx {
            palette: self.config.palette(),
            height: self.config.height,
            output,
            now_ms: self.now.timestamp().as_millisecond(),
            font_size: self.config.font_size,
            terminal: self.config.terminal.clone(),
            taskbar_scope: self.config.taskbar_scope,
            width: surface
                .or_else(|| self.popup.as_ref().map(|popup| popup.parent))
                .and_then(|surface| {
                    self.bars
                        .iter()
                        .find(|bar| bar.surface == surface)
                        .and_then(|bar| bar.width)
                }),
        }
    }

    fn add_bar(&mut self, output: WlOutput, name: Option<String>) -> Task<Message> {
        if let Some(existing) = self.bars.iter_mut().find(|bar| bar.output == output) {
            // InfoUpdate for an output we already carry: only the name can change.
            if name.is_some() {
                existing.name = name;
            }
            return Task::none();
        }
        if !self.config.outputs.is_empty() {
            let wanted = name
                .as_deref()
                .is_some_and(|name| self.config.outputs.iter().any(|want| want == name));
            if !wanted {
                return Task::none();
            }
        }

        let surface = SurfaceId::unique();
        let height = self.config.height;
        self.bars.push(BarSurface {
            output: output.clone(),
            name,
            surface,
            width: None,
        });

        get_layer_surface(SctkLayerSurfaceSettings {
            id: surface,
            layer: if self.config.overlay_layer {
                Layer::Overlay
            } else {
                Layer::Top
            },
            // OnDemand (not None): a wlr-layer-shell popup grab is only
            // granted to a surface that can take keyboard focus, and the
            // grab is what dismisses a popup when you click elsewhere.
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            output: IcedOutput::Output(output),
            namespace: "cosmicbar".into(),
            size: Some((None, Some(height))),
            exclusive_zone: height as i32,
            size_limits: Limits::NONE
                .min_width(1.0)
                .min_height(height as f32)
                .max_height(height as f32),
            ..Default::default()
        })
    }

    fn remove_bar(&mut self, output: &WlOutput) -> Task<Message> {
        let Some(index) = self.bars.iter().position(|bar| &bar.output == output) else {
            return Task::none();
        };
        let removed = self.bars.remove(index);
        self.rects
            .retain(|(surface, _), _| *surface != removed.surface);
        let mut tasks = vec![destroy_layer_surface(removed.surface)];
        if self
            .popup
            .as_ref()
            .is_some_and(|popup| popup.parent == removed.surface)
            && let Some(popup) = self.popup.take()
        {
            tasks.push(destroy_popup(popup.id));
        }
        Task::batch(tasks)
    }

    /// Re-read the config file. Layout, palette and height are picked up live;
    /// a bar surface only has to be rebuilt when its output set or height
    /// changed, and a popup for a module that is no longer placed is dropped.
    fn reload(&mut self) -> Task<Message> {
        let previous = std::mem::replace(&mut self.config, Config::load());
        self.modules.sync_extensions(&self.config);
        let mut tasks = Vec::new();

        if self
            .popup
            .as_ref()
            .is_some_and(|popup| !self.config.wants(popup.module))
        {
            tasks.push(self.close_popup());
        }

        if previous.height != self.config.height || previous.outputs != self.config.outputs {
            let outputs: Vec<(WlOutput, Option<String>)> = self
                .bars
                .iter()
                .map(|bar| (bar.output.clone(), bar.name.clone()))
                .collect();
            for (output, _) in &outputs {
                tasks.push(self.remove_bar(output));
            }
            for (output, name) in outputs {
                tasks.push(self.add_bar(output, name));
            }
        }

        Task::batch(tasks)
    }

    fn close_popup(&mut self) -> Task<Message> {
        match self.popup.take() {
            Some(popup) => destroy_popup(popup.id),
            None => Task::none(),
        }
    }

    fn toggle_popup(&mut self, parent: SurfaceId, module: ModuleId, grab: bool) -> Task<Message> {
        let already_open = self
            .popup
            .as_ref()
            .is_some_and(|popup| popup.module == module && popup.parent == parent);
        let closing = self.popup.is_some();
        let close = self.close_popup();
        if already_open {
            return close;
        }
        if !self.modules.has_popup(module) {
            return close;
        }

        let anchor = self
            .rects
            .get(&(parent, module))
            .copied()
            .unwrap_or(Rectangle {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: self.config.height as f32,
            });
        let id = SurfaceId::unique();
        self.popup = Some(Popup { id, parent, module });
        log::debug!(
            "opening popup {id:?} for {} on {parent:?}, anchor {anchor:?}, grab={grab}",
            module.name()
        );

        let open = get_popup(SctkPopupSettings {
                parent,
                id,
                positioner: SctkPositioner {
                    size: Some((360, 240)),
                    size_limits: Limits::NONE
                        .min_width(POPUP_MIN_WIDTH)
                        .min_height(1.0)
                        .max_width(POPUP_MAX_WIDTH)
                        .max_height(POPUP_MAX_HEIGHT),
                    anchor_rect: Rectangle {
                        x: anchor.x.round() as i32,
                        y: anchor.y.round() as i32,
                        width: anchor.width.round().max(1.0) as i32,
                        height: anchor.height.round().max(1.0) as i32,
                    },
                    anchor: cosmic::cctk::sctk::reexports::protocols::xdg::shell::client::xdg_positioner::Anchor::Bottom,
                    gravity: cosmic::cctk::sctk::reexports::protocols::xdg::shell::client::xdg_positioner::Gravity::Bottom,
                    // slide_x | slide_y | flip_x | flip_y
                    constraint_adjustment: 15,
                    offset: (0, 4),
                    reactive: true,
                },
                parent_size: None,
                // A pointer-driven popup grabs so a click elsewhere dismisses
                // it; a keybind-driven one has no serial to grab with.
                grab,
                close_with_children: true,
                input_zone: None,
        });

        // Only batch when a different popup actually had to be torn down: an
        // extra `Task::none()` in the batch is harmless, but keeping the open
        // request alone in the common case makes the effect ordering obvious.
        match closing {
            true => Task::batch([close, open]),
            false => open,
        }
    }

    fn view_popup(&self, popup: &Popup) -> Element<'_, Message> {
        let ctx = self.ctx_for(Some(popup.parent));
        let content = self.modules.popup(popup.module, &ctx);
        // The two must agree: `has_popup` is what makes the cell clickable and
        // what keeps this surface up, so a module answering `true` and then
        // building nothing would leave an empty card on screen.
        debug_assert_eq!(
            content.is_some(),
            self.modules.has_popup(popup.module),
            "{}: has_popup disagrees with popup()",
            popup.module.name()
        );
        let content = content.unwrap_or_else(|| widget::text("").into());

        autosize::autosize(
            crate::hover::guard(content)
                .apply(widget::container)
                .class(crate::theme::popup(ctx.palette)),
            AUTOSIZE_ID.clone(),
        )
        .limits(
            Limits::NONE
                .min_width(POPUP_MIN_WIDTH)
                .min_height(1.0)
                .max_width(POPUP_MAX_WIDTH)
                .max_height(POPUP_MAX_HEIGHT),
        )
        .into()
    }

    fn view_bar(&self, surface: SurfaceId) -> Element<'_, Message> {
        let ctx = self.ctx_for(Some(surface));
        let height = Length::Fixed(self.config.height as f32);

        let edges = widget::Row::new()
            .push(self.region(surface, &self.config.left, &ctx))
            .push(widget::space::horizontal())
            .push(self.region(surface, &self.config.right, &ctx))
            .width(Length::Fill)
            .height(height)
            .align_y(Alignment::Center);

        let center = self
            .region(surface, &self.config.center, &ctx)
            .apply(widget::container)
            .width(Length::Fill)
            .height(height)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center);

        crate::hover::guard(
            cosmic::iced::widget::Stack::new()
                .push(edges)
                .push(center)
                .width(Length::Fill)
                .height(height)
                .apply(widget::container)
                .class(crate::theme::bar(ctx.palette))
                .padding([0.0, 8.0])
                .width(Length::Fill)
                .height(height),
        )
    }

    /// Modules are laid out left to right; a module either opens an island or
    /// joins the one its left neighbour opened, in which case the two are welded
    /// into a single rounded pill with a hairline between them — what the waybar
    /// powerline glyphs were imitating.
    fn region<'a>(
        &'a self,
        surface: SurfaceId,
        ids: &'a [ModuleId],
        ctx: &Ctx,
    ) -> Element<'a, Message> {
        let height = Length::Fixed(self.config.height as f32);
        let mut row = widget::Row::new()
            .spacing(6)
            .align_y(Alignment::Center)
            .height(height);

        let mut group: Vec<(ModuleId, Element<'a, Message>)> = Vec::new();
        let mut group_island = None;

        for &id in ids {
            let Some(content) = self.modules.view(id, ctx) else {
                continue;
            };
            let island = id.island();
            // A `Join` continues the island to its left; anything else closes it
            // and opens its own. There has to be an island to continue: first in
            // the region, or after a module that paints none, a `Join` opens one
            // rather than losing its background to a neighbour that has none.
            let joins = island == crate::theme::Island::Join
                && group_island == Some(crate::theme::Island::Start);
            if !joins && !group.is_empty() {
                row = row.push(self.island_group(
                    surface,
                    group_island,
                    std::mem::take(&mut group),
                    ctx,
                ));
            }
            if !joins {
                group_island = Some(island.opened());
            }
            group.push((id, content));
        }
        if !group.is_empty() {
            row = row.push(self.island_group(surface, group_island, group, ctx));
        }

        row.into()
    }

    fn island_group<'a>(
        &'a self,
        surface: SurfaceId,
        island: Option<crate::theme::Island>,
        members: Vec<(ModuleId, Element<'a, Message>)>,
        ctx: &Ctx,
    ) -> Element<'a, Message> {
        let height = Length::Fixed(self.config.height as f32);
        let last = members.len().saturating_sub(1);
        // What the cells sit on, so a hover lifts away from it instead of
        // painting one flat colour that vanishes on the lighter roles.
        let island_bg = island
            .unwrap_or(crate::theme::Island::Flat)
            .color(&ctx.palette)
            .unwrap_or_else(|| ctx.palette.bar_bg());
        let mut inner = widget::Row::new().align_y(Alignment::Center).height(height);

        for (index, (id, content)) in members.into_iter().enumerate() {
            let radius = match (index == 0, index == last) {
                (true, true) => [crate::theme::ISLAND_RADIUS; 4],
                (true, false) => [
                    crate::theme::ISLAND_RADIUS,
                    0.0,
                    0.0,
                    crate::theme::ISLAND_RADIUS,
                ],
                (false, true) => [
                    0.0,
                    crate::theme::ISLAND_RADIUS,
                    crate::theme::ISLAND_RADIUS,
                    0.0,
                ],
                (false, false) => [0.0; 4],
            };
            // A state test, not a build: laying out one bar frame must not
            // construct the popup contents of every module on the bar.
            let clickable = self.modules.has_popup(id);
            // A clickable cell lights up under the pointer, the way the waybar
            // `:hover` rules did; a passive one is a plain container and stays
            // inert. The button carries the click, `fill` carries the paint: it
            // fades between the island colour and the lift instead of swapping
            // one flat style for another on the frame the pointer arrives.
            let cell: Element<'a, Message> = if clickable {
                // The inner container is what centers: a `button` lays its
                // content out at the top of its bounds, so without this a cell
                // whose content is shorter than the bar — every icon — sits
                // high by half the slack.
                let content = content
                    .apply(widget::container)
                    .height(height)
                    .align_y(Alignment::Center);
                let button = widget::button::custom(content)
                    .padding([0.0, modules::ISLAND_PADDING])
                    .height(height)
                    .class(crate::theme::cell(ctx.palette.fg(), radius))
                    .on_press(Message::Toggle(surface, id));
                crate::fill::fill(
                    button,
                    crate::fill::Fill {
                        base: None,
                        over: Some(ctx.palette.hover_over(island_bg)),
                        pressed: Some(ctx.palette.press_over(island_bg)),
                    },
                    radius,
                )
            } else {
                content
                    .apply(widget::container)
                    .padding([0.0, modules::ISLAND_PADDING])
                    .height(height)
                    .align_y(Alignment::Center)
                    .into()
            };
            // Right-click: the module's one obvious verb, done in place. It
            // wraps *outside* the button, which claims only left presses, so
            // both buttons keep working on the same cell. A module with no verb
            // of its own gets its popup, which is how the window list stays
            // reachable now that the taskbar strip is nothing but items, each
            // eating its own left click.
            let on_right = modules::right_click(id)
                .or_else(|| clickable.then(|| Message::Toggle(surface, id)));
            let cell: Element<'a, Message> = match on_right {
                Some(message) => modules::pointer::Pointer::new(cell)
                    .on_right(message)
                    .wrap(),
                None => cell,
            };
            // The tracker container forwards `state()` to whatever it wraps but
            // leaves `tag()` at the default *stateless* tag, so iced sees every
            // tracked cell as the same kind of node and happily reuses one
            // module's state for another module's widget the moment a cell
            // appears, vanishes or shifts along the row — a downcast panic one
            // frame later. A single-child `Row` inside the tracker keeps that
            // node genuinely stateless: reuse then only re-diffs the child,
            // where the real tags are compared and a mismatch rebuilds.
            let cell: Element<'a, Message> = match &self.tracker {
                Some(tracker) => tracker
                    .container(
                        (surface, id),
                        widget::Row::new()
                            .push(cell)
                            .height(height)
                            .align_y(Alignment::Center),
                    )
                    .into(),
                None => cell,
            };
            inner = inner.push(cell);
            if index != last {
                inner = inner.push(
                    widget::space::vertical()
                        .width(Length::Fixed(1.0))
                        .height(Length::Fixed(ctx.height as f32 * 0.45)),
                );
            }
        }

        inner
            .apply(widget::container)
            .height(height)
            .class(crate::theme::island(
                ctx.palette,
                island.unwrap_or(crate::theme::Island::Flat),
            ))
            .into()
    }
}

/// Wall clock for a `Ctx::now_ms` timestamp.
pub fn local(now_ms: i64) -> jiff::Zoned {
    jiff::Timestamp::from_millisecond(now_ms)
        .unwrap_or_else(|_| jiff::Timestamp::now())
        .in_tz("UTC")
        .map(|zoned| zoned.with_time_zone(jiff::tz::TimeZone::system()))
        .unwrap_or_else(|_| jiff::Zoned::now())
}

/// The bar's own clock, aligned to the boundary it renders: every module that
/// shows a wall-clock time flips at the same instant, and the bar sleeps a full
/// minute unless a module renders per-second detail.
///
/// The alignment lives *inside* the stream. Recomputing the interval outside it
/// would change the subscription's identity on every update, and iced would
/// tear the timer down and start a new one each time — which fires immediately,
/// so the bar would spin instead of tick.
fn tick(fast: bool) -> Subscription<Message> {
    Subscription::run_with(fast, |fast| {
        let fast = *fast;
        cosmic::iced::stream::channel(1, async move |mut sender| {
            use cosmic::iced::futures::SinkExt;

            loop {
                let now = jiff::Zoned::now();
                let into_second = now.millisecond() as u64;
                let wait = if fast {
                    1_000 - into_second
                } else {
                    60_000 - (now.second() as u64 * 1_000 + into_second)
                };
                tokio::time::sleep(std::time::Duration::from_millis(wait.max(20))).await;
                if sender.send(Message::Tick).await.is_err() {
                    return;
                }
            }
        })
    })
}
