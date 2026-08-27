//! Bar modules.
//!
//! One module = one file = one [`ModuleId`]. Every module file exposes the
//! same small surface, and the [`modules!`] macro below wires the registry, so
//! adding a module never touches shared plumbing:
//!
//! ```ignore
//! pub const ISLAND: Island = Island::Start;       // island boundary
//! #[derive(Debug, Clone)] pub enum Event { .. }   // its own messages
//! #[derive(Default)] pub struct State { .. }      // its own state
//! impl State {
//!     /// Push sources. `open` is true while this module's popup is on screen,
//!     /// so detail only worth streaming while it is visible (rates, meters,
//!     /// menu contents) can be subscribed to exactly then. iced starts and
//!     /// stops subscriptions as they appear and disappear from this list.
//!     pub fn subscription(&self, open: bool) -> Subscription<Message>;
//!     pub fn update(&mut self, event: Event) -> Task<Message>;
//!     /// `None` hides the module entirely.
//!     pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>>;
//!     /// Popup content, built only for the popup that is open.
//!     pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>>;
//!     /// Whether `popup` would return `Some` — the bar asks this of every
//!     /// placed module on every frame to know which cells are clickable, so it
//!     /// must be a state test and never build the popup's widgets.
//!     pub fn has_popup(&self) -> bool;
//!     /// True while the module renders something that changes every second.
//!     /// `open` is this module's popup being on screen: a seek bar that only
//!     /// exists in the popup asks for the fast clock only while it is there.
//!     pub fn fast_tick(&self, open: bool) -> bool;
//! }
//! ```
//!
//! Modules never poll a shell script: each reads its source directly (unix
//! socket, D-Bus signal, niri IPC event stream, kernel interface).

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};

use crate::bar::Message;
use crate::config::Config;
use crate::theme::{Island, Palette};
use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::Subscription;

/// Horizontal padding inside an island, in logical pixels.
pub const ISLAND_PADDING: f32 = 10.0;

/// Everything a module needs to render one frame.
pub struct Ctx {
    pub palette: Palette,
    pub height: u32,
    /// Name of the output this frame is being drawn for, when the compositor
    /// has told us; per-output modules (workspaces, taskbar) filter on it.
    pub output: Option<String>,
    /// Wall clock for the frame, so durations agree across modules.
    pub now_ms: i64,
    pub font_size: f32,
    /// Terminal to use for modules that open a TUI.
    pub terminal: String,
    /// Which windows the taskbar strip lists.
    pub taskbar_scope: crate::config::TaskbarScope,
    /// Logical width of the bar this frame is drawn for, once the compositor
    /// has said. A module that can grow without bound needs a budget.
    pub width: Option<f32>,
}

impl Ctx {
    /// Popup type scale. Three steps from the bar's own size, so a card reads
    /// as a hierarchy rather than as one size in several colours: the title of
    /// a popup is as big as the cell it hangs from, a row's own name is a step
    /// down from it, and the detail under that name is a step down again.
    ///
    /// A row's own text: the name of the thing the row is about.
    pub fn body(&self) -> f32 {
        (self.font_size - 2.0).max(10.0)
    }

    /// Secondary text: the detail under a name, a section label, a chip.
    pub fn small(&self) -> f32 {
        (self.font_size - 4.0).max(9.0)
    }
}

/// Shared widget: wheel and right-click over a bar cell.
pub mod pointer;

/// A module drawn by another process. Not in the registry below: extensions are
/// declared in the config, so there are as many of them as the user asked for.
pub mod extension;

/// Extension names come from the config, but a [`ModuleId`] is `Copy` and has
/// to stay meaningful for as long as anything holds one — an open popup, a
/// message already queued, a config reload after that. So each distinct name is
/// registered once in this table and carried as its index. The table only ever
/// grows by names the config actually used.
fn names() -> &'static std::sync::Mutex<Vec<std::sync::Arc<str>>> {
    static NAMES: std::sync::LazyLock<std::sync::Mutex<Vec<std::sync::Arc<str>>>> =
        std::sync::LazyLock::new(Default::default);
    &NAMES
}

/// Index of `name` in the table, registering it on first sight.
fn intern(name: &str) -> u32 {
    let mut names = names().lock().expect("extension name table poisoned");
    if let Some(index) = names.iter().position(|known| &**known == name) {
        return index as u32;
    }
    names.push(std::sync::Arc::from(name));
    (names.len() - 1) as u32
}

/// Index of `name`, and no more: nothing is registered. Names arriving from
/// outside the config — a control command — can only mean something this
/// process already knows, and a table that grew by an entry per typo would
/// never give it back.
fn interned_index(name: &str) -> Option<u32> {
    let names = names().lock().expect("extension name table poisoned");
    names
        .iter()
        .position(|known| &**known == name)
        .map(|index| index as u32)
}

/// The name behind an index. Empty only for an index this process never handed
/// out, which nothing can construct through [`ModuleId`].
fn interned(index: u32) -> std::sync::Arc<str> {
    names()
        .lock()
        .expect("extension name table poisoned")
        .get(index as usize)
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::from(""))
}

macro_rules! modules {
    ($($variant:ident => $module:ident),* $(,)?) => {
        $(pub mod $module;)*

        /// Identifies a module in the config file, on the control socket and
        /// at runtime.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ModuleId {
            $($variant,)*
            /// A module whose cell and popup come from another process. The
            /// payload identifies its config name; see [`ModuleId::extension`].
            Extension(u32),
        }

        impl ModuleId {
            /// Every built-in module. Extensions are named by the config, so
            /// they are not enumerable here.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            /// Position of a built-in module in [`ModuleId::ALL`], so a set of
            /// them fits in a bitmask. `None` for an extension: extensions are
            /// named by the config, so there is no fixed set to index into.
            pub fn bit(self) -> Option<u32> {
                match self {
                    Self::Extension(_) => None,
                    id => Self::ALL.iter().position(|other| *other == id).map(|at| at as u32),
                }
            }

            /// The name used in config files and control commands. Borrowed for
            /// every built-in module; an extension's name lives in the name
            /// table, so reading it back copies the string.
            pub fn name(self) -> std::borrow::Cow<'static, str> {
                match self {
                    $(Self::$variant => std::borrow::Cow::Borrowed(stringify!($module)),)*
                    Self::Extension(index) => {
                        std::borrow::Cow::Owned(format!("extension:{}", interned(index)))
                    }
                }
            }

            /// The id of the extension declared under `name` in the config.
            pub fn extension(name: &str) -> Self {
                Self::Extension(intern(name))
            }

            pub fn parse(name: &str) -> Option<Self> {
                if let Some(rest) = name.strip_prefix("extension:") {
                    // An undeclared extension is not an error here: placements
                    // are resolved against the declared extensions, and one
                    // with no entry simply draws nothing.
                    return (!rest.is_empty()).then(|| Self::extension(rest));
                }
                Self::ALL.iter().copied().find(|id| id.name() == name)
            }

            /// Like [`ModuleId::parse`], but for a name from outside the config
            /// file: an extension nothing has declared is unknown rather than
            /// newly registered, so a stream of junk on the control socket
            /// cannot grow the name table.
            pub fn parse_declared(name: &str) -> Option<Self> {
                if let Some(rest) = name.strip_prefix("extension:") {
                    return interned_index(rest).map(Self::Extension);
                }
                Self::ALL.iter().copied().find(|id| id.name() == name)
            }

            /// Island background role, mirroring the waybar colour chain.
            pub fn island(self) -> Island {
                match self {
                    $(Self::$variant => $module::ISLAND,)*
                    Self::Extension(_) => extension::ISLAND,
                }
            }
        }

        /// One variant per module, so module files never edit shared enums.
        #[derive(Debug, Clone)]
        pub enum ModuleEvent {
            $($variant($module::Event),)*
            /// From the extension registered under this name index.
            Extension(u32, extension::Event),
        }

        impl ModuleEvent {
            /// Which module an event came from, without formatting its payload:
            /// a module event can carry a whole queue snapshot.
            pub fn label(&self) -> Cow<'static, str> {
                match self {
                    $(Self::$variant(_) => Cow::Borrowed(stringify!($module)),)*
                    Self::Extension(index, _) => ModuleId::Extension(*index).name(),
                }
            }
        }

        /// All module state, owned by the bar.
        #[derive(Default)]
        pub struct Modules {
            $(pub $module: $module::State,)*
            /// One entry per placed extension, keyed by its name index. Built
            /// from the config by [`Modules::sync_extensions`].
            pub extensions: BTreeMap<u32, extension::State>,
        }

        impl Modules {
            /// Push subscriptions for the modules that are actually placed.
            /// `popup` is the module whose popup is open, so a module can
            /// stream extra detail only while it is on screen.
            pub fn subscriptions(
                &self,
                config: &Config,
                popup: Option<ModuleId>,
            ) -> Vec<Subscription<Message>> {
                let mut subscriptions = Vec::new();
                $(if config.wants(ModuleId::$variant) {
                    subscriptions
                        .push(self.$module.subscription(popup == Some(ModuleId::$variant)));
                })*
                // An extension is a push source whose popup state travels as a
                // command, so it needs no `open` here: see `set_popup`.
                subscriptions.extend(self.extensions.values().map(extension::State::subscription));
                subscriptions
            }

            /// Bring the extension states in line with the config: one running
            /// program per declared extension that some region actually places.
            /// Dropping a state ends its subscription, which kills its process.
            pub fn sync_extensions(&mut self, config: &Config) {
                self.extensions.retain(|index, _| {
                    let id = ModuleId::Extension(*index);
                    // An entry whose command was blanked is a stopped extension,
                    // not a running one with nothing to run: the process has to
                    // go the same way a deleted entry's does.
                    config.wants(id)
                        && config
                            .extension(id)
                            .is_some_and(|entry| !entry.command.is_empty())
                });
                let mut placed = std::collections::BTreeSet::new();
                for entry in &config.extensions {
                    let ModuleId::Extension(index) = ModuleId::extension(&entry.name) else {
                        continue;
                    };
                    if !config.wants(ModuleId::Extension(index)) {
                        continue;
                    }
                    // One name is one module, so a second entry under it is a
                    // typo rather than a second program. `Config::extension`
                    // answers with the first declaration, and this has to agree
                    // even when that first declaration is the blank one — a
                    // duplicate that took over would leave the running process
                    // attached to a state the bar had already dropped.
                    if !placed.insert(index) {
                        log::warn!(
                            "extension `{}` is declared twice; ignoring the later entry",
                            entry.name
                        );
                        continue;
                    }
                    if entry.command.is_empty() {
                        log::warn!("extension `{}` has an empty command", entry.name);
                        continue;
                    }
                    let command: std::sync::Arc<[String]> = entry.command.as_slice().into();
                    match self.extensions.get_mut(&index) {
                        Some(state) => state.set_command(command),
                        None => {
                            self.extensions
                                .insert(index, extension::State::new(index, command));
                        }
                    }
                }
            }

            /// Tell each extension whether its popup is the one on screen, so a
            /// program only gathers popup detail while it can be seen.
            pub fn set_popup(&mut self, open: Option<ModuleId>) {
                for (index, state) in &mut self.extensions {
                    state.set_open(open == Some(ModuleId::Extension(*index)));
                }
            }

            pub fn update(&mut self, event: ModuleEvent) -> Task<Message> {
                match event {
                    $(ModuleEvent::$variant(event) => self.$module.update(event),)*
                    // An event from an extension the config just dropped has
                    // nowhere to go, and nothing to change.
                    ModuleEvent::Extension(index, event) => match self.extensions.get_mut(&index) {
                        Some(state) => state.update(event),
                        None => Task::none(),
                    },
                }
            }

            /// The widget shown in the bar, without island styling.
            pub fn view(&self, id: ModuleId, ctx: &Ctx) -> Option<Element<'_, Message>> {
                match id {
                    $(ModuleId::$variant => self.$module.view(ctx),)*
                    ModuleId::Extension(index) => {
                        self.extensions.get(&index)?.view(ctx)
                    }
                }
            }

            /// Popup content. Only ever called for the popup that is open: the
            /// clickability of a cell comes from `has_popup`, so a closed
            /// popup's rows, icons and D-Bus-fed lists are never built.
            pub fn popup(&self, id: ModuleId, ctx: &Ctx) -> Option<Element<'_, Message>> {
                match id {
                    $(ModuleId::$variant => self.$module.popup(ctx),)*
                    ModuleId::Extension(index) => self.extensions.get(&index)?.popup(ctx),
                }
            }

            /// Whether this module would show a popup, without building one.
            pub fn has_popup(&self, id: ModuleId) -> bool {
                match id {
                    $(ModuleId::$variant => self.$module.has_popup(),)*
                    ModuleId::Extension(index) => self
                        .extensions
                        .get(&index)
                        .is_some_and(extension::State::has_popup),
                }
            }

            /// True when any placed module renders per-second detail. `popup` is
            /// the module whose popup is open, if any: per-second detail that
            /// only exists inside a popup does not wake the bar while it is shut.
            pub fn fast_tick(&self, config: &Config, popup: Option<ModuleId>) -> bool {
                $(if config.wants(ModuleId::$variant)
                    && self.$module.fast_tick(popup == Some(ModuleId::$variant))
                {
                    return true;
                })*
                false
            }
        }
    };
}

// Registry. One line per module; the file name is the config name.
modules! {
    Time => time,
    Date => date,
    Workspaces => workspaces,
    Taskbar => taskbar,
    Cpu => cpu,
    Memory => memory,
    Gpu => gpu,
    Network => network,
    Bluetooth => bluetooth,
    Volume => volume,
    Mpris => mpris,
    Tray => tray,
    Notifications => notifications,
    Updates => updates,
    Battery => battery,
    Brightness => brightness,
    IdleInhibitor => idle_inhibitor,
    Launcher => launcher,
    Power => power,
}

/// Right-click actions. Cross-module policy, so it lives with the registry
/// rather than in nineteen module files: a right-click does the module's one
/// obvious verb without opening anything, mirroring the `on-click-right`
/// bindings of the waybar config this replaces. Anything not listed falls
/// through to the bar's default, which is to open the module's popup.
pub fn right_click(id: ModuleId) -> Option<Message> {
    let event = match id {
        // Pausing is the verb you want without looking, and the popup's own
        // controls stay for everything else.
        ModuleId::Mpris => ModuleEvent::Mpris(mpris::Event::Dispatch(mpris::Action::PlayPause)),
        // waybar: `volume.sh output mute`.
        ModuleId::Volume => ModuleEvent::Volume(volume::Event::MuteDefault),
        // waybar: `nmcli radio wifi off`, made reversible.
        ModuleId::Network => ModuleEvent::Network(network::Event::ToggleWireless),
        // waybar: `bluetoothctl power off`, made reversible.
        ModuleId::Bluetooth => ModuleEvent::Bluetooth(bluetooth::Event::TogglePowered),
        // waybar: `mako.sh dismiss`.
        ModuleId::Notifications => ModuleEvent::Notifications(notifications::Event::DismissAll),
        // waybar: `pkill -RTMIN+1 waybar`, which is how its update module was
        // told to re-check.
        ModuleId::Updates => ModuleEvent::Updates(updates::Event::CheckNow),
        // Holding the machine awake is a switch, not a menu.
        ModuleId::IdleInhibitor => ModuleEvent::IdleInhibitor(idle_inhibitor::Event::Toggle),
        _ => return None,
    };
    Some(Message::Module(event))
}

impl<'de> Deserialize<'de> for ModuleId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown module `{name}`; known modules: {}",
                Self::ALL
                    .iter()
                    .map(|id| id.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}
