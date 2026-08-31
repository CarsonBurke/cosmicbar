//! Power menu, replacing `scripts/power-menu.sh` (an fzf list in a spawned
//! kitty) with a popup of real buttons.
//!
//! Everything goes through logind on D-Bus instead of `systemctl`/`loginctl`,
//! and log out goes through niri's IPC. Choosing an available action executes
//! it directly; the menu itself is the deliberate interaction boundary.
//!
//! logind exposes no signal for `CanPowerOff`/`CanHibernate`, so capabilities
//! are queried while the popup is open (see [`State::subscription`]) and
//! refreshed on a slow timer; a machine that gains or loses swap is rare.

use std::time::Duration;

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::futures::{SinkExt, Stream};
use cosmic::iced::{Alignment, Subscription};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card};
use crate::theme::Island;

/// A flat system-foreground control, matching the launcher at the other edge.
pub const ISLAND: Island = Island::Flat;

/// nf-md-power. Waybar used nf-md-power-sleep (a crescent moon), which now
/// belongs to the Suspend entry inside the menu; the bar cell gets the actual
/// power symbol, because that is what the menu is.
const ICON: &str = "\u{f0425}";

/// Capability re-query interval while the popup is open.
const CAPABILITY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Lock,
    LogOut,
    Suspend,
    Hibernate,
    Reboot,
    PowerOff,
}

impl Action {
    /// Menu order, mirroring the waybar script's list with lock first.
    const MENU: [Self; 6] = [
        Self::Lock,
        Self::LogOut,
        Self::Suspend,
        Self::Hibernate,
        Self::Reboot,
        Self::PowerOff,
    ];

    fn glyph(self) -> &'static str {
        match self {
            // nf-md-lock, nf-md-logout, nf-md-sleep, nf-md-snowflake,
            // nf-md-restart, nf-md-power
            Self::Lock => "\u{f033e}",
            Self::LogOut => "\u{f0343}",
            Self::Suspend => "\u{f04b2}",
            Self::Hibernate => "\u{f0717}",
            Self::Reboot => "\u{f0709}",
            Self::PowerOff => "\u{f0425}",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Lock => "Lock",
            Self::LogOut => "Log out",
            Self::Suspend => "Suspend",
            Self::Hibernate => "Hibernate",
            Self::Reboot => "Reboot",
            Self::PowerOff => "Power off",
        }
    }

}

/// logind's `Can*` answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Capability {
    /// Not asked yet: assume the action works, the way the old script did.
    #[default]
    Unknown,
    Yes,
    /// Allowed, but polkit will want authentication.
    Challenge,
    /// Refused by policy.
    No,
    /// Not available on this machine at all (no swap, no firmware support).
    NotAvailable,
}

impl Capability {
    fn parse(answer: &str) -> Self {
        match answer {
            "yes" => Self::Yes,
            "challenge" => Self::Challenge,
            "na" => Self::NotAvailable,
            _ => Self::No,
        }
    }

    fn usable(self) -> bool {
        matches!(self, Self::Unknown | Self::Yes | Self::Challenge)
    }

    /// logind refuses a non-interactive call it wants authentication for.
    fn interactive(self) -> bool {
        self == Self::Challenge
    }

    fn note(self) -> Option<&'static str> {
        match self {
            Self::NotAvailable => Some("unavailable"),
            Self::No => Some("not permitted"),
            Self::Challenge => Some("asks for password"),
            Self::Unknown | Self::Yes => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    suspend: Capability,
    hibernate: Capability,
    reboot: Capability,
    power_off: Capability,
}

#[derive(Debug, Clone)]
pub enum Event {
    Capabilities(Capabilities),
    Fire(Action),
    Done(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct State {
    capabilities: Capabilities,
    error: Option<String>,
}

impl State {
    /// Capabilities are only interesting while the menu is visible, so the
    /// query runs exactly then.
    pub fn subscription(&self, open: bool) -> Subscription<Message> {
        if open {
            Subscription::run(capabilities)
        } else {
            Subscription::none()
        }
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Capabilities(capabilities) => {
                self.capabilities = capabilities;
                Task::none()
            }
            Event::Fire(action) => {
                self.error = None;
                let interactive = self.capability(action).interactive();
                Task::batch([
                    // Leaving the menu open over a suspend or a shutdown would
                    // be wrong; the action is decided, so the popup goes away.
                    Task::done(cosmic::Action::App(Message::ClosePopup)),
                    Task::future(async move {
                        cosmic::Action::App(event_message(Event::Done(
                            fire(action, interactive)
                                .await
                                .map_err(|error| format!("{error:#}")),
                        )))
                    }),
                ])
            }
            Event::Done(result) => {
                if let Err(error) = &result {
                    log::warn!("power action failed: {error}");
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
            ctx.palette.fg()
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
        let palette = ctx.palette;
        let mut menu = popup::column();

        for action in Action::MENU {
            let capability = self.capability(action);
            let usable = capability.usable();
            let note: Option<Element<'_, Message>> = capability
                .note()
                .map(|note| popup::detail(note, ctx).into());
            menu = menu.push(popup::row(
                popup::split(
                    crate::theme::label(
                        action.glyph(),
                        action.label(),
                        ctx.body(),
                        // An action the system cannot do reads as dimmed, which
                        // the label has to carry itself: the row's own text
                        // colour never reaches it.
                        cosmic::theme::Text::Color(match usable {
                            true => palette.fg(),
                            false => palette.muted(),
                        }),
                    ),
                    note,
                ),
                palette,
                usable.then(|| event_message(Event::Fire(action))),
            ));
        }

        Some(
            Card::new()
                .block(menu)
                .maybe(self.error.as_ref().map(|error| {
                    popup::detail(error.as_str(), ctx)
                        .class(cosmic::theme::Text::Color(palette.red))
                }))
                .build(),
        )
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }

    fn capability(&self, action: Action) -> Capability {
        match action {
            Action::Suspend => self.capabilities.suspend,
            Action::Hibernate => self.capabilities.hibernate,
            Action::Reboot => self.capabilities.reboot,
            Action::PowerOff => self.capabilities.power_off,
            // logind has no capability for either, and both always work.
            Action::Lock | Action::LogOut => Capability::Unknown,
        }
    }

}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Power(event))
}

fn capabilities() -> impl Stream<Item = Message> {
    cosmic::iced::stream::channel(1, async move |mut sender| {
        loop {
            match query_capabilities().await {
                Ok(capabilities) => {
                    if sender
                        .send(event_message(Event::Capabilities(capabilities)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                // No logind: every entry stays optimistically enabled and a
                // click reports the real error.
                Err(error) => log::debug!("logind capabilities: {error:#}"),
            }
            tokio::time::sleep(CAPABILITY_INTERVAL).await;
        }
    })
}

async fn query_capabilities() -> anyhow::Result<Capabilities> {
    let connection = zbus::Connection::system().await?;
    let manager = Login1ManagerProxy::new(&connection).await?;
    Ok(Capabilities {
        suspend: Capability::parse(&manager.can_suspend().await?),
        hibernate: Capability::parse(&manager.can_hibernate().await?),
        reboot: Capability::parse(&manager.can_reboot().await?),
        power_off: Capability::parse(&manager.can_power_off().await?),
    })
}

async fn fire(action: Action, interactive: bool) -> anyhow::Result<()> {
    match action {
        Action::LogOut => quit_niri().await,
        Action::Lock => lock_session().await,
        Action::Suspend | Action::Hibernate | Action::Reboot | Action::PowerOff => {
            let connection = zbus::Connection::system().await?;
            let manager = Login1ManagerProxy::new(&connection).await?;
            match action {
                Action::Suspend => manager.suspend(interactive).await?,
                Action::Hibernate => manager.hibernate(interactive).await?,
                Action::Reboot => manager.reboot(interactive).await?,
                Action::PowerOff => manager.power_off(interactive).await?,
                Action::Lock | Action::LogOut => unreachable!(),
            }
            Ok(())
        }
    }
}

/// Log out immediately once its menu row is chosen, matching the other power
/// actions.
async fn quit_niri() -> anyhow::Result<()> {
    tokio::task::spawn_blocking(|| -> anyhow::Result<()> {
        let mut socket = niri_ipc::socket::Socket::connect()?;
        let reply = socket.send(niri_ipc::Request::Action(niri_ipc::Action::Quit {
            skip_confirmation: true,
        }))?;
        reply.map_err(|message| anyhow::anyhow!("niri: {message}"))?;
        Ok(())
    })
    .await?
}

/// `Manager.LockSession`, which is what `loginctl lock-session` calls: the
/// session-wide, correct way to lock.
///
/// Nothing on a bare niri session necessarily listens for that signal (a
/// screen locker is usually started from a keybind), so if logind's
/// `LockedHint` has not gone true shortly afterwards, the configured locker is
/// started as a fallback. A session with a real lock handler never reaches it,
/// so the screen is never locked twice.
async fn lock_session() -> anyhow::Result<()> {
    /// Lockers in preference order; the first one installed wins.
    const LOCKERS: [&str; 4] = ["swaylock", "hyprlock", "gtklock", "waylock"];

    let connection = zbus::Connection::system().await?;
    let manager = Login1ManagerProxy::new(&connection).await?;
    let path = manager.get_session_by_pid(std::process::id()).await?;
    let session = Login1SessionProxy::builder(&connection)
        .path(path)?
        .build()
        .await?;
    let id = session.id().await?;
    manager.lock_session(&id).await?;

    tokio::time::sleep(Duration::from_millis(1200)).await;
    if session.locked_hint().await.unwrap_or(false) {
        return Ok(());
    }
    let Some(locker) = LOCKERS.into_iter().find(|locker| which(locker).is_some()) else {
        anyhow::bail!("session locked, but no locker is listening and none is installed");
    };
    log::info!("no lock handler answered LockSession; starting {locker}");
    spawn_detached(locker, &[])
}

/// First match for `binary` in `PATH`, or `None`.
fn which(binary: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

/// Start a program in its own process group with no stdio, then reap it in the
/// background so the bar never collects a zombie and never waits on a child.
fn spawn_detached(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(false)
        .spawn()?;
    tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
    });
    Ok(())
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait Login1Manager {
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;
    fn reboot(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
    fn lock_session(&self, session_id: &str) -> zbus::Result<()>;
    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn can_suspend(&self) -> zbus::Result<String>;
    fn can_hibernate(&self) -> zbus::Result<String>;
    fn can_reboot(&self) -> zbus::Result<String>;
    fn can_power_off(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
trait Login1Session {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}
