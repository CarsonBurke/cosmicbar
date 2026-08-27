//! Declarative bar configuration: `$XDG_CONFIG_HOME/cosmicbar/config.toml`.
//!
//! A malformed or missing config never prevents the bar from coming up; the
//! defaults below are the shipped layout.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use cosmic::iced::futures::{SinkExt, StreamExt};
use inotify::{EventOwned, EventStream, Inotify, WatchMask};
use serde::Deserialize;

use crate::modules::ModuleId;

/// What a save looks like, however the editor performs it: `CLOSE_WRITE` for a
/// write in place, `MOVED_TO` for the write-a-temporary-and-rename every careful
/// editor does, and the two removals so deleting the config falls back to the
/// shipped defaults. `MODIFY` is deliberately absent: it fires while the writer
/// is still going, and half a TOML file is a parse error, not a config.
const FILE_EVENTS: WatchMask = WatchMask::CLOSE_WRITE
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::DELETE);
/// The config's own directory coming or going. Removing it takes the config with
/// it, which is a fall back to the defaults, and the same event re-arms the
/// directory watch when it returns.
const DIR_EVENTS: WatchMask = WatchMask::CREATE
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::DELETE);
/// One save is a burst of events; this collapses it into a single reload.
const COALESCE: Duration = Duration::from_millis(150);
/// Interval for the fallback watcher, and the fallback watcher only.
const POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Bar height in logical pixels; also the exclusive zone.
    pub height: u32,
    /// Base text size in logical pixels.
    pub font_size: f32,
    /// Use bold weight for system text and Nerd Font icons.
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
    /// Which built-in modules the three regions place, one bit per
    /// [`ModuleId::bit`]. Derived, never read from the file: iced recomputes
    /// every subscription after every message the bar handles, and answering
    /// [`Config::wants`] for each of nineteen modules — twice over, once for
    /// the subscription list and once for the fast clock — by scanning three
    /// placement lists is a few hundred comparisons per frame to learn
    /// something that only changes when the file does.
    #[serde(skip)]
    placed: u64,
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
            placed: 0,
        }
        .indexed()
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
                    Self::indexed(config)
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
    fn stamp() -> Option<std::time::SystemTime> {
        std::fs::metadata(Self::path())
            .ok()
            .and_then(|meta| meta.modified().ok())
    }

    /// The directory the config file lives in.
    fn dir() -> PathBuf {
        Self::path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Reload the layout when the config file changes, so editing it does not
    /// mean restarting the bar. The kernel reports the write; nothing here runs
    /// on an interval unless inotify itself is unavailable.
    pub fn watch() -> cosmic::iced::Subscription<crate::bar::Message> {
        cosmic::iced::Subscription::run(|| {
            cosmic::iced::stream::channel(1, async move |mut sender| {
                match Self::inotify() {
                    Ok(events) => Self::on_events(events, &mut sender).await,
                    // inotify instances and watches are per-user limits that
                    // another program can exhaust. A bar that stops noticing
                    // config edits is worse than a `stat` every two seconds.
                    Err(error) => {
                        log::warn!(
                            "watching {}: {error}; polling instead",
                            Self::path().display()
                        );
                        Self::on_stamp(&mut sender).await;
                    }
                }
            })
        })
    }

    /// inotify for the config file, watching its *directory*: an editor saves by
    /// writing a temporary file and renaming it over the target, so the inode a
    /// watch on the file would hold is not the inode that ends up on disk. A
    /// directory watch survives that, and reports a config written for the first
    /// time. `~/.config` is watched as well, for the one thing the directory
    /// watch cannot report: that directory itself appearing.
    fn inotify() -> std::io::Result<EventStream<[u8; 1024]>> {
        let dir = Self::dir();
        let inotify = Inotify::init()?;
        let mut watches = inotify.watches();
        let watched = watches.add(&dir, FILE_EVENTS);
        let parent = match dir.parent() {
            Some(parent) => watches.add(parent, DIR_EVENTS).is_ok(),
            None => false,
        };
        // A config directory that does not exist yet is not a failure: the
        // parent watch reports it appearing and the watch is added then. Neither
        // directory existing is, because there is nothing to hang a watch on.
        if let Err(error) = watched
            && !parent
        {
            return Err(error);
        }
        inotify.into_event_stream([0; 1024])
    }

    /// One reload per save, whatever burst of events the editor produced.
    async fn on_events(
        mut events: EventStream<[u8; 1024]>,
        sender: &mut cosmic::iced::futures::channel::mpsc::Sender<crate::bar::Message>,
    ) {
        let dir = Self::dir();
        let name = dir.file_name().map(OsStr::to_owned);
        // `path` is two components joined onto a base, so it always has a file
        // name; without one there is no file to watch for.
        let Some(file) = Self::path().file_name().map(OsStr::to_owned) else {
            return;
        };
        loop {
            // Wait for the config file, ignoring the rest of the directory.
            loop {
                match events.next().await {
                    Some(Ok(event)) => {
                        let directory = Self::rearm(&event, &dir, name.as_deref(), &events);
                        if directory || event.name.as_deref() == Some(file.as_os_str()) {
                            break;
                        }
                    }
                    // A read error, or the instance closing: no watch, and so no
                    // reloads. The bar keeps the config it has.
                    Some(Err(error)) => {
                        log::warn!("watching {}: {error}", dir.display());
                        return;
                    }
                    None => return,
                }
            }
            // A save is several events - the temporary file, the rename over the
            // target, an editor's backup - and each one would cost a relayout.
            while let Ok(Some(Ok(event))) = tokio::time::timeout(COALESCE, events.next()).await {
                Self::rearm(&event, &dir, name.as_deref(), &events);
            }
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
    }

    /// Re-arm the directory watch when the config directory itself appears, and
    /// say whether the event was about it.
    ///
    /// A directory that is created, or replaced by a rename, is a different inode
    /// from the one the previous watch held, and inotify drops a watch with its
    /// inode. `add` is keyed by inode, so re-adding a watch that is still live
    /// costs a syscall and changes nothing. What moved in may contain a config,
    /// which is why this is also a reason to reload.
    fn rearm(
        event: &EventOwned,
        dir: &std::path::Path,
        name: Option<&OsStr>,
        events: &EventStream<[u8; 1024]>,
    ) -> bool {
        if name.is_none() || event.name.as_deref() != name {
            return false;
        }
        if let Err(error) = events.watches().add(dir, FILE_EVENTS) {
            log::debug!("watching {}: {error}", dir.display());
        }
        true
    }

    /// The fallback when inotify is unavailable: one `stat` every two seconds.
    async fn on_stamp(
        sender: &mut cosmic::iced::futures::channel::mpsc::Sender<crate::bar::Message>,
    ) {
        let mut seen = Self::stamp();
        loop {
            tokio::time::sleep(POLL).await;
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

    /// The placed built-in modules, as a bitmask. Both callers of this are
    /// construction sites: the mask is derived from the placement lists, and
    /// `wants` asserts in debug builds that it still agrees with them.
    fn mask(&self) -> u64 {
        debug_assert!(
            ModuleId::ALL.len() <= u64::BITS as usize,
            "more built-in modules than bits in the placed mask"
        );
        self.modules()
            .filter_map(ModuleId::bit)
            .fold(0, |mask, bit| mask | 1 << bit)
    }

    fn indexed(mut self) -> Self {
        self.placed = self.mask();
        self
    }

    pub fn wants(&self, module: ModuleId) -> bool {
        debug_assert_eq!(
            self.placed,
            self.mask(),
            "the placed mask is stale: a placement list changed after construction"
        );
        match module.bit() {
            Some(bit) => self.placed & 1 << bit != 0,
            // An extension is named by the config, so it has no bit; there are
            // as many declared extensions as the user asked for and no more.
            None => self.modules().any(|placed| placed == module),
        }
    }

    /// The declaration behind an extension module, if the config still has one.
    pub fn extension(&self, module: ModuleId) -> Option<&Extension> {
        self.extensions
            .iter()
            .find(|entry| ModuleId::extension(&entry.name) == module)
    }
}
