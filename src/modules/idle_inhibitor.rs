//! Idle inhibitor: keep the session awake while engaged.
//!
//! Waybar's `idle_inhibitor` only flips an icon and asks the compositor's idle
//! protocol; this holds a real logind inhibitor lock (`what="idle:sleep"`,
//! mode `block`), so both the idle timer *and* automatic suspend are held off
//! for every consumer that respects logind, not just the compositor.
//!
//! The lock lives exactly as long as the file descriptor logind handed us:
//! engaging stores it, disengaging drops it, and process exit closes it. There
//! is nothing to poll and nothing to clean up.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use cosmic::app::Task;
use cosmic::iced::{Alignment, Subscription, mouse};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::theme::Island;

/// waybar: `@time` → surface0, same island as the clock it sits next to.
pub const ISLAND: Island = Island::Start;

/// nf-md-eye, waybar's `format-icons.activated`.
const ENGAGED: &str = "\u{f0208}";
/// nf-md-eye-off, waybar's `format-icons.deactivated`.
const RELEASED: &str = "\u{f0209}";

/// An inhibitor lock: logind keeps it while this fd is open. The connection is
/// parked alongside it because logind drops the locks of a client that
/// disappears from the bus, so the fd alone is not enough.
pub struct Lock {
    _fd: OwnedFd,
    _connection: zbus::Connection,
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Lock")
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Toggle,
    /// logind granted the lock; the bar owns it from here.
    Engaged(Arc<Lock>),
    Failed(String),
}

#[derive(Debug, Default)]
pub struct State {
    lock: Option<Arc<Lock>>,
    /// An `Inhibit` call is in flight; further clicks are ignored until it lands.
    pending: bool,
    error: Option<String>,
}

impl State {
    /// No subscription: the state changes only when the user clicks, and the
    /// lock needs no upkeep.
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Toggle => {
                if self.pending {
                    return Task::none();
                }
                self.error = None;
                if self.lock.take().is_some() {
                    // Dropping the fd releases the lock; logind needs no call.
                    return Task::none();
                }
                self.pending = true;
                Task::future(async move {
                    cosmic::Action::App(event_message(match inhibit().await {
                        Ok(lock) => Event::Engaged(Arc::new(lock)),
                        Err(error) => Event::Failed(format!("{error:#}")),
                    }))
                })
            }
            Event::Engaged(lock) => {
                self.pending = false;
                self.lock = Some(lock);
                Task::none()
            }
            Event::Failed(error) => {
                self.pending = false;
                log::warn!("idle inhibitor: {error}");
                self.error = Some(error);
                Task::none()
            }
        }
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        let (glyph, color) = match (&self.error, self.lock.is_some()) {
            (Some(_), _) => (RELEASED, ctx.palette.red),
            // waybar `#idle_inhibitor.deactivated { color: @hover-fg }`: the
            // engaged state is the loud one.
            (None, true) => (ENGAGED, ctx.palette.accent()),
            (None, false) => (RELEASED, ctx.palette.muted()),
        };
        Some(
            crate::theme::glyph_only(glyph, ctx.font_size)
                .class(cosmic::theme::Text::Color(color))
                .align_y(Alignment::Center)
                .apply(widget::mouse_area)
                .on_press(event_message(Event::Toggle))
                .interaction(mouse::Interaction::Pointer)
                .into(),
        )
    }

    /// Deliberately `None`: the bar opens popups on click, and this module's
    /// click is the toggle. The `mouse_area` in [`State::view`] handles it.
    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        false
    }

    pub fn popup(&self, _ctx: &Ctx) -> Option<Element<'_, Message>> {
        None
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::IdleInhibitor(event))
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
trait Login1Manager {
    /// Returns the pipe fd whose lifetime is the lock's lifetime.
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

async fn inhibit() -> anyhow::Result<Lock> {
    let connection = zbus::Connection::system().await?;
    let manager = Login1ManagerProxy::new(&connection).await?;
    let fd = manager
        .inhibit(
            "idle:sleep",
            "cosmicbar",
            "Idle inhibited from the bar",
            "block",
        )
        .await?;
    Ok(Lock {
        _fd: fd.into(),
        _connection: connection,
    })
}
