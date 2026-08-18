//! Clock time. The bar's own tick drives it, so there is no timer here.

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::{Alignment, Subscription};

use crate::bar::Message;
use crate::modules::Ctx;
use crate::theme::Island;

pub const ISLAND: Island = Island::Join;

#[derive(Debug, Clone)]
pub enum Event {}

#[derive(Debug, Default)]
pub struct State;

impl State {
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {}
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        Some(
            crate::theme::text(crate::bar::local(ctx.now_ms).strftime("%H:%M").to_string())
                .align_y(Alignment::Center)
                .into(),
        )
    }

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
