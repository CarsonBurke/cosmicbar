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
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Source events carry the command revision across async delivery.
    Source {
        revision: u64,
        event: extension::Event,
    },
    /// A popup button was pressed; the extension decides what it means.
    Press {
        revision: u64,
        run: u64,
        action: String,
    },
}

/// Island role: an extension is one flat cell beside its neighbours, the way
/// the built-in single-cell modules are.
pub const ISLAND: crate::theme::Island = crate::theme::Island::Flat;

// Module indices are interned for the process lifetime, but module states can
// disappear and be recreated. An epoch must never identify both incarnations.
fn epoch() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut current = NEXT.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).expect("extension epochs exhausted");
        match NEXT.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return current,
            Err(updated) => current = updated,
        }
    }
}

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
    revision: u64,
    run: u64,
    frame: Option<Arc<Frame>>,
    /// Pipe to the running program, absent while it is being restarted.
    commands: Option<tokio::sync::mpsc::Sender<Command>>,
    /// A restarted process must supply its own frame before old controls can
    /// become active; its action IDs may differ from the frame kept on screen.
    ready: bool,
    /// Whether this module's popup is on screen. Kept here because the
    /// extension has to be *told*, and a restart has to be told again.
    open: bool,
}

impl State {
    pub fn new(index: u32, command: Arc<[String]>) -> Self {
        Self {
            index,
            command,
            revision: epoch(),
            run: 0,
            frame: None,
            commands: None,
            ready: false,
            open: false,
        }
    }

    /// Adopt a reloaded config's command. The running process keeps going while
    /// the command is unchanged; a different one restarts it on the next
    /// subscription rebuild, since the command is part of the identity.
    pub fn set_command(&mut self, command: Arc<[String]>) {
        if self.command != command {
            self.revision = epoch();
            self.commands = None;
            self.ready = false;
        }
        self.command = command;
    }

    /// The program is spawned once and left running: it is a push source like
    /// any other, and restarting it whenever a popup opened would make an
    /// extension the most expensive module on the bar. `open` reaches the
    /// program as a command instead.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::run_with((self.index, self.revision, self.command.clone()), spawn)
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Source { revision, event } if revision == self.revision => match event {
                extension::Event::Started(commands) => {
                    self.run = epoch();
                    self.ready = false;
                    // A fresh process must learn whether its popup is open.
                    let _ = commands.try_send(Command::Popup { popup: self.open });
                    self.commands = Some(commands);
                }
                extension::Event::Frame(frame) => {
                    self.frame = Some(frame);
                    self.ready = true;
                }
                // Retain the last frame while restarting, with its controls disabled.
                extension::Event::Stopped => {
                    self.commands = None;
                    self.ready = false;
                }
            },
            Event::Press {
                revision,
                run,
                action,
            } if revision == self.revision && run == self.run && self.connected() => {
                self.send(Command::Action { action });
            }
            // Cancellation cannot retract an event already queued by the old
            // subscription, nor a click from the view shown before a reload.
            _ => {}
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

    fn connected(&self) -> bool {
        self.ready
            && self
                .commands
                .as_ref()
                .is_some_and(|commands| !commands.is_closed())
    }

    fn action_message(&self, action: &extension::Action) -> Option<Message> {
        (action.enabled && self.connected()).then(|| {
            Message::Module(ModuleEvent::Extension(
                self.index,
                Event::Press {
                    revision: self.revision,
                    run: self.run,
                    action: action.id.clone(),
                },
            ))
        })
    }

    /// `None` hides the module: an extension with nothing to report takes no bar
    /// space, and neither does one whose program has never sent a frame.
    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let cell = self.frame.as_ref()?.cell.as_ref()?;
        Some(crate::theme::label(
            cell.glyph.as_str(),
            cell.text.as_str(),
            ctx.font_size,
            cosmic::theme::Text::Color(if self.connected() {
                cell.color.color(&ctx.palette)
            } else {
                ctx.palette.muted()
            }),
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
            let header = self.row(header, ctx, true);
            card = if self.connected() {
                card.block(header)
            } else {
                card.block(
                    crate::popup::lines()
                        .push(header)
                        .push(crate::popup::detail(
                            "reconnecting · showing last update",
                            ctx,
                        )),
                )
            };
        } else if !self.connected() {
            card = card.block(crate::popup::detail(
                "reconnecting · showing last update",
                ctx,
            ));
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
                self.action_message(action),
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
fn spawn(input: &(u32, u64, Arc<[String]>)) -> impl Stream<Item = Message> + use<> {
    let (index, revision, command) = input;
    let (index, revision) = (*index, *revision);
    extension::stream(command.clone()).map(move |event| {
        Message::Module(ModuleEvent::Extension(
            index,
            Event::Source { revision, event },
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(0, Arc::from(["test-extension".to_string()]))
    }

    fn frame() -> Arc<Frame> {
        Arc::new(serde_json::from_str(r#"{"cell":{"text":"job running"},"header":{"lines":[{"text":"queue"}],"action":{"id":"pause","label":"pause"}}}"#).unwrap())
    }

    #[test]
    fn stopped_extension_keeps_frame_but_disables_actions_until_restart_frame() {
        let mut state = state();
        let frame = frame();
        let action = frame.header.as_ref().unwrap().action.as_ref().unwrap();
        let (commands, mut inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        assert!(matches!(
            inbox.try_recv().unwrap(),
            Command::Popup { popup: false }
        ));
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(frame.clone()),
        });
        assert!(state.action_message(action).is_some());
        state.set_open(true);
        assert!(matches!(
            inbox.try_recv().unwrap(),
            Command::Popup { popup: true }
        ));
        let _ = state.update(Event::Press {
            revision: state.revision,
            run: state.run,
            action: "pause".into(),
        });
        assert!(
            matches!(inbox.try_recv().unwrap(), Command::Action { action } if action == "pause")
        );

        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Stopped,
        });
        assert!(Arc::ptr_eq(state.frame.as_ref().unwrap(), &frame));
        assert!(state.has_popup());
        assert!(state.action_message(action).is_none());
        let _ = state.update(Event::Press {
            revision: state.revision,
            run: state.run,
            action: "pause".into(),
        });
        assert!(inbox.try_recv().is_err());

        let (commands, mut inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        assert!(matches!(
            inbox.try_recv().unwrap(),
            Command::Popup { popup: true }
        ));
        assert!(state.action_message(action).is_none());
        let _ = state.update(Event::Press {
            revision: state.revision,
            run: state.run,
            action: "pause".into(),
        });
        assert!(inbox.try_recv().is_err());
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(frame.clone()),
        });
        assert!(state.action_message(action).is_some());
        let _ = state.update(Event::Press {
            revision: state.revision,
            run: state.run,
            action: "pause".into(),
        });
        assert!(
            matches!(inbox.try_recv().unwrap(), Command::Action { action } if action == "pause")
        );
    }

    #[test]
    fn closed_command_pipe_and_reconfigured_process_disable_retained_actions() {
        let mut state = state();
        let frame = frame();
        let action = frame.header.as_ref().unwrap().action.as_ref().unwrap();
        let (commands, inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(frame.clone()),
        });
        assert!(state.action_message(action).is_some());
        let disabled = extension::Action {
            enabled: false,
            ..action.clone()
        };
        assert!(state.action_message(&disabled).is_none());
        drop(inbox);
        assert!(state.action_message(action).is_none());

        let (commands, _inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(frame.clone()),
        });
        state.set_command(Arc::from(["replacement-extension".to_string()]));
        assert!(Arc::ptr_eq(state.frame.as_ref().unwrap(), &frame));
        assert!(state.action_message(action).is_none());
    }

    #[test]
    fn old_source_events_and_clicks_cannot_change_a_replacement_process() {
        let mut state = state();
        let old_revision = state.revision;
        let old_frame = frame();
        let action = old_frame.header.as_ref().unwrap().action.as_ref().unwrap();
        let (old_commands, _old_inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: old_revision,
            event: extension::Event::Started(old_commands.clone()),
        });
        let _ = state.update(Event::Source {
            revision: old_revision,
            event: extension::Event::Frame(old_frame.clone()),
        });
        let queued_click = state.action_message(action).unwrap();

        state.set_command(Arc::from(["replacement-extension".to_string()]));
        // Returning to the same command must still reject its previous lifetime.
        state.set_command(Arc::from(["test-extension".to_string()]));
        let new_frame = frame();
        let (commands, mut inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        let _ = inbox.try_recv().unwrap();
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(new_frame.clone()),
        });

        for event in [
            extension::Event::Started(old_commands),
            extension::Event::Frame(old_frame),
            extension::Event::Stopped,
        ] {
            let _ = state.update(Event::Source {
                revision: old_revision,
                event,
            });
            assert!(Arc::ptr_eq(state.frame.as_ref().unwrap(), &new_frame));
            assert!(state.connected());
        }
        let Message::Module(ModuleEvent::Extension(_, event)) = queued_click else {
            panic!("expected extension action");
        };
        let _ = state.update(event);
        assert!(inbox.try_recv().is_err());
        let _ = state.update(Event::Press {
            revision: state.revision,
            run: state.run,
            action: "pause".into(),
        });
        assert!(
            matches!(inbox.try_recv().unwrap(), Command::Action { action } if action == "pause")
        );
    }

    #[test]
    fn removed_and_readded_state_rejects_events_from_the_old_incarnation() {
        let old = state();
        let mut replacement = state();
        assert_ne!(old.revision, replacement.revision);
        let (commands, _inbox) = tokio::sync::mpsc::channel(4);
        let _ = replacement.update(Event::Source {
            revision: old.revision,
            event: extension::Event::Started(commands),
        });
        let _ = replacement.update(Event::Source {
            revision: old.revision,
            event: extension::Event::Frame(frame()),
        });
        assert!(replacement.commands.is_none());
        assert!(replacement.frame.is_none());
    }

    #[test]
    fn queued_click_from_previous_run_is_rejected_after_same_command_restarts() {
        let mut state = state();
        let frame = frame();
        let (commands, _inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(frame.clone()),
        });
        let old_click = state
            .action_message(frame.header.as_ref().unwrap().action.as_ref().unwrap())
            .unwrap();
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Stopped,
        });
        let (commands, mut inbox) = tokio::sync::mpsc::channel(4);
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Started(commands),
        });
        let _ = inbox.try_recv().unwrap();
        let _ = state.update(Event::Source {
            revision: state.revision,
            event: extension::Event::Frame(frame),
        });
        let Message::Module(ModuleEvent::Extension(_, event)) = old_click else {
            panic!("expected extension action");
        };
        let _ = state.update(event);
        assert!(inbox.try_recv().is_err());
    }
}
