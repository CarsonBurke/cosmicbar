//! Date, with a real calendar in its popup instead of a pango tooltip.

use std::process::Stdio;

use cosmic::{Apply, Element};
use cosmic::app::Task;
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::widget;
use cosmic::widget::calendar::{CalendarModel, get_calendar_first};
use jiff::ToSpan;
use jiff::civil::{Date, Weekday};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleEvent};
use crate::popup::{self, Card, Chip};
use crate::theme::Island;

pub const ISLAND: Island = Island::Join;

/// nf-md-calendar-month
const ICON: &str = "\u{f0e17}";

#[derive(Debug, Clone)]
pub enum Event {
    PrevMonth,
    NextMonth,
    Select(Date),
    Today(Date),
    Copy(Date),
    Open(Date),
    Opened(Result<(), String>),
}

#[derive(Debug)]
pub struct State {
    calendar: CalendarModel,
    follows_selected_day: bool,
    follows_visible_day: bool,
    copied: Option<Date>,
    error: Option<String>,
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
            copied: None,
            error: None,
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
                self.move_month(-1);
                Task::none()
            }
            Event::NextMonth => {
                self.move_month(1);
                Task::none()
            }
            Event::Select(date) => {
                if date.month() != self.calendar.visible.month()
                    || date.year() != self.calendar.visible.year()
                {
                    self.calendar.visible = date;
                }
                self.calendar.selected = date;
                self.follows_selected_day = false;
                self.follows_visible_day = false;
                self.copied = None;
                Task::none()
            }
            Event::Today(today) => {
                self.calendar.selected = today;
                self.calendar.visible = today;
                self.follows_selected_day = true;
                self.follows_visible_day = true;
                self.copied = None;
                Task::none()
            }
            Event::Copy(date) => {
                self.copied = Some(date);
                cosmic::iced::clipboard::write(numeric_date(date))
            }
            Event::Open(date) => {
                self.error = None;
                Task::future(async move {
                    cosmic::Action::App(event_message(Event::Opened(
                        spawn_calendar(date).map_err(|error| format!("{error:#}")),
                    )))
                })
            }
            Event::Opened(Ok(())) => {
                Task::done(cosmic::Action::App(Message::ClosePopup))
            }
            Event::Opened(Err(error)) => {
                log::warn!("calendar launch failed: {error}");
                self.error = Some(error);
                Task::none()
            }
        }
    }

    fn move_month(&mut self, months: i64) {
        let month = shift_month(self.calendar.visible, months);
        self.calendar.visible = select_in_month(month, self.calendar.selected.day());
        self.calendar.selected = self.calendar.visible;
        self.follows_selected_day = false;
        self.follows_visible_day = false;
        self.copied = None;
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

    pub fn popup(&self, ctx: &Ctx) -> Option<Element<'_, Message>> {
        const PREVIOUS: &str = "\u{f0141}";
        const NEXT: &str = "\u{f0142}";

        let heading = widget::Row::new()
            .push(popup::title(month_name(self.calendar.visible), ctx))
            .push(popup::section(
                weekday(self.calendar.selected.weekday()),
                ctx,
            ))
            .spacing(popup::ROW_GAP)
            .align_y(Alignment::Center);
        let header = popup::split(
            heading,
            [
                popup::icon_chip(
                    PREVIOUS,
                    Chip::Plain,
                    ctx,
                    Some(event_message(Event::PrevMonth)),
                ),
                popup::icon_chip(
                    NEXT,
                    Chip::Plain,
                    ctx,
                    Some(event_message(Event::NextMonth)),
                ),
                popup::popout(ctx, Some(event_message(Event::Open(self.calendar.selected)))),
            ],
        );

        let mut weekdays = widget::Row::new().spacing(DAY_GAP);
        for label in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] {
            weekdays = weekdays.push(
                popup::section(label, ctx)
                    .width(Length::Fixed(DAY_SIZE))
                    .align_x(Alignment::Center),
            );
        }

        let today = crate::bar::local(ctx.now_ms).date();
        let (first, weeks) = month_grid(self.calendar.visible);
        let mut week_rows = widget::Column::new().spacing(popup::GAP);
        for week in 0..weeks {
            let mut days = widget::Row::new().spacing(DAY_GAP);
            for day in 0..7 {
                let offset = i64::try_from(week * 7 + day).unwrap_or_default();
                let date = first.checked_add(offset.days()).unwrap_or(first);
                let in_month = date.month() == self.calendar.visible.month()
                    && date.year() == self.calendar.visible.year();
                days = days.push(day_button(
                    date,
                    in_month,
                    date == self.calendar.selected,
                    date == today,
                    ctx,
                ));
            }
            week_rows = week_rows.push(days);
        }

        let calendar = widget::Column::new()
            .spacing(popup::GAP)
            .push(weekdays)
            .push(week_rows)
            .apply(widget::container)
            .width(Length::Fill)
            .align_x(Alignment::Center);

        let week = self.calendar.selected.iso_week_date().week();
        let status = match self.copied == Some(self.calendar.selected) {
            true => format!("copied to clipboard · week {week}"),
            false => format!(
                "{} · week {week}",
                relative_date(self.calendar.selected, today)
            ),
        };
        let mut actions: Vec<Element<'_, Message>> = vec![popup::chip(
            numeric_date(self.calendar.selected),
            Chip::Plain,
            ctx,
            Some(event_message(Event::Copy(self.calendar.selected))),
        )];
        if self.calendar.selected != today {
            actions.push(popup::chip(
                "today",
                Chip::Plain,
                ctx,
                Some(event_message(Event::Today(today))),
            ));
        }
        let selection = popup::detail(status, ctx);

        Some(
            Card::new()
                .block(header)
                .block(calendar)
                .block(popup::split(selection, actions))
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
}

const DAY_SIZE: f32 = 28.0;
const DAY_GAP: f32 = 4.0;
const DAY_RADIUS: f32 = 7.0;
const CALENDAR_APP: &str = "gnome-calendar";

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn month_name(date: Date) -> String {
    format!("{} {}", month(date), date.year())
}


fn month(date: Date) -> &'static str {
    let index = usize::try_from(date.month().saturating_sub(1)).unwrap_or_default();
    MONTHS[index]
}

fn weekday(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Monday",
        Weekday::Tuesday => "Tuesday",
        Weekday::Wednesday => "Wednesday",
        Weekday::Thursday => "Thursday",
        Weekday::Friday => "Friday",
        Weekday::Saturday => "Saturday",
        Weekday::Sunday => "Sunday",
    }
}

fn numeric_date(date: Date) -> String {
    format!("{:02}-{:02}-{:04}", date.day(), date.month(), date.year())
}

fn relative_date(date: Date, today: Date) -> String {
    let days = date.duration_since(today).as_hours() / 24;
    match days {
        0 => "today".into(),
        1 => "tomorrow".into(),
        -1 => "yesterday".into(),
        days if days > 0 => format!("in {days} days"),
        days => format!("{} days ago", -days),
    }
}

fn month_grid(visible: Date) -> (Date, usize) {
    let first_of_month = visible.with().day(1).build().unwrap_or(visible);
    let first = get_calendar_first(
        first_of_month.year(),
        first_of_month.month(),
        Weekday::Monday,
    );
    let leading = usize::try_from(first_of_month.duration_since(first).as_hours() / 24)
        .unwrap_or_default();
    (first, (leading + days_in_month(first_of_month)).div_ceil(7))
}

fn days_in_month(month: Date) -> usize {
    let next = shift_month(month, 1);
    let last = next.checked_sub(1.days()).unwrap_or(month);
    usize::try_from(last.day()).unwrap_or(31)
}

fn select_in_month(month: Date, day: i8) -> Date {
    let day = day.min(i8::try_from(days_in_month(month)).unwrap_or(31));
    month.with().day(day).build().unwrap_or(month)
}

fn day_button<'a>(
    date: Date,
    in_month: bool,
    selected: bool,
    today: bool,
    ctx: &Ctx,
) -> Element<'a, Message> {
    let palette = ctx.palette;
    let color = if selected {
        palette.crust
    } else if today {
        palette.accent()
    } else if in_month {
        palette.fg()
    } else {
        palette.overlay0
    };
    let label = crate::theme::text(date.day().to_string())
        .size(ctx.body())
        .class(cosmic::theme::Text::Color(color))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    let content = widget::container(label)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    let button = widget::button::custom(content)
        .padding(0)
        .width(Length::Fixed(DAY_SIZE))
        .height(Length::Fixed(DAY_SIZE))
        .class(crate::theme::cell(color, [DAY_RADIUS; 4]))
        .on_press(event_message(Event::Select(date)));

    let base = if selected {
        Some(palette.accent())
    } else if today {
        Some(palette.hover_over(palette.base))
    } else {
        None
    };
    crate::fill::fill(
        button,
        crate::fill::Fill {
            base,
            over: Some(palette.hover_over(base.unwrap_or(palette.base))),
            pressed: Some(palette.press_over(base.unwrap_or(palette.base))),
        },
        [DAY_RADIUS; 4],
    )
}

fn shift_month(date: Date, months: i64) -> Date {
    let first = date.with().day(1).build().unwrap_or(date);
    first
        .checked_add(jiff::Span::new().months(months))
        .unwrap_or(date)
}

fn event_message(event: Event) -> Message {
    Message::Module(ModuleEvent::Date(event))
}

fn spawn_calendar(date: Date) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new(CALENDAR_APP)
        .arg("--date")
        .arg(date.strftime("%Y-%m-%d").to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(false)
        .spawn()
        .map_err(|error| anyhow::anyhow!("{CALENDAR_APP}: {error}"))?;
    tokio::spawn(async move {
        if let Err(error) = child.wait().await {
            log::warn!("{CALENDAR_APP} wait failed: {error}");
        }
    });
    Ok(())
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
        assert_eq!(state.calendar.visible.month(), visible.month());
        assert_eq!(state.calendar.visible.year(), visible.year());
    }

    #[test]
    fn selecting_a_date_stops_the_calendar_following_the_clock() {
        let tuesday = Date::new(2026, 8, 18).unwrap();
        let thursday = Date::new(2026, 8, 20).unwrap();
        let mut state = State::at(tuesday);
        drop(state.update(Event::Select(tuesday)));

        state.sync_today(thursday);

        assert_eq!(state.calendar.selected, tuesday);
        assert_eq!(state.calendar.visible, tuesday);
    }

    #[test]
    fn today_action_restores_clock_following() {
        let today = Date::new(2026, 8, 18).unwrap();
        let tomorrow = Date::new(2026, 8, 19).unwrap();
        let mut state = State::at(today);
        drop(state.update(Event::Select(Date::new(2026, 7, 12).unwrap())));
        drop(state.update(Event::PrevMonth));

        drop(state.update(Event::Today(today)));
        state.sync_today(tomorrow);

        assert_eq!(state.calendar.selected, tomorrow);
        assert_eq!(state.calendar.visible, tomorrow);
    }

    #[test]
    fn selecting_an_adjacent_month_date_navigates_to_it() {
        let mut state = State::at(Date::new(2026, 8, 18).unwrap());
        let september = Date::new(2026, 9, 1).unwrap();

        drop(state.update(Event::Select(september)));

        assert_eq!(state.calendar.selected, september);
        assert_eq!(state.calendar.visible, september);
    }

    #[test]
    fn month_navigation_preserves_or_clamps_the_day() {
        let mut state = State::at(Date::new(2026, 1, 31).unwrap());

        drop(state.update(Event::NextMonth));

        assert_eq!(state.calendar.selected, Date::new(2026, 2, 28).unwrap());
        assert_eq!(state.calendar.visible, Date::new(2026, 2, 28).unwrap());
    }

    #[test]
    fn month_grid_uses_only_the_rows_the_month_needs() {
        let (_, february) = month_grid(Date::new(2026, 2, 1).unwrap());
        let (_, march) = month_grid(Date::new(2026, 3, 1).unwrap());

        assert_eq!(february, 5);
        assert_eq!(march, 6);
    }

    #[test]
    fn numeric_date_is_day_month_year() {
        assert_eq!(
            numeric_date(Date::new(2026, 8, 7).unwrap()),
            "07-08-2026"
        );
    }

    #[test]
    fn relative_dates_explain_the_selection() {
        let today = Date::new(2026, 8, 18).unwrap();

        assert_eq!(relative_date(today, today), "today");
        assert_eq!(
            relative_date(Date::new(2026, 8, 19).unwrap(), today),
            "tomorrow"
        );
        assert_eq!(
            relative_date(Date::new(2026, 8, 15).unwrap(), today),
            "3 days ago"
        );
    }
}
