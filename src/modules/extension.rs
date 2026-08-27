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

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::Subscription;
use cosmic::iced::futures::{Stream, StreamExt};
use cosmic::widget;

use crate::bar::Message;
use crate::extension::{self, Command, Frame, Item, Line, Row};
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

    /// Mirrors `popup`'s own test: a frame with nothing in its popup is a cell
    /// that does not click.
    pub fn has_popup(&self) -> bool {
        self.frame
            .as_ref()
            .is_some_and(|frame| frame.header.is_some() || !frame.popup.is_empty())
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let frame = self.frame.as_ref()?;
        if frame.header.is_none() && frame.popup.is_empty() {
            return None;
        }
        let mut card = crate::popup::Card::new();
        if let Some(header) = &frame.header {
            card = card.block(self.row(header, ctx, true));
        }
        if !frame.popup.is_empty() {
            let mut list = crate::popup::column();
            for item in &frame.popup {
                list = list.push(match item {
                    Item::Divider => widget::divider::horizontal::default().into(),
                    Item::Text(line) => self.line(line, ctx, false),
                    Item::Row(row) => self.row(row, ctx, false),
                });
            }
            // How long the list is belongs to the extension: the card scrolls
            // it rather than asking the program to guess what fits.
            card = card.list(list);
        }
        Some(card.build())
    }

    /// One row: its lines stacked on the left, its action on the right. In the
    /// header the first line is the card's title, which is what makes a
    /// `header` worth sending instead of a first row.
    fn row<'a>(&self, row: &'a Row, ctx: &Ctx, header: bool) -> Element<'a, Message> {
        let mut lines = crate::popup::lines();
        for (index, line) in row.lines.iter().enumerate() {
            lines = lines.push(self.line(line, ctx, header && index == 0));
        }
        let action = row.action.as_ref().map(|action| {
            let style = match action.danger {
                true => crate::popup::Chip::Danger,
                false => crate::popup::Chip::Plain,
            };
            crate::popup::chip(
                action.label.as_str(),
                style,
                ctx,
                action.enabled.then(|| {
                    Message::Module(ModuleEvent::Extension(
                        self.index,
                        Event::Press(action.id.clone()),
                    ))
                }),
            )
        });
        crate::popup::split(lines, action).into()
    }

    fn line<'a>(&self, line: &'a Line, ctx: &Ctx, title: bool) -> Element<'a, Message> {
        let size = match (title, line.small) {
            (true, _) => ctx.font_size,
            (false, true) => ctx.small(),
            (false, false) => ctx.body(),
        };
        crate::theme::text(line.text.as_str())
            .size(size)
            .class(cosmic::theme::Text::Color(line.color.color(&ctx.palette)))
            .into()
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
