//! A module whose contents come from another process.
//!
//! One instance per `[[extensions]]` entry in the config. All this holds is the
//! last frame the program sent and the pipe back to it, so drawing an extension
//! costs the same as drawing a built-in module: no interval, no shelling out per
//! frame, and the popup's rows are built only while the popup is open. There is
//! no per-second clock either: an extension sends a frame when it has something
//! to say, including a countdown it wants ticking.
//!
//! The protocol lives in [`crate::extension`].

use std::sync::Arc;

use cosmic::app::Task;
use cosmic::iced::futures::{Stream, StreamExt};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::extension::{self, Command, Frame, Item, Line};
use crate::modules::{Ctx, ModuleEvent};

#[derive(Debug, Clone)]
pub enum Event {
    Started(tokio::sync::mpsc::Sender<Command>),
    Frame(Arc<Frame>),
    Stopped,
    /// A popup button was pressed; the extension decides what it means.
    Press(String),
}

/// Island role: an extension is one flat cell beside its neighbours, the way
/// the built-in single-cell modules are.
pub const ISLAND: crate::theme::Island = crate::theme::Island::Flat;
/// Estimating how tall a frame wants to be: iced's default relative line
/// height, the `Column` spacing between items, the tighter spacing between the
/// lines inside a row, and a divider with its own line.
const LINE_HEIGHT: f32 = 1.4;
const ITEM_SPACING: f32 = 6.0;
const LINE_SPACING: f32 = 1.0;
const DIVIDER_ROW: f32 = 1.0;
/// Where an extension's list starts scrolling instead of growing.
const LIST_HEIGHT: f32 = 420.0;

#[derive(Debug)]
pub struct State {
    /// Name-table index of this module's config name: the routing key for its
    /// events and half of its subscription's identity.
    index: u32,
    /// Program and arguments. Changing them restarts the process, because the
    /// command is the other half of the subscription's identity, and it is
    /// shared rather than cloned because iced rebuilds subscriptions after
    /// every message the bar handles.
    command: Arc<[String]>,
    frame: Option<Arc<Frame>>,
    /// Pipe to the running program, absent while it is being restarted.
    commands: Option<tokio::sync::mpsc::Sender<Command>>,
    /// Whether this module's popup is on screen. Kept here because the
    /// extension has to be *told*, and a restart has to be told again.
    open: bool,
}

impl State {
    pub fn new(index: u32, command: Arc<[String]>) -> Self {
        Self {
            index,
            command,
            frame: None,
            commands: None,
            open: false,
        }
    }

    /// Adopt a reloaded config's command. The running process keeps going while
    /// the command is unchanged; a different one restarts it on the next
    /// subscription rebuild, since the command is part of the identity.
    pub fn set_command(&mut self, command: Arc<[String]>) {
        self.command = command;
    }

    /// The program is spawned once and left running: it is a push source like
    /// any other, and restarting it whenever a popup opened would make an
    /// extension the most expensive module on the bar. `open` reaches the
    /// program as a command instead.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::run_with((self.index, self.command.clone()), spawn)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Started(commands) => {
                // A fresh process knows nothing about the popup it may have
                // been restarted underneath.
                let _ = commands.try_send(Command::Popup { popup: self.open });
                self.commands = Some(commands);
            }
            Event::Frame(frame) => self.frame = Some(frame),
            // The last frame stays on screen: an extension being restarted is
            // not a reason to make the bar jump.
            Event::Stopped => self.commands = None,
            Event::Press(action) => self.send(Command::Action { action }),
        }
        Task::none()
    }

    /// Tell the extension whether its popup is on screen, so detail nothing can
    /// see is never gathered.
    pub fn set_open(&mut self, open: bool) {
        if self.open == open {
            return;
        }
        self.open = open;
        self.send(Command::Popup { popup: open });
    }

    fn send(&self, command: Command) {
        let Some(commands) = &self.commands else {
            return;
        };
        if let Err(error) = commands.try_send(command) {
            // A full queue means the extension has stopped reading its stdin.
            log::debug!("extension: dropping command: {error}");
        }
    }

    /// `None` hides the module: an extension with nothing to report takes no bar
    /// space, and neither does one whose program has never sent a frame.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let cell = self.frame.as_ref()?.cell.as_ref()?;
        Some(crate::theme::label(
            cell.glyph.as_str(),
            cell.text.as_str(),
            ctx.font_size,
            cosmic::theme::Text::Color(cell.color.color(&ctx.palette)),
        ))
    }

    /// Mirrors `popup`'s own test: a frame with an empty popup is a cell that
    /// does not click.
    pub fn has_popup(&self) -> bool {
        self.frame
            .as_ref()
            .is_some_and(|frame| !frame.popup.is_empty())
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let frame = self.frame.as_ref()?;
        if frame.popup.is_empty() {
            return None;
        }
        let mut body = widget::Column::new().spacing(6).width(Length::Fill);
        for item in &frame.popup {
            body = body.push(match item {
                Item::Divider => widget::divider::horizontal::default().into(),
                Item::Text(line) => self.line(line, ctx),
                Item::Row(row) => {
                    let mut lines = widget::Column::new().spacing(1).width(Length::Fill);
                    for line in &row.lines {
                        lines = lines.push(self.line(line, ctx));
                    }
                    let mut content = widget::Row::new()
                        .push(lines)
                        .spacing(8)
                        .align_y(Alignment::Center);
                    if let Some(action) = &row.action {
                        let class = if action.danger {
                            crate::theme::chip_danger(ctx.palette)
                        } else {
                            crate::theme::chip(ctx.palette)
                        };
                        content = content.push(
                            widget::button::text(action.label.as_str())
                                .class(class)
                                .on_press_maybe(action.enabled.then(|| {
                                    Message::Module(ModuleEvent::Extension(
                                        self.index,
                                        Event::Press(action.id.clone()),
                                    ))
                                })),
                        );
                    }
                    content.into()
                }
            });
        }
        // How long the list is belongs to the extension, and a popup taller than
        // the bar's own limit is never mapped at all, so scroll it. The height is
        // measured from the text the items actually carry, so a three-row popup
        // neither opens a half-empty panel nor grows a scrollbar it does not need.
        let line = |line: &Line| {
            LINE_HEIGHT
                * match line.small {
                    true => ctx.small(),
                    false => ctx.font_size,
                }
        };
        let rows: f32 = frame
            .popup
            .iter()
            .map(|item| match item {
                Item::Divider => DIVIDER_ROW,
                Item::Text(text) => line(text),
                Item::Row(row) => {
                    let lines: f32 = row.lines.iter().map(&line).sum();
                    // A row is at least as tall as its button.
                    lines.max(ctx.font_size * LINE_HEIGHT)
                        + LINE_SPACING * row.lines.len().saturating_sub(1) as f32
                }
            })
            .sum::<f32>()
            + ITEM_SPACING * frame.popup.len().saturating_sub(1) as f32;
        Some(
            widget::scrollable(body)
                .height(Length::Fixed(rows.min(LIST_HEIGHT)))
                .apply(widget::container)
                .padding(12)
                .into(),
        )
    }

    fn line<'a>(&self, line: &'a Line, ctx: &Ctx) -> Element<'a, Message> {
        let text = crate::theme::text(line.text.as_str())
            .class(cosmic::theme::Text::Color(line.color.color(&ctx.palette)));
        match line.small {
            true => text.size(ctx.small()).into(),
            false => text.into(),
        }
    }
}

/// One extension's event stream, tagged with the module it belongs to.
///
/// `Subscription::run_with` takes a plain function, so the name travels in the
/// subscription's identity rather than in a captured variable — which is also
/// what makes an edited command restart that one program and no others.
fn spawn(input: &(u32, Arc<[String]>)) -> impl Stream<Item = Message> + use<> {
    let index = input.0;
    extension::stream(input.1.clone()).map(move |event| {
        Message::Module(ModuleEvent::Extension(
            index,
            match event {
                extension::Event::Started(commands) => Event::Started(commands),
                extension::Event::Frame(frame) => Event::Frame(frame),
                extension::Event::Stopped => Event::Stopped,
            },
        ))
    })
}
