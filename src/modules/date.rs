//! Date, with a real calendar in its popup instead of a pango tooltip.

use cosmic::Element;
use cosmic::app::Task;
use cosmic::iced::Subscription;
use cosmic::widget;
use cosmic::widget::calendar::CalendarModel;
use jiff::civil::{Date, Weekday};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::theme::Island;

pub const ISLAND: Island = Island::Join;

/// nf-md-calendar-month
const ICON: &str = "\u{f0e17}";

#[derive(Debug, Clone)]
pub enum Event {
    PrevMonth,
    NextMonth,
    Select(Date),
}

#[derive(Debug)]
pub struct State {
    calendar: CalendarModel,
}

impl Default for State {
    fn default() -> Self {
        Self {
            calendar: CalendarModel::now(),
        }
    }
}

impl State {
    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::PrevMonth => self.calendar.visible = shift_month(self.calendar.visible, -1),
            Event::NextMonth => self.calendar.visible = shift_month(self.calendar.visible, 1),
            Event::Select(date) => self.calendar.selected = date,
        }
        Task::none()
    }

    pub fn view(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        Some(
            crate::theme::label(
                ICON,
                crate::bar::local(ctx.now_ms).strftime("%m-%d").to_string(),
                ctx.font_size,
                cosmic::theme::Text::Default,
            ),
        )
    }

    /// Mirrors `popup`'s own test, so the bar can ask which cells are
    /// clickable without building any popup's contents.
    pub fn has_popup(&self) -> bool {
        true
    }

    pub fn popup(&self, _ctx: &Ctx) -> Option<Element<'_, Message>> {
        Some(
            widget::calendar(
                &self.calendar,
                |date| Message::Module(ModuleEvent::Date(Event::Select(date))),
                || Message::Module(ModuleEvent::Date(Event::PrevMonth)),
                || Message::Module(ModuleEvent::Date(Event::NextMonth)),
                Weekday::Monday,
            )
            .into(),
        )
    }

    pub fn fast_tick(&self, _open: bool) -> bool {
        false
    }
}

fn shift_month(date: Date, months: i64) -> Date {
    let first = date.with().day(1).build().unwrap_or(date);
    first
        .checked_add(jiff::Span::new().months(months))
        .unwrap_or(date)
}
