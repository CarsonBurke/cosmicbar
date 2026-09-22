//! Brightness, for internal panels *and* external monitors.
//!
//! Waybar needed two modules and two shell scripts for this: `backlight` +
//! `scripts/backlight.sh` (brightnessctl) for a laptop panel, and
//! `custom/brightness` + `scripts/brightness.sh` (ddcutil, a cache directory,
//! `pkill -RTMIN+5 waybar` and a zenity dialog per monitor) for external ones.
//! Here one module owns both backends, the popup has a real slider per display,
//! and scrolling the bar cell nudges the monitor that bar is drawn on.
//!
//! Backends, in order of preference:
//!
//! * `/sys/class/backlight/*` when a panel exists. Reads come from sysfs;
//!   writes go through logind's `Session.SetBrightness`, which is why this needs
//!   no root, no udev rule and no `brightnessctl` suid helper.
//! * DDC/CI over i2c otherwise, by running `ddcutil` as a child process — there
//!   is no Rust binding for it. Measured on this machine: `ddcutil detect
//!   --brief` ≈ 0.8 s, and each `getvcp`/`setvcp` ≈ 0.28 s. That is far too slow
//!   to touch from a view, so displays are detected once, values are cached and
//!   rendered from the cache, writes are coalesced per display (a slider drag
//!   issues one write at a time, always with the newest value), and live re-reads
//!   only happen while the popup is open.
//!
//! Named modes (`day`, `night`) store a level per display; see [`modes`].

mod modes;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, renderer};
use cosmic::iced::{Length, Rectangle, Size, Subscription, Vector, mouse};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

/// waybar: `@backlight` → base.
pub const ISLAND: Island = Island::Start;

/// Scroll step, matching `custom/brightness`'s `up 5` / `down 5`.
const STEP: u32 = 5;
/// Shortest gap between two accepted wheel notches.
const NUDGE_DEBOUNCE: Duration = Duration::from_millis(80);
/// Presets offered in the popup.
const PRESETS: [u32; 5] = [10, 25, 50, 75, 100];
/// nf-md-theme_light_dark: step to the next mode.
const ICON_CYCLE: &str = "\u{f050e}";
/// nf-md-plus: save the current levels as a new mode.
const ICON_ADD: &str = "\u{f0415}";
/// nf-md-pencil
const ICON_EDIT: &str = "\u{f03eb}";
/// nf-md-delete
const ICON_DELETE: &str = "\u{f01b4}";
/// nf-md-close: leave the editor without saving.
const ICON_CANCEL: &str = "\u{f0156}";
/// nf-md-radiobox_marked / nf-md-radiobox_blank: the mode in effect.
const ICON_ACTIVE: &str = "\u{f043e}";
const ICON_INACTIVE: &str = "\u{f043d}";
/// A wedged i2c bus must not pin a task forever.
const DDC_TIMEOUT: Duration = Duration::from_secs(5);
/// Re-read interval while the popup is open, so a change made elsewhere shows up.
const REFRESH: Duration = Duration::from_secs(3);
/// Retry interval while no display has been found yet (monitor plugged in later).
const DETECT_RETRY: Duration = Duration::from_secs(60);
/// sysfs panels never go fully dark, mirroring `brightnessctl -n`.
const SYSFS_FLOOR: u32 = 1;

/// Where one display's brightness is read and written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Sink {
    /// `/sys/class/backlight/<name>`, `max` raw units.
    Backlight { name: String, max: u32 },
    /// External monitor: `bus` is the i2c bus number from `ddcutil detect`,
    /// which survives display renumbering, unlike `--display N`. `max` is the
    /// monitor's own maximum for VCP feature 0x10.
    Ddc { bus: u32, max: u32 },
}

impl Sink {
    /// The hardware value `percent` is written as. Two levels are the same
    /// brightness on this display exactly when these agree: a panel with eight
    /// steps reads 30% back as 29%.
    fn raw(&self, percent: u32) -> u32 {
        match self {
            Self::Backlight { max, .. } => from_percent(percent.max(SYSFS_FLOOR), *max),
            Self::Ddc { max, .. } => from_percent(percent, *max),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Found {
    sink: Sink,
    /// Monitor model, or the backlight device name.
    label: String,
    /// DRM connector (`DP-1`), when the backend reports it: lets each bar show
    /// and scroll the display it is actually drawn on.
    connector: Option<String>,
    percent: u32,
}

impl Found {
    /// What a mode calls this display: its connector, which is stable across
    /// replugs and matches the compositor's name for it, or else its label.
    fn key(&self) -> &str {
        self.connector.as_deref().unwrap_or(&self.label)
    }
}

#[derive(Debug)]
struct Display {
    found: Found,
    /// Newest value the user asked for while a write was in flight.
    pending: Option<u32>,
    writing: bool,
}

/// What a click or a scroll applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    One(usize),
    All,
}

#[derive(Debug, Clone)]
pub enum Event {
    Detected(Arc<Vec<Found>>),
    /// Live values from the popup-only refresh.
    Refreshed(Arc<Vec<(Sink, u32)>>),
    Set(Target, u32),
    Nudge(Target, i32),
    Wrote {
        index: usize,
        percent: u32,
        result: Result<(), String>,
    },
    /// The saved modes, read at startup and on `cosmicbar reload`.
    ModesLoaded(Result<Arc<modes::Loaded>, String>),
    ApplyMode(usize),
    /// `cosmicbar brightness-mode <name>`.
    ApplyNamed(String),
    /// Step to the mode after the one in effect.
    CycleMode,
    /// Open the editor on a new mode, which will hold the current levels.
    NewMode,
    /// Apply a mode and open it in the editor: the sliders are how its levels
    /// are changed.
    EditMode(usize),
    /// Text typed into the editor's name.
    Typed(String),
    /// Backspace; with Ctrl, the whole last word.
    Erase { word: bool },
    SaveMode,
    DeleteMode,
    CancelEdit,
    Saved(Result<(), String>),
}

/// A mode being named, and what saving it will replace.
#[derive(Debug)]
struct Editor {
    /// The mode being edited, or `None` for a new one.
    target: Option<usize>,
    name: String,
}

#[derive(Debug, Default)]
pub struct State {
    displays: Vec<Display>,
    /// When the last wheel notch was accepted; see [`NUDGE_DEBOUNCE`].
    nudged_at: Option<Instant>,
    error: Option<String>,
    modes: Vec<modes::Mode>,
    /// False until the modes file has been read, and while it cannot be: a
    /// save then would overwrite whatever made it unreadable — a hand edit
    /// half done — so editing waits for a `cosmicbar reload` that reads it.
    modes_readable: bool,
    /// Why the modes file could not be read or written.
    modes_error: Option<String>,
    /// The mode applied last, so cycling from levels that match no mode still
    /// moves on from where it was rather than starting over.
    last_mode: Option<usize>,
    editor: Option<Editor>,
    /// Bumped per edit; orders the writes, see [`modes::save`].
    saves: u64,
}

impl State {
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        if self.displays.is_empty() {
            // Detection is the only thing worth doing until something is found.
            return Subscription::run(detect);
        }
        if !open {
            // Nobody is looking, and a DDC read costs a third of a second.
            return Subscription::none();
        }
        // Typing only means something while the editor is on screen.
        let typing = match self.editor {
            Some(_) => cosmic::iced::event::listen_with(keys),
            None => Subscription::none(),
        };
        let sinks: Vec<Sink> = self
            .displays
            .iter()
            .map(|display| display.found.sink.clone())
            .collect();
        Subscription::batch([
            Subscription::run_with(sinks, |sinks| refresh(sinks.clone())),
            typing,
        ])
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Detected(found) => {
                self.displays = found
                    .iter()
                    .cloned()
                    .map(|found| Display {
                        found,
                        pending: None,
                        writing: false,
                    })
                    .collect();
                Task::none()
            }
            Event::Refreshed(values) => {
                for (sink, percent) in values.iter() {
                    if let Some(display) = self
                        .displays
                        .iter_mut()
                        .find(|display| &display.found.sink == sink)
                    {
                        // Never fight the user: a display we are writing to
                        // keeps the value the pointer picked.
                        if !display.writing && display.pending.is_none() {
                            display.found.percent = *percent;
                        }
                    }
                }
                Task::none()
            }
            Event::Set(target, percent) => self.apply(target, |_| percent),
            Event::Nudge(target, steps) => {
                // niri/winit can deliver two wheel events for one physical
                // notch (`axis_value120` plus the legacy axis), which would
                // double every step. One step per debounce window is also all
                // the i2c bus can absorb: a DDC write takes ~0.3 s.
                let now = Instant::now();
                if steps == 0
                    || self
                        .nudged_at
                        .is_some_and(|last| now.duration_since(last) < NUDGE_DEBOUNCE)
                {
                    return Task::none();
                }
                self.nudged_at = Some(now);
                let delta = steps.signum().saturating_mul(STEP as i32);
                self.apply(target, |current| {
                    (current as i32 + delta).clamp(0, 100) as u32
                })
            }
            Event::ModesLoaded(Ok(loaded)) => {
                let modes::Loaded { modes, generation } = Arc::unwrap_or_clone(loaded);
                // Read before an edit's save ran: the save writes what is
                // here already, over what was read.
                if generation < self.saves {
                    return Task::none();
                }
                // A save that failed left edits only in memory; the file is
                // the truth now, but say where they went.
                let unsaved = self.modes_readable.then_some(()).and(self.modes_error.take());
                self.modes_readable = true;
                // `cosmicbar reload` follows every config.toml save; only a
                // changed file disturbs the editor and the cycling position.
                if modes != self.modes {
                    self.modes_error = unsaved
                        .map(|error| format!("{error}; unsaved changes were replaced by the file"));
                    self.modes = modes;
                    self.last_mode = None;
                    // The list may have been reordered under an open editor.
                    if self.editor.as_ref().is_some_and(|editor| editor.target.is_some()) {
                        self.editor = None;
                    }
                }
                Task::none()
            }
            Event::ModesLoaded(Err(error)) => {
                log::warn!("brightness modes: {error}");
                self.modes_readable = false;
                self.modes_error = Some(error);
                self.editor = None;
                Task::none()
            }
            Event::ApplyMode(index) => self.apply_mode(index),
            Event::ApplyNamed(name) => {
                let name = name.trim();
                match self
                    .modes
                    .iter()
                    .position(|mode| mode.name.trim().eq_ignore_ascii_case(name))
                {
                    Some(index) => self.apply_mode(index),
                    None => {
                        log::warn!("no brightness mode named `{name}`");
                        Task::none()
                    }
                }
            }
            Event::CycleMode => {
                if self.modes.is_empty() {
                    return Task::none();
                }
                let next = self
                    .active_mode()
                    .or(self.last_mode)
                    .map_or(0, |index| (index + 1) % self.modes.len());
                self.apply_mode(next)
            }
            Event::NewMode => {
                if !self.modes_readable {
                    return Task::none();
                }
                self.editor = Some(Editor {
                    target: None,
                    name: String::new(),
                });
                Task::none()
            }
            Event::EditMode(index) => {
                let Some(mode) = self.modes.get(index).filter(|_| self.modes_readable) else {
                    return Task::none();
                };
                self.editor = Some(Editor {
                    target: Some(index),
                    name: mode.name.clone(),
                });
                self.apply_mode(index)
            }
            Event::Typed(text) => {
                if let Some(editor) = &mut self.editor {
                    editor.name.extend(text.chars().filter(|c| !c.is_control()));
                }
                Task::none()
            }
            Event::Erase { word } => {
                if let Some(editor) = &mut self.editor {
                    match word {
                        true => {
                            let kept = editor.name.trim_end().rfind(' ').map_or(0, |at| at + 1);
                            editor.name.truncate(kept);
                        }
                        false => {
                            editor.name.pop();
                        }
                    }
                }
                Task::none()
            }
            Event::SaveMode => {
                // An unsaveable draft stays open to be fixed.
                let Some(name) = self.draft_name() else {
                    return Task::none();
                };
                let Some(editor) = self.editor.take() else {
                    return Task::none();
                };
                let levels = self.levels();
                match editor.target.filter(|index| *index < self.modes.len()) {
                    Some(index) => {
                        let mode = &mut self.modes[index];
                        mode.name = name;
                        // A display that is unplugged right now keeps the
                        // level the mode already had for it.
                        mode.levels.extend(levels);
                        self.last_mode = Some(index);
                    }
                    None => {
                        self.modes.push(modes::Mode { name, levels });
                        self.last_mode = Some(self.modes.len() - 1);
                    }
                }
                self.save()
            }
            Event::DeleteMode => {
                let Some(index) = self
                    .editor
                    .take()
                    .and_then(|editor| editor.target)
                    .filter(|index| *index < self.modes.len())
                else {
                    return Task::none();
                };
                self.modes.remove(index);
                // Opening the editor applied the mode, so it was the last one.
                self.last_mode = None;
                self.save()
            }
            Event::CancelEdit => {
                self.editor = None;
                Task::none()
            }
            Event::Saved(result) => {
                match result {
                    Ok(()) => self.modes_error = None,
                    Err(error) => {
                        log::warn!("brightness modes: {error}");
                        self.modes_error = Some(error);
                    }
                }
                Task::none()
            }
            Event::Wrote {
                index,
                percent,
                result,
            } => {
                if let Err(error) = &result {
                    log::warn!("brightness write failed: {error}");
                }
                self.error = result.err();
                let Some(display) = self.displays.get_mut(index) else {
                    return Task::none();
                };
                display.writing = false;
                match display.pending.take() {
                    // The user moved on while that write was in flight.
                    Some(newest) if newest != percent => self.write(index, newest),
                    _ => Task::none(),
                }
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if self.displays.is_empty() {
            // No panel, no DDC monitor: nothing to show.
            return None;
        }
        let target = self.target(ctx);
        let percent = self.percent(target);
        let color = if self.error.is_some() {
            ctx.palette.red
        } else {
            ctx.palette.fg()
        };
        Some(
            crate::theme::label(
                glyph(percent),
                format!("{percent}%"),
                ctx.font_size,
                cosmic::theme::Text::Color(color),
            )
            // waybar bound the wheel to `brightness.sh up/down 5`. This cannot
            // be a `mouse_area`: that widget captures every left click over it,
            // which would eat the bar's popup toggle.
            .apply(|label| {
                Wheel::new(label, move |delta| {
                    event_message(Event::Nudge(target, steps(delta)))
                })
            })
            .into(),
        )
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        !self.displays.is_empty()
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        if self.displays.is_empty() {
            return None;
        }
        let mut card = Card::new();
        for (index, display) in self.displays.iter().enumerate() {
            let percent = display.found.percent;
            let value: Element<'_, Message> = popup::item(format!("{percent}%"), ctx)
                .class(cosmic::theme::Text::Color(ctx.palette.accent()))
                .into();
            card = card.block(
                popup::column()
                    .push(popup::section(heading(&display.found), ctx))
                    .push(popup::split(
                        widget::slider(0..=100, percent, move |percent| {
                            event_message(Event::Set(Target::One(index), percent))
                        })
                        .step(1u32),
                        [value],
                    )),
            );
        }

        // The presets move every display at once, which is what makes them the
        // card's footer rather than a control inside one display's block.
        Some(
            card.block(self.modes_block(ctx))
                .block(popup::split(
                popup::detail("all", ctx),
                PRESETS.map(|preset| {
                    popup::chip(
                        format!("{preset}%"),
                        Chip::Plain,
                        ctx,
                        Some(event_message(Event::Set(Target::All, preset))),
                    )
                }),
            ))
            .maybe(self.error.as_ref().map(|error| {
                popup::detail(error.as_str(), ctx)
                    .class(cosmic::theme::Text::Color(ctx.palette.red))
            }))
            .build(),
        )
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    /// Whether a right-click has a mode to step to.
    pub fn has_modes(&self) -> bool {
        !self.modes.is_empty() && !self.displays.is_empty()
    }

    /// Read the modes file: at startup, and again on `cosmicbar reload`.
    pub fn load_modes() -> Task<Message> {
        Task::future(async {
            let loaded = modes::load().await.map(Arc::new);
            cosmic::Action::App(event_message(Event::ModesLoaded(loaded)))
        })
    }

    /// The modes, a button to step through them and one to add another, then
    /// one row per mode — or the editor, in place of the mode it is editing.
    fn modes_block<'a>(&'a self, ctx: &Ctx) -> Element<'a, Message> {
        let palette = ctx.palette;
        let active = self.active_mode();
        // One edit at a time, and none while the file cannot be trusted.
        let editable = self.modes_readable && self.editor.is_none();
        let editing = self.editor.as_ref().map(|editor| editor.target);

        let mut block = popup::column().push(popup::split(
            popup::section("modes", ctx),
            [
                popup::icon_chip(
                    ICON_CYCLE,
                    Chip::Plain,
                    ctx,
                    (!self.modes.is_empty()).then(|| event_message(Event::CycleMode)),
                ),
                popup::icon_chip(
                    ICON_ADD,
                    Chip::Plain,
                    ctx,
                    editable.then(|| event_message(Event::NewMode)),
                ),
            ],
        ));

        for (index, mode) in self.modes.iter().enumerate() {
            if editing == Some(Some(index)) {
                block = block.push(self.editor_row(ctx));
                continue;
            }
            let (radio, color) = match active == Some(index) {
                true => (ICON_ACTIVE, palette.accent()),
                false => (ICON_INACTIVE, palette.muted()),
            };
            let label = widget::Row::new()
                .push(
                    crate::theme::icon_text(radio)
                        .size(ctx.small())
                        .class(cosmic::theme::Text::Color(color)),
                )
                .push(
                    popup::lines()
                        .push(
                            popup::item(mode.name.as_str(), ctx)
                                .class(cosmic::theme::Text::Color(color)),
                        )
                        .push(popup::detail(mode.summary(), ctx)),
                )
                .spacing(popup::ROW_GAP)
                .align_y(cosmic::iced::Alignment::Center);
            block = block.push(popup::split(
                popup::row(
                    label,
                    palette,
                    Some(event_message(Event::ApplyMode(index))),
                ),
                [popup::icon_chip(
                    ICON_EDIT,
                    Chip::Plain,
                    ctx,
                    editable.then(|| event_message(Event::EditMode(index))),
                )],
            ));
        }
        if editing == Some(None) {
            block = block.push(self.editor_row(ctx));
        } else if self.modes.is_empty() && self.modes_readable {
            block = block.push(popup::detail(
                "set the sliders, then + saves them as a mode",
                ctx,
            ));
        }
        block
            .push_maybe(self.modes_error.as_ref().map(|error| {
                popup::detail(error.as_str(), ctx)
                    .class(cosmic::theme::Text::Color(palette.red))
            }))
            .into()
    }

    /// The name field and its verbs, over the levels saving will store.
    fn editor_row<'a>(&'a self, ctx: &Ctx) -> Element<'a, Message> {
        let Some(editor) = &self.editor else {
            return widget::Row::new().into();
        };
        let save = self.draft_name().map(|_| event_message(Event::SaveMode));
        let mut actions = vec![popup::chip("save", Chip::Accent, ctx, save)];
        if editor.target.is_some() {
            actions.push(popup::icon_chip(
                ICON_DELETE,
                Chip::Danger,
                ctx,
                Some(event_message(Event::DeleteMode)),
            ));
        }
        actions.push(popup::icon_chip(
            ICON_CANCEL,
            Chip::Plain,
            ctx,
            Some(event_message(Event::CancelEdit)),
        ));
        let levels = self.levels();
        popup::lines()
            .push(popup::split(
                popup::field(editor.name.as_str(), "mode name", ctx),
                actions,
            ))
            .push(popup::detail(
                format!(
                    "saves {}",
                    modes::summary(levels.iter().map(|(key, level)| (key.as_str(), *level)))
                ),
                ctx,
            ))
            .into()
    }

    /// Every display's level right now, as a mode stores it.
    fn levels(&self) -> BTreeMap<String, u32> {
        self.displays
            .iter()
            .map(|display| (display.found.key().to_string(), display.found.percent))
            .collect()
    }

    /// The mode in effect: the one applied last while it still is, else the
    /// first that is. Modes can share levels — or one can name a subset of
    /// another's displays — and cycling has to move on from the one it applied.
    fn active_mode(&self) -> Option<usize> {
        self.last_mode
            .filter(|&index| self.in_effect(index))
            .or_else(|| (0..self.modes.len()).find(|&index| self.in_effect(index)))
    }

    /// Whether every display mode `index` names is at its level. A mode that
    /// names none of the displays present is never in effect.
    fn in_effect(&self, index: usize) -> bool {
        let Some(mode) = self.modes.get(index) else {
            return false;
        };
        let mut named = self
            .displays
            .iter()
            .filter_map(|display| {
                mode.levels
                    .get(display.found.key())
                    .map(|level| (&display.found, *level))
            })
            .peekable();
        named.peek().is_some()
            && named.all(|(found, level)| found.sink.raw(level) == found.sink.raw(found.percent))
    }

    /// The editor's name, trimmed, when it can be saved: not empty, and not
    /// another mode's, since `cosmicbar brightness-mode <name>` has to pick one.
    fn draft_name(&self) -> Option<String> {
        let editor = self.editor.as_ref()?;
        let name = editor.name.trim();
        let taken = self.modes.iter().enumerate().any(|(index, mode)| {
            Some(index) != editor.target && mode.name.trim().eq_ignore_ascii_case(name)
        });
        (!name.is_empty() && !taken).then(|| name.to_string())
    }

    fn apply_mode(&mut self, index: usize) -> Task<Message> {
        let Some(mode) = self.modes.get(index) else {
            return Task::none();
        };
        self.last_mode = Some(index);
        let targets: Vec<(usize, u32)> = self
            .displays
            .iter()
            .enumerate()
            .filter_map(|(at, display)| {
                mode.levels
                    .get(display.found.key())
                    .map(|level| (at, (*level).min(100)))
            })
            .collect();
        Task::batch(
            targets
                .into_iter()
                .map(|(at, level)| self.apply(Target::One(at), |_| level)),
        )
    }

    fn save(&mut self) -> Task<Message> {
        self.saves += 1;
        let (modes, generation) = (self.modes.clone(), self.saves);
        Task::future(async move {
            let result = modes::save(modes, generation).await;
            cosmic::Action::App(event_message(Event::Saved(result)))
        })
    }

    /// The display this bar cell speaks for: the one on this output when the
    /// connector is known, otherwise every display at once (which is what the
    /// waybar script's averaged readout and paired `up 1 5 && up 2 5` did).
    fn target(&self, ctx: &Ctx) -> Target {
        if let Some(output) = &ctx.output
            && let Some(index) = self.displays.iter().position(|display| {
                display
                    .found
                    .connector
                    .as_deref()
                    .is_some_and(|connector| connector == output)
            }) {
                return Target::One(index);
            }
        if self.displays.len() == 1 {
            Target::One(0)
        } else {
            Target::All
        }
    }

    /// The number the bar cell shows: one display's value, or the average when
    /// the cell speaks for all of them.
    fn percent(&self, target: Target) -> u32 {
        match target {
            Target::One(index) => self
                .displays
                .get(index)
                .map_or(0, |display| display.found.percent),
            Target::All if self.displays.is_empty() => 0,
            Target::All => {
                let total: u32 = self
                    .displays
                    .iter()
                    .map(|display| display.found.percent)
                    .sum();
                total / self.displays.len() as u32
            }
        }
    }

    /// Change one or every display, coalescing against writes in flight.
    fn apply(&mut self, target: Target, value: impl Fn(u32) -> u32) -> Task<Message> {
        let indices: Vec<usize> = match target {
            Target::One(index) => vec![index],
            Target::All => (0..self.displays.len()).collect(),
        };
        let mut tasks = Vec::with_capacity(indices.len());
        for index in indices {
            let Some(display) = self.displays.get_mut(index) else {
                continue;
            };
            let wanted = value(display.found.percent).min(100);
            if wanted == display.found.percent && display.pending.is_none() {
                continue;
            }
            // Optimistic: the slider and the bar follow the pointer, and the
            // hardware catches up a third of a second later.
            display.found.percent = wanted;
            if display.writing {
                display.pending = Some(wanted);
                continue;
            }
            display.writing = true;
            tasks.push(self.write(index, wanted));
        }
        Task::batch(tasks)
    }

    fn write(&mut self, index: usize, percent: u32) -> Task<Message> {
        let Some(display) = self.displays.get_mut(index) else {
            return Task::none();
        };
        display.writing = true;
        let sink = display.found.sink.clone();
        Task::future(async move {
            cosmic::Action::App(event_message(Event::Wrote {
                index,
                percent,
                result: set(&sink, percent)
                    .await
                    .map_err(|error| format!("{error:#}")),
            }))
        })
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Brightness(event))
}

/// Keys for the mode editor. They arrive at the bar's layer surface, not the
/// popup: niri leaves keyboard focus on the layer surface under a grabbing
/// popup (see [`popup::field`]). Nothing on the bar itself takes keys, so
/// reading them here while the editor is on screen takes nothing from anyone.
fn keys(
    event: cosmic::iced::Event,
    _status: cosmic::iced::event::Status,
    _window: cosmic::iced::window::Id,
) -> Option<Message> {
    use cosmic::iced::keyboard::{self, key::Named};

    let cosmic::iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modifiers,
        text,
        ..
    }) = event
    else {
        return None;
    };
    let event = match key {
        keyboard::Key::Named(Named::Enter) => Event::SaveMode,
        keyboard::Key::Named(Named::Escape) => Event::CancelEdit,
        keyboard::Key::Named(Named::Backspace) => Event::Erase {
            word: modifiers.control(),
        },
        // Shortcuts are not text, whatever the layout makes of them.
        _ if modifiers.control() || modifiers.alt() || modifiers.logo() => return None,
        _ => Event::Typed(text?.to_string()),
    };
    Some(event_message(event))
}

/// How a display introduces itself. The connector leads when the backend knows
/// one, because `DP-1` is what the compositor and this bar's own per-output
/// cells call that monitor, and the model beside it is what is printed on the
/// bezel in front of the user.
fn heading(found: &Found) -> String {
    match &found.connector {
        Some(connector) => format!("{connector} · {}", found.label),
        None => found.label.clone(),
    }
}

/// waybar's script picked its icon from an average; the thresholds are the same,
/// with the two middle glyphs put back in ascending order (the script had
/// nf-md-brightness-4 above nf-md-brightness-5).
fn glyph(percent: u32) -> &'static str {
    match percent {
        75.. => "\u{f00e0}",
        50..75 => "\u{f00de}",
        25..50 => "\u{f00dd}",
        _ => "\u{f00dc}",
    }
}

fn steps(delta: mouse::ScrollDelta) -> i32 {
    match delta {
        mouse::ScrollDelta::Lines { y, .. } => y.round() as i32,
        // Touchpads and high-resolution wheels report pixels.
        mouse::ScrollDelta::Pixels { y, .. } => (y / 50.0).round() as i32,
    }
}

/// Probe for displays until something answers.
fn detect() -> impl Stream<Item = Message> {
    cosmic::iced::stream::channel(1, async move |mut sender| {
        loop {
            let found = match backlights().await {
                // A panel exists: DDC is not worth the seconds it costs.
                found if !found.is_empty() => found,
                _ => ddc_detect().await.unwrap_or_else(|error| {
                    log::debug!("ddcutil detect: {error:#}");
                    Vec::new()
                }),
            };
            if !found.is_empty() {
                let _ = sender
                    .send(event_message(Event::Detected(Arc::new(found))))
                    .await;
            }
            tokio::time::sleep(DETECT_RETRY).await;
        }
    })
}

/// Live values, only while the popup is open.
fn refresh(sinks: Vec<Sink>) -> impl Stream<Item = Message> {
    cosmic::iced::stream::channel(1, async move |mut sender| {
        loop {
            let mut values = Vec::with_capacity(sinks.len());
            for sink in &sinks {
                if let Ok(percent) = get(sink).await {
                    values.push((sink.clone(), percent));
                }
            }
            if sender
                .send(event_message(Event::Refreshed(Arc::new(values))))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(REFRESH).await;
        }
    })
}

async fn get(sink: &Sink) -> anyhow::Result<u32> {
    match sink {
        Sink::Backlight { name, max } => {
            let raw = read_number(&format!("/sys/class/backlight/{name}/brightness")).await?;
            Ok(to_percent(raw, *max))
        }
        Sink::Ddc { bus, max } => {
            let (current, _) = ddc_get(*bus).await?;
            Ok(to_percent(current, *max))
        }
    }
}

async fn set(sink: &Sink, percent: u32) -> anyhow::Result<()> {
    match sink {
        Sink::Backlight { name, .. } => set_backlight(name, sink.raw(percent)).await,
        Sink::Ddc { bus, .. } => ddc_set(*bus, sink.raw(percent)).await,
    }
}

fn to_percent(raw: u32, max: u32) -> u32 {
    if max == 0 {
        return 0;
    }
    ((f64::from(raw) / f64::from(max)) * 100.0).round().min(100.0) as u32
}

fn from_percent(percent: u32, max: u32) -> u32 {
    ((f64::from(percent.min(100)) / 100.0) * f64::from(max)).round() as u32
}

/// Internal panels, newest kernel naming first (`/sys/class/backlight` holds
/// one directory per panel and is the only interface the kernel offers; there is
/// nothing to subscribe to, hence the popup-gated re-read).
async fn backlights() -> Vec<Found> {
    let mut found = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir("/sys/class/backlight").await else {
        return found;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let base = format!("/sys/class/backlight/{name}");
        let (Ok(max), Ok(current)) = (
            read_number(&format!("{base}/max_brightness")).await,
            read_number(&format!("{base}/brightness")).await,
        ) else {
            continue;
        };
        if max == 0 {
            continue;
        }
        found.push(Found {
            percent: to_percent(current, max),
            sink: Sink::Backlight {
                name: name.clone(),
                max,
            },
            label: name,
            connector: None,
        });
    }
    found
}

async fn read_number(path: &str) -> anyhow::Result<u32> {
    Ok(tokio::fs::read_to_string(path).await?.trim().parse()?)
}

/// Writes through logind, so the bar needs no privilege on
/// `/sys/class/backlight/*/brightness`.
async fn set_backlight(name: &str, raw: u32) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;
    let manager = Login1ManagerProxy::new(&connection).await?;
    let path = manager.get_session_by_pid(std::process::id()).await?;
    Login1SessionProxy::builder(&connection)
        .path(path)?
        .build()
        .await?
        .set_brightness("backlight", name, raw)
        .await?;
    Ok(())
}

/// `ddcutil detect --brief`, parsed into one display per usable monitor.
async fn ddc_detect() -> anyhow::Result<Vec<Found>> {
    let output = ddcutil(&["detect", "--brief"]).await?;
    let mut found = Vec::new();
    let mut bus = None;
    let mut connector = None;
    let mut label = None;

    let mut flush = |bus: &mut Option<u32>, connector: &mut Option<String>, label: &mut Option<String>| {
        if let Some(bus) = bus.take() {
            found.push((bus, connector.take(), label.take()));
        } else {
            connector.take();
            label.take();
        }
    };

    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("Display ") || line.starts_with("Invalid display") {
            flush(&mut bus, &mut connector, &mut label);
        } else if let Some(value) = line.strip_prefix("I2C bus:") {
            bus = value.trim().rsplit('-').next().and_then(|n| n.parse().ok());
        } else if let Some(value) = line.strip_prefix("DRM connector:") {
            // `card1-DP-2` is the same connector niri calls `DP-2`.
            let value = value.trim();
            connector = value
                .split_once('-')
                .map(|(_, rest)| rest.to_string())
                .filter(|rest| !rest.is_empty());
        } else if let Some(value) = line.strip_prefix("Monitor:") {
            // `MFG:MODEL:SERIAL`
            let value = value.trim();
            label = value
                .split(':')
                .nth(1)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .or_else(|| Some(value.to_string()));
        }
    }
    flush(&mut bus, &mut connector, &mut label);

    let mut displays = Vec::with_capacity(found.len());
    for (bus, connector, label) in found {
        // A bus that will not answer VCP 0x10 has no brightness to offer.
        match ddc_get(bus).await {
            Ok((current, max)) => displays.push(Found {
                percent: to_percent(current, max),
                sink: Sink::Ddc { bus, max },
                label: label.unwrap_or_else(|| format!("i2c-{bus}")),
                connector,
            }),
            Err(error) => log::debug!("ddc bus {bus}: {error:#}"),
        }
    }
    Ok(displays)
}

/// `VCP 10 C <current> <max>`
async fn ddc_get(bus: u32) -> anyhow::Result<(u32, u32)> {
    let output = ddcutil(&["getvcp", "10", "--brief", "--bus", &bus.to_string()]).await?;
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("VCP 10"))
        .ok_or_else(|| anyhow::anyhow!("no VCP 10 in ddcutil output"))?;
    let mut fields = line.split_whitespace().skip(3);
    let current = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("unparsable ddcutil value: {line}"))?;
    let max = fields.next().and_then(|value| value.parse().ok()).unwrap_or(100);
    Ok((current, max.max(1)))
}

async fn ddc_set(bus: u32, value: u32) -> anyhow::Result<()> {
    ddcutil(&[
        "setvcp",
        "10",
        &value.to_string(),
        "--bus",
        &bus.to_string(),
    ])
    .await
    .map(|_| ())
}

/// One `ddcutil` run. `ddcutil` is a child process because it is the only
/// DDC/CI implementation available here; it is never run from a view, and a
/// hung i2c transaction is bounded by [`DDC_TIMEOUT`].
async fn ddcutil(args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        DDC_TIMEOUT,
        tokio::process::Command::new("ddcutil")
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ddcutil {} timed out", args.join(" ")))??;

    if !output.status.success() {
        anyhow::bail!(
            "ddcutil {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A wrapper that reports wheel notches and nothing else.
///
/// `mouse_area` cannot be used for this: it captures every left press and
/// release that lands on it (`iced/widget/src/mouse_area.rs`, unconditional
/// `shell.capture_event()`), so the bar's own cell button never sees the click
/// and the popup would never open. This forwards everything to the content and
/// only consumes `WheelScrolled` while the cursor is inside the cell.
struct Wheel<'a, Message> {
    content: Element<'a, Message>,
    on_scroll: Box<dyn Fn(mouse::ScrollDelta) -> Message + 'a>,
}

impl<'a, Message> Wheel<'a, Message> {
    fn new(
        content: impl Into<Element<'a, Message>>,
        on_scroll: impl Fn(mouse::ScrollDelta) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            on_scroll: Box::new(on_scroll),
        }
    }
}

impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for Wheel<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &cosmic::iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if shell.is_event_captured() || !cursor.is_over(layout.bounds()) {
            return;
        }
        if let cosmic::iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) = event {
            shell.publish((self.on_scroll)(*delta));
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<
        cosmic::iced::advanced::overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>,
    > {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<Wheel<'a, Message>> for Element<'a, Message> {
    fn from(wheel: Wheel<'a, Message>) -> Self {
        Element::new(wheel)
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait Login1Manager {
    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait Login1Session {
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(connector: &str, max: u32, percent: u32) -> Display {
        Display {
            found: Found {
                sink: Sink::Ddc {
                    bus: connector.len() as u32,
                    max,
                },
                label: format!("{connector} monitor"),
                connector: Some(connector.to_string()),
                percent,
            },
            pending: None,
            writing: false,
        }
    }

    fn mode(name: &str, levels: &[(&str, u32)]) -> modes::Mode {
        modes::Mode {
            name: name.to_string(),
            levels: levels
                .iter()
                .map(|(key, level)| (key.to_string(), *level))
                .collect(),
        }
    }

    /// Two monitors at 80/70, with `day` (80/70) and `night` (20/15) saved.
    fn state() -> State {
        let mut state = State {
            displays: vec![display("DP-1", 100, 80), display("DP-2", 100, 70)],
            ..State::default()
        };
        let _ = state.update(loaded(vec![
            mode("day", &[("DP-1", 80), ("DP-2", 70)]),
            mode("night", &[("DP-1", 20), ("DP-2", 15)]),
        ]));
        state
    }

    fn loaded(modes: Vec<modes::Mode>) -> Event {
        Event::ModesLoaded(Ok(Arc::new(modes::Loaded {
            modes,
            generation: 0,
        })))
    }

    fn percents(state: &State) -> Vec<u32> {
        state
            .displays
            .iter()
            .map(|display| display.found.percent)
            .collect()
    }

    #[test]
    fn the_mode_in_effect_is_the_one_every_display_matches() {
        let mut state = state();
        assert_eq!(state.active_mode(), Some(0));
        let _ = state.update(Event::Set(Target::One(0), 81));
        assert_eq!(state.active_mode(), None);
    }

    #[test]
    fn cycling_steps_through_the_modes_and_wraps() {
        let mut state = state();
        let _ = state.update(Event::CycleMode);
        assert_eq!(percents(&state), [20, 15]);
        assert_eq!(state.active_mode(), Some(1));
        let _ = state.update(Event::CycleMode);
        assert_eq!(percents(&state), [80, 70]);
    }

    #[test]
    fn cycling_off_a_mode_continues_from_the_last_one_applied() {
        let mut state = state();
        let _ = state.update(Event::ApplyMode(1));
        let _ = state.update(Event::Set(Target::All, 50));
        let _ = state.update(Event::CycleMode);
        assert_eq!(state.active_mode(), Some(0));
    }

    #[test]
    fn cycling_moves_past_modes_that_share_levels() {
        let mut state = state();
        state.modes.insert(1, mode("work", &[("DP-1", 80), ("DP-2", 70)]));
        state.modes.push(mode("left", &[("DP-1", 80)]));
        let mut seen = Vec::new();
        for _ in 0..4 {
            let _ = state.update(Event::CycleMode);
            seen.push(state.active_mode());
        }
        assert_eq!(seen, [Some(1), Some(2), Some(3), Some(0)]);
    }

    #[test]
    fn an_unchanged_reload_keeps_the_editor_and_the_cycle() {
        let mut state = state();
        let _ = state.update(Event::ApplyMode(1));
        let _ = state.update(Event::Set(Target::All, 50));
        let _ = state.update(Event::EditMode(0));
        let modes = state.modes.clone();
        let _ = state.update(loaded(modes));
        assert!(state.editor.is_some());
        let _ = state.update(Event::CancelEdit);
        let _ = state.update(Event::Set(Target::All, 50));
        let _ = state.update(Event::CycleMode);
        assert_eq!(state.active_mode(), Some(1));
        // A changed file may have moved the mode being edited.
        let _ = state.update(Event::EditMode(1));
        let _ = state.update(loaded(vec![mode("dusk", &[("DP-1", 40)])]));
        assert!(state.editor.is_none());
        assert_eq!(state.modes.len(), 1);
    }

    #[test]
    fn a_load_older_than_an_edit_is_ignored() {
        let mut state = state();
        let _ = state.update(Event::NewMode);
        let _ = state.update(Event::Typed("dusk".into()));
        let _ = state.update(Event::SaveMode);
        // Read before the save above ran.
        let _ = state.update(loaded(vec![mode("day", &[("DP-1", 80)])]));
        assert_eq!(state.modes.len(), 3);
        // Read after it.
        let _ = state.update(Event::ModesLoaded(Ok(Arc::new(modes::Loaded {
            modes: vec![mode("day", &[("DP-1", 80)])],
            generation: 1,
        }))));
        assert_eq!(state.modes.len(), 1);
    }

    #[test]
    fn a_reload_after_a_failed_save_says_the_edit_is_gone() {
        let mut state = state();
        let _ = state.update(Event::Saved(Err("disk full".into())));
        let modes = state.modes.clone();
        let _ = state.update(loaded(modes));
        assert_eq!(state.modes_error, None, "nothing was lost");
        let _ = state.update(Event::Saved(Err("disk full".into())));
        let _ = state.update(loaded(vec![mode("dusk", &[("DP-1", 40)])]));
        assert!(state.modes_error.as_deref().unwrap().starts_with("disk full; unsaved"));
    }

    #[test]
    fn a_mode_is_applied_by_name_whatever_its_case() {
        let mut state = state();
        let _ = state.update(Event::ApplyNamed(" Night ".into()));
        assert_eq!(percents(&state), [20, 15]);
        let _ = state.update(Event::ApplyNamed("dusk".into()));
        assert_eq!(percents(&state), [20, 15]);
    }

    #[test]
    fn a_display_a_mode_does_not_name_is_left_alone() {
        let mut state = state();
        state.modes.push(mode("left only", &[("DP-1", 5)]));
        let _ = state.update(Event::ApplyMode(2));
        assert_eq!(percents(&state), [5, 70]);
        assert_eq!(state.active_mode(), Some(2));
    }

    #[test]
    fn a_new_mode_stores_the_current_levels() {
        let mut state = state();
        let _ = state.update(Event::Set(Target::All, 40));
        let _ = state.update(Event::NewMode);
        let _ = state.update(Event::Typed("  dusk ".into()));
        let _ = state.update(Event::SaveMode);
        assert!(state.editor.is_none());
        assert_eq!(state.modes[2], mode("dusk", &[("DP-1", 40), ("DP-2", 40)]));
        assert_eq!(state.active_mode(), Some(2));
        assert_eq!(state.saves, 1);
    }

    #[test]
    fn editing_applies_the_mode_and_keeps_levels_for_absent_displays() {
        let mut state = state();
        state.modes[1].levels.insert("HDMI-A-1".into(), 30);
        let _ = state.update(Event::EditMode(1));
        assert_eq!(percents(&state), [20, 15]);
        let _ = state.update(Event::Set(Target::One(1), 10));
        // Ctrl+Backspace clears the one word, then the new name is typed.
        let _ = state.update(Event::Erase { word: true });
        let _ = state.update(Event::Typed("late".into()));
        let _ = state.update(Event::SaveMode);
        assert_eq!(
            state.modes[1],
            mode("late", &[("DP-1", 20), ("DP-2", 10), ("HDMI-A-1", 30)])
        );
    }

    #[test]
    fn a_name_must_be_present_and_not_another_modes() {
        let mut state = state();
        let _ = state.update(Event::NewMode);
        let _ = state.update(Event::Typed("   ".into()));
        assert_eq!(state.draft_name(), None);
        let _ = state.update(Event::Typed("DAX".into()));
        let _ = state.update(Event::Erase { word: false });
        let _ = state.update(Event::Typed("Y\n".into()));
        assert_eq!(state.editor.as_ref().unwrap().name, "   DAY");
        assert_eq!(state.draft_name(), None);
        let _ = state.update(Event::SaveMode);
        assert_eq!(state.modes.len(), 2);
        assert!(state.editor.is_some(), "an unsaveable draft stays open");
        // A mode keeps its own name.
        let _ = state.update(Event::CancelEdit);
        let _ = state.update(Event::EditMode(0));
        assert_eq!(state.draft_name().as_deref(), Some("day"));
    }

    #[test]
    fn deleting_removes_only_the_mode_being_edited() {
        let mut state = state();
        let _ = state.update(Event::EditMode(0));
        let _ = state.update(Event::DeleteMode);
        assert_eq!(state.modes.len(), 1);
        assert_eq!(state.modes[0].name, "night");
        assert!(state.editor.is_none());
        // Cycling starts over at the first mode left.
        let _ = state.update(Event::CycleMode);
        assert_eq!(percents(&state), [20, 15]);
        // Nothing to delete outside the editor.
        let _ = state.update(Event::DeleteMode);
        assert_eq!(state.modes.len(), 1);
    }

    #[test]
    fn nothing_is_edited_while_the_file_is_unreadable() {
        let mut state = state();
        let _ = state.update(Event::ModesLoaded(Err("bad toml".into())));
        let _ = state.update(Event::NewMode);
        let _ = state.update(Event::EditMode(0));
        assert!(state.editor.is_none());
        // What was loaded before still applies.
        let _ = state.update(Event::ApplyMode(1));
        assert_eq!(percents(&state), [20, 15]);
    }

    #[test]
    fn levels_match_at_the_displays_own_resolution() {
        // Eight steps: 30% is written as 2 and reads back as 29%.
        let panel = Sink::Backlight {
            name: "intel_backlight".into(),
            max: 7,
        };
        assert_eq!(panel.raw(30), panel.raw(to_percent(panel.raw(30), 7)));
        // The floor: 0% on a panel is 1%, not off.
        assert_eq!(panel.raw(0), panel.raw(1));
    }
}
