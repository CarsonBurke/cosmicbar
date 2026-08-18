//! Distro button, replacing waybar's `custom/distro` badge and
//! `custom/trigger` (`on-click: walker`).
//!
//! Waybar could only fire one command per click, so the badge was decorative
//! and the launcher lived on a second module. Here the badge *is* the button:
//! it opens a quick-launch card whose first entry is the session launcher, and
//! entries whose binary is missing are left out instead of failing silently.
//!
//! Everything is started in its own process group with no stdio and reaped in
//! the background, so a launched app is never a child the bar has to wait on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use cosmic::app::Task;
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::theme::Island;

/// waybar: `#custom-trigger` sits directly on the bar background.
pub const ISLAND: Island = Island::Flat;

/// nf-linux-cachyos. Waybar's `custom/distro` used the generic Arch mark
/// (nf-md-arch); this machine is CachyOS and the font has its actual logo.
const ICON: &str = "\u{f385}";

/// Popup content width; see the note in `power.rs`.
const POPUP_WIDTH: f32 = 400.0;

/// The session launcher: niri binds Mod+D to it, waybar's trigger clicked it.
const LAUNCHER: &str = "walker";

#[derive(Debug, Clone)]
pub enum Event {
    Launch(Vec<String>),
    Launched(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    error: Option<String>,
}

impl State {
    /// Nothing to watch: the entries are decided by which binaries exist, and
    /// that is resolved once, lazily.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Launch(argv) => {
                self.error = None;
                Task::batch([
                    // The card has done its job; leaving it over the new window
                    // would be wrong.
                    Task::done(cosmic::Action::App(Message::ClosePopup)),
                    // Spawning happens inside the task: `tokio::process` needs
                    // the executor's runtime, which `update` does not run on.
                    Task::future(async move {
                        cosmic::Action::App(event_message(Event::Launched(
                            spawn_detached(&argv).map_err(|error| format!("{error:#}")),
                        )))
                    }),
                ])
            }
            Event::Launched(result) => {
                if let Err(error) = &result {
                    log::warn!("launch failed: {error}");
                }
                self.error = result.err();
                Task::none()
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let color = if self.error.is_some() {
            ctx.palette.red
        } else {
            ctx.palette.accent()
        };
        Some(
            crate::theme::glyph_only(ICON, ctx.font_size)
                .class(cosmic::theme::Text::Color(color))
                .align_y(Alignment::Center)
                .into(),
        )
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        true
    }

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let mut body = widget::Column::new()
            .spacing(2)
            .width(Length::Fixed(POPUP_WIDTH));

        for entry in entries(ctx) {
            // The glyph sits against its label at the bar's own ink gap, and the
            // command hangs off the right edge: one line per entry, whatever the
            // label's length.
            let row = widget::Row::new()
                .push(crate::theme::label(
                    entry.glyph,
                    entry.label,
                    ctx.font_size,
                    cosmic::theme::Text::Color(ctx.palette.fg()),
                ))
                .push(widget::space::horizontal())
                .push(
                    crate::theme::text(entry.hint)
                        .size(ctx.small())
                        .class(cosmic::theme::Text::Color(ctx.palette.muted())),
                )
                .spacing(10)
                .align_y(Alignment::Center);
            // Faded like a bar cell: `button` would snap between two greys the
            // width of the card.
            body = body.push(crate::fill::fill(
                row.apply(widget::button::custom)
                    .width(Length::Fill)
                    .padding([6, 10])
                    .class(crate::theme::cell(
                        ctx.palette.fg(),
                        crate::theme::ROW_CORNERS,
                    ))
                    .on_press(event_message(Event::Launch(entry.argv))),
                crate::theme::row_fill(ctx.palette),
                crate::theme::ROW_CORNERS,
            ));
        }

        if let Some(error) = &self.error {
            body = body.push(widget::divider::horizontal::default()).push(
                crate::theme::text(error.clone())
                    .size(ctx.small())
                    .class(cosmic::theme::Text::Color(ctx.palette.red)),
            );
        }

        Some(body.apply(widget::container).padding(10).into())
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Launcher(event))
}

struct Entry {
    glyph: &'static str,
    label: String,
    /// What will actually run, so the card never lies about the binary.
    hint: String,
    argv: Vec<String>,
}

impl Entry {
    fn new(glyph: &'static str, label: &str, target: Target) -> Self {
        Self {
            glyph,
            label: label.to_string(),
            hint: target.hint,
            argv: target.argv,
        }
    }
}

/// A resolved launch target: the command line, plus the name of the thing it
/// starts, which is what the card shows under the role label.
struct Target {
    hint: String,
    argv: Vec<String>,
}

impl Target {
    /// A binary on `PATH`, or an absolute path to one.
    fn binary(program: &str) -> Option<Self> {
        present(program).then(|| Self {
            hint: leaf(program).to_string(),
            argv: vec![program.to_string()],
        })
    }

    /// A desktop entry, given as `helium.desktop` or `helium`.
    ///
    /// An AppImage, a flatpak, or anything else installed without a binary on
    /// `PATH` can only be started this way, and its `Exec` line is not the bar's
    /// to reimplement: `gio launch` (glib) and `gtk-launch` (gtk) already handle
    /// field codes, `TryExec`, `Terminal=true` and startup notification.
    fn desktop(id: &str) -> Option<Self> {
        let id = id.strip_suffix(".desktop").unwrap_or(id);
        let file = data_dirs()
            .map(|dir| dir.join("applications").join(format!("{id}.desktop")))
            .find(|candidate| candidate.is_file())?;
        let name = desktop_name(&file).unwrap_or_else(|| id.to_string());
        let argv = if present("gio") {
            vec![
                "gio".to_string(),
                "launch".to_string(),
                file.to_string_lossy().into_owned(),
            ]
        } else if present("gtk-launch") {
            vec!["gtk-launch".to_string(), id.to_string()]
        } else {
            return None;
        };
        Some(Self { hint: name, argv })
    }
}

/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, with the spec's defaults. On this
/// session the list is where flatpak exports its entries, which is how a
/// flatpak app is found at all.
fn data_dirs() -> impl Iterator<Item = PathBuf> {
    let home = xdg_var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let dirs = xdg_var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&dirs).collect::<Vec<_>>())
}

/// An XDG base-directory variable. The spec is explicit that an empty value
/// means "unset", and treating `XDG_CONFIG_HOME=` as a directory would silently
/// resolve every lookup against the process's working directory.
fn xdg_var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// The entry's `Name`, which is the app's own name (`Helium`, `Resources`) and
/// reads better than the binary buried in its `Exec` line. Localised
/// `Name[xx]` keys and later groups (the desktop actions) are not it.
fn desktop_name(file: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines()
        .skip_while(|line| line.trim() != "[Desktop Entry]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .find_map(|line| line.strip_prefix("Name="))
        .map(|name| name.trim().to_string())
}

/// The quick-launch card: launcher first, then one entry per role that is
/// actually installed, whether as a binary or as a desktop entry.
fn entries(ctx: &Ctx) -> Vec<Entry> {
    /// nf-md-menu, waybar's `custom/trigger` glyph.
    const LAUNCHER_GLYPH: &str = "\u{f035c}";
    /// nf-md-console, nf-md-folder, nf-md-web, nf-md-chart-line.
    const TERMINAL_GLYPH: &str = "\u{f018d}";
    const FILES_GLYPH: &str = "\u{f024b}";
    const BROWSER_GLYPH: &str = "\u{f059f}";
    const MONITOR_GLYPH: &str = "\u{f012a}";

    let mut entries = Vec::with_capacity(5);
    if let Some(launcher) = Target::binary(LAUNCHER) {
        entries.push(Entry::new(LAUNCHER_GLYPH, "Applications", launcher));
    }
    if let Some(terminal) = Target::binary(&ctx.terminal) {
        entries.push(Entry::new(TERMINAL_GLYPH, "Terminal", terminal));
    }
    if let Some(files) = first_target(&["nautilus", "dolphin", "thunar", "nemo", "pcmanfm"]) {
        entries.push(Entry::new(FILES_GLYPH, "Files", files));
    }
    if let Some(browser) = browser() {
        entries.push(Entry::new(BROWSER_GLYPH, "Browser", browser));
    }
    if let Some(monitor) = monitor(ctx) {
        entries.push(Entry::new(MONITOR_GLYPH, "System monitor", monitor));
    }
    entries
}

/// Resources first: it is this session's system monitor, and it is a flatpak,
/// so only its desktop entry can start it. A TUI in the terminal is the last
/// resort, which is what the waybar power menu did.
fn monitor(ctx: &Ctx) -> Option<Target> {
    Target::binary("resources")
        .or_else(|| Target::desktop("net.nokyan.Resources"))
        .or_else(|| Target::binary("missioncenter"))
        .or_else(|| Target::desktop("io.missioncenter.MissionCenter"))
        .or_else(|| Target::binary("gnome-system-monitor"))
        .or_else(|| Target::binary("plasma-systemmonitor"))
        .or_else(|| {
            let tui = first_target(&["btop", "htop"])?;
            let terminal = Target::binary(&ctx.terminal)?;
            Some(Target {
                hint: tui.hint,
                // `-e` is how the waybar power menu opened its TUI in kitty.
                argv: vec![terminal.argv[0].clone(), "-e".to_string(), tui.argv[0].clone()],
            })
        })
}

/// `$BROWSER` wins, so a session that points somewhere unusual (an AppImage
/// behind a desktop entry, a wrapper script) is honoured first. Failing that,
/// the browser this session actually opens links with — which is the answer the
/// user expects, and on this machine is not any of the well-known names.
fn browser() -> Option<Target> {
    let configured = std::env::var("BROWSER")
        .ok()
        .filter(|browser| !browser.is_empty())
        .and_then(|browser| {
            if browser.ends_with(".desktop") {
                Target::desktop(&browser)
            } else {
                Target::binary(&browser)
            }
        });
    configured
        .or_else(|| default_handler("x-scheme-handler/https"))
        .or_else(|| {
            first_target(&[
                "firefox",
                "chromium",
                "google-chrome-stable",
                "brave",
                "epiphany",
                "qutebrowser",
            ])
        })
}

/// The session's default application for a mime type, read from the
/// `mimeapps.list` chain the way `xdg-settings` reads it: the user's config
/// first, then the system files, `[Default Applications]` ahead of
/// `[Added Associations]`. The first id whose desktop file is installed wins,
/// because a default left behind by an uninstalled app is not an application.
fn default_handler(mime: &str) -> Option<Target> {
    let config_home = xdg_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let config_dirs = xdg_var("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into());
    let lists = config_home
        .into_iter()
        .chain(std::env::split_paths(&config_dirs).collect::<Vec<_>>())
        .map(|dir| dir.join("mimeapps.list"))
        .chain(data_dirs().map(|dir| dir.join("applications").join("mimeapps.list")));

    for list in lists {
        let Ok(text) = std::fs::read_to_string(&list) else {
            continue;
        };
        for group in ["[Default Applications]", "[Added Associations]"] {
            // Keys are exact: `x-scheme-handler/http` must not answer for
            // `x-scheme-handler/https`, so the `=` is part of the match.
            let ids = text
                .lines()
                .skip_while(|line| line.trim() != group)
                .skip(1)
                .take_while(|line| !line.trim_start().starts_with('['))
                .find_map(|line| line.strip_prefix(mime)?.strip_prefix('='));
            let Some(ids) = ids else {
                continue;
            };
            if let Some(target) = ids
                .split(';')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .find_map(Target::desktop)
            {
                return Some(target);
            }
        }
    }
    None
}

fn first_target(candidates: &[&str]) -> Option<Target> {
    candidates.iter().find_map(|program| Target::binary(program))
}

fn leaf(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Is this program installed?
///
/// [`State::popup`] runs once per frame while the launcher popup is open, and
/// every row asks, so the answer is memoised: one `stat` per binary for the
/// life of the process, and a map lookup after that.
fn present(program: &str) -> bool {
    static CACHE: LazyLock<RwLock<HashMap<String, bool>>> =
        LazyLock::new(|| RwLock::new(HashMap::new()));

    if let Some(&found) = CACHE
        .read()
        .expect("launcher probe cache poisoned")
        .get(program)
    {
        return found;
    }
    let found = resolve(program).is_some();
    CACHE
        .write()
        .expect("launcher probe cache poisoned")
        .insert(program.to_string(), found);
    found
}

fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// Start `argv` detached: own process group, no stdio, reaped in the background
/// so the bar neither blocks on it nor leaves a zombie behind.
fn spawn_detached(argv: &[String]) -> anyhow::Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty command"))?;
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(false)
        .spawn()
        .map_err(|error| anyhow::anyhow!("{program}: {error}"))?;
    tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
    });
    Ok(())
}
