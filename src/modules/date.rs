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
    follows_selected_day: bool,
    follows_visible_day: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::at(jiff::Zoned::now().date())
    }
}

impl State {
    fn at(today: Date) -> Self {
        Self {
            calendar: CalendarModel::new(today, today),
            follows_selected_day: true,
            follows_visible_day: true,
        }
    }

    /// Advance untouched calendar fields with the wall clock while preserving
    /// any month or day the user deliberately chose.
    pub(crate) fn sync_today(&mut self, today: Date) {
        if self.follows_selected_day {
            self.calendar.selected = today;
        }
        if self.follows_visible_day {
            self.calendar.visible = today;
        }
    }

    pub fn subscription(&self, _open: bool) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn update(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::PrevMonth => {
                self.calendar.visible = shift_month(self.calendar.visible, -1);
                self.follows_visible_day = false;
            }
            Event::NextMonth => {
                self.calendar.visible = shift_month(self.calendar.visible, 1);
                self.follows_visible_day = false;
            }
            Event::Select(date) => {
                self.calendar.selected = date;
                self.follows_selected_day = false;
            }
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

    /// The calendar widget draws its own month header and arrows, so the card
    /// adds no title of its own — just the inset every other popup has, which
    /// this one went without.
    pub fn popup(&self, _ctx: &Ctx) -> Option<Element<'_, Message>> {
        Some(
            crate::popup::Card::new()
                .block(widget::calendar(
                    &self.calendar,
                    |date| Message::Module(ModuleEvent::Date(Event::Select(date))),
                    || Message::Module(ModuleEvent::Date(Event::PrevMonth)),
                    || Message::Module(ModuleEvent::Date(Event::NextMonth)),
                    Weekday::Monday,
                ))
                .build(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_calendar_advances_to_current_weekday() {
        let tuesday = Date::new(2026, 8, 18).unwrap();
        let thursday = Date::new(2026, 8, 20).unwrap();
        let mut state = State::at(tuesday);

        state.sync_today(thursday);

        assert_eq!(state.calendar.selected, thursday);
        assert_eq!(state.calendar.visible, thursday);
        assert_eq!(state.calendar.visible.weekday(), Weekday::Thursday);
    }

    #[test]
    fn calendar_keeps_dates_chosen_by_the_user() {
        let mut state = State::at(Date::new(2026, 8, 18).unwrap());
        let selected = Date::new(2026, 7, 12).unwrap();
        drop(state.update(Event::PrevMonth));
        let visible = state.calendar.visible;
        drop(state.update(Event::Select(selected)));

        state.sync_today(Date::new(2026, 8, 20).unwrap());

        assert_eq!(state.calendar.selected, selected);
        assert_eq!(state.calendar.visible, visible);
    }

    #[test]
    fn selecting_today_stops_the_selection_following_the_clock() {
        let tuesday = Date::new(2026, 8, 18).unwrap();
        let thursday = Date::new(2026, 8, 20).unwrap();
        let mut state = State::at(tuesday);
        drop(state.update(Event::Select(tuesday)));

        state.sync_today(thursday);

        assert_eq!(state.calendar.selected, tuesday);
        assert_eq!(state.calendar.visible, thursday);
    }
}
