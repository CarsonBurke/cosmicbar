//! Declarative bar configuration: `$XDG_CONFIG_HOME/cosmicbar/config.toml`.
//!
//! A malformed or missing config never prevents the bar from coming up; the
//! defaults below are the shipped layout.

use std::path::PathBuf;

use serde::Deserialize;

use crate::modules::ModuleId;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Bar height in logical pixels; also the exclusive zone.
    pub height: u32,
    /// Base text size in logical pixels.
    pub font_size: f32,
    /// The waybar setup used a bold nerd font; keep that by default.
    pub font_weight_bold: bool,
    /// Palette name: `catppuccin-mocha` or `catppuccin-latte`.
    pub palette: String,
    /// Terminal used by modules that open a TUI.
    pub terminal: String,
    /// Put the bar on the overlay layer instead of top (top is normal).
    pub overlay_layer: bool,
    /// Outputs to place a bar on; empty means every output.
    pub outputs: Vec<String>,
    /// Which windows the taskbar strip lists: `output` for every window on
    /// this bar's display, workspace by workspace, or `workspace` for only
    /// the windows of the workspace that display is showing right now.
    pub taskbar_scope: TaskbarScope,
    pub left: Vec<ModuleId>,
    pub center: Vec<ModuleId>,
    pub right: Vec<ModuleId>,
    /// Modules drawn by other processes. A region places one by name, as
    /// `extension:<name>`; see `docs/extensions.md`.
    pub extensions: Vec<Extension>,
}

/// One `[[extensions]]` entry: a program the bar keeps running, which draws its
/// own cell and popup over stdio.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extension {
    /// Referred to in a region as `extension:<name>`.
    pub name: String,
    /// Program and arguments. The program is spawned once and left running; it
    /// is restarted, with backoff, if it exits.
    pub command: Vec<String>,
}

/// Scope of the taskbar strip. The taskbar popup is not affected: it always
/// lists every window on the output, grouped by workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskbarScope {
    /// Every window on the bar's own output, workspace by workspace in index
    /// order. Switching workspace then rearranges nothing.
    #[default]
    Output,
    /// Only the windows of the workspace currently visible on that output.
    Workspace,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Measured off the bar this replaces: waybar's strip is 24 logical
            // pixels tall, and its digits carry 15 physical pixels of ink at
            // this monitor's 1.5 scale. `styles/fonts.css` asked for 16px and
            // that is what lands, so the islands are pills and the text is the
            // same size the CSS asked for.
            height: 24,
            font_size: 16.0,
            font_weight_bold: true,
            palette: "catppuccin-mocha".into(),
            terminal: "kitty".into(),
            overlay_layer: false,
            outputs: Vec::new(),
            // waybar's taskbar listed every toplevel the compositor handed it
            // and had no notion of a workspace to filter by, so the whole
            // display's windows is the behaviour being replaced.
            taskbar_scope: TaskbarScope::Output,
            // The layout this replaced: `~/.config/waybar/config.jsonc`
            // modules-left/center/right, module for module — with the workspace
            // dots moved off the left edge of the centre group, where they now
            // sit between the machine readings and the clock.
            left: vec![ModuleId::Launcher, ModuleId::Taskbar],
            center: vec![
                ModuleId::Cpu,
                ModuleId::Memory,
                ModuleId::Gpu,
                ModuleId::Workspaces,
                ModuleId::IdleInhibitor,
                ModuleId::Date,
                ModuleId::Time,
                ModuleId::Network,
                ModuleId::Bluetooth,
                ModuleId::Updates,
                ModuleId::Notifications,
                ModuleId::Tray,
            ],
            right: vec![
                ModuleId::Mpris,
                ModuleId::Volume,
                ModuleId::Brightness,
                ModuleId::Battery,
                ModuleId::Power,
            ],
            // Nothing by default: an extension is another program, and the
            // shipped bar spawns none.
            extensions: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("cosmicbar").join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(config) => {
                    log::info!("loaded {}", path.display());
                    config
                }
                Err(error) => {
                    log::error!("{}: {error}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::error!("{}: {error}; using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Modification time of the config file, if it exists.
    pub(crate) fn stamp() -> Option<std::time::SystemTime> {
        std::fs::metadata(Self::path())
            .ok()
            .and_then(|meta| meta.modified().ok())
    }

    /// Reload the layout when the config file changes, so editing it does not
    /// mean restarting the bar. This is one `stat` every two seconds: watching
    /// the file properly would mean an inotify dependency and re-arming the
    /// watch on every editor rename-in-place, which costs more than the stat.
    pub fn watch() -> cosmic::iced::Subscription<crate::bar::Message> {
        cosmic::iced::Subscription::run(|| {
            cosmic::iced::stream::channel(1, async move |mut sender| {
                use cosmic::iced::futures::SinkExt;

                let mut seen = Self::stamp();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let stamp = Self::stamp();
                    if stamp == seen {
                        continue;
                    }
                    seen = stamp;
                    log::info!("{} changed; reloading", Self::path().display());
                    if sender
                        .send(crate::bar::Message::Control(
                            crate::control::Command::Reload,
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            })
        })
    }

    pub fn palette(&self) -> crate::theme::Palette {
        crate::theme::Palette::by_name(&self.palette)
    }

    /// Every module placed in any region, in layout order.
    pub fn modules(&self) -> impl Iterator<Item = ModuleId> + '_ {
        self.left
            .iter()
            .chain(self.center.iter())
            .chain(self.right.iter())
            .copied()
    }

    pub fn wants(&self, module: ModuleId) -> bool {
        self.modules().any(|placed| placed == module)
    }

    /// The declaration behind an extension module, if the config still has one.
    pub fn extension(&self, module: ModuleId) -> Option<&Extension> {
        self.extensions
            .iter()
            .find(|entry| ModuleId::extension(&entry.name) == module)
    }
}
