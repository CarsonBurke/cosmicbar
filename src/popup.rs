//! The shape every popup takes.
//!
//! A popup is a card, and a card is a stack of blocks: a header, then whatever
//! the module has to say, then a footer. One block is separated from the next by
//! a hairline that runs the full width of the card, and every block carries the
//! same inset — so the title of one popup sits exactly where the title of the
//! next one does, and a divider reads as a section boundary rather than as a
//! line floating inside a padded box.
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ title                 chip   │  header block: what this popup is
//! ├──────────────────────────────┤
//! │ section                      │  a block: rows under a small label
//! │ row                    chip  │
//! ├──────────────────────────────┤
//! │ row                    chip ▌│  the list block: scrolls, scrollbar in
//! │ row                    chip ▌│  the card's own right padding
//! ├──────────────────────────────┤
//! │ status               chip    │  footer block: state and its verbs
//! └──────────────────────────────┘
//! ```
//!
//! Two properties are worth stating because they are the reason this module
//! exists rather than each popup padding a `Column` of its own:
//!
//! * **The header and footer do not scroll.** Only [`Card::list`] scrolls, so
//!   the thing that says *what you are looking at* and the thing that offers
//!   *what you can do about it* stay on screen however long the list is.
//! * **The scrollbar sits in the card's padding.** The list block is full
//!   bleed, and the scrollbar's own gutter is exactly [`PAD_X`] wide, so the
//!   thumb hugs the card edge and the text inside the list still lines up with
//!   the text in every other block. A scrollbar drawn inside the padding — the
//!   default, floating over the content — instead reads as a bar sitting on top
//!   of the rows, and covers whatever is on their right.

use std::borrow::Cow;

use cosmic::iced::widget::scrollable::{Direction, Scrollbar};
use cosmic::iced::{Alignment, Length, Padding};
use cosmic::widget;
use cosmic::{Apply, Element};

use crate::bar::Message;
use crate::modules::Ctx;
use crate::theme::Palette;

/// Text, rows and columns in the popup's own message type: `cosmic::widget`'s
/// aliases default their theme parameter to iced's, not COSMIC's.
type Text<'a> = cosmic::widget::Text<'a, cosmic::Theme, cosmic::Renderer>;
type Row<'a> = cosmic::widget::Row<'a, Message, cosmic::Theme>;
type Column<'a> = cosmic::widget::Column<'a, Message, cosmic::Theme>;

/// Inset from the card's edge to its text, horizontally and vertically. The
/// vertical one is smaller: a block of rows already carries [`GAP`] between its
/// own lines, and matching the horizontal inset on top of that leaves a header
/// looking like it is floating.
pub const PAD_X: f32 = 12.0;
pub const PAD_Y: f32 = 9.0;

/// Between two items inside one block: two rows of a list, a label and the rows
/// under it.
pub const GAP: f32 = 6.0;
/// Between the two lines of one item — a name and the detail under it. Tight
/// enough that the pair reads as one thing.
pub const LINE_GAP: f32 = 1.0;
/// Between an item's text and the action on its right, and between two actions.
pub const ROW_GAP: f32 = 8.0;

/// Where a list stops growing and starts scrolling. The compositor will not map
/// a popup taller than [`crate::bar::POPUP_MAX_HEIGHT`] at all, and a card that
/// tall is a wall of text regardless; this leaves room for a header, a footer
/// and the bar itself.
const LIST_HEIGHT: f32 = 420.0;

/// Scrollbar geometry. The thumb is slim and lives centred in a gutter exactly
/// [`PAD_X`] wide, which is the padding the list's own content would otherwise
/// have used: the rows keep their alignment with every other block, and the
/// thumb lands where a scrollbar belongs — against the card's edge.
const THUMB: f32 = 6.0;
const THUMB_MARGIN: f32 = (PAD_X - THUMB) / 2.0;

/// A popup card under construction. Blocks are drawn in the order they are
/// pushed, with a hairline between each pair.
#[derive(Default)]
pub struct Card<'a> {
    blocks: Vec<Block<'a>>,
}

enum Block<'a> {
    /// Padded on all four sides: the card's own inset.
    Padded(Element<'a, Message>),
    /// Full bleed and scrolling, with its content padded instead.
    List(Element<'a, Message>),
}

impl<'a> Card<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    /// One block of content, inset from the card's edges and separated from the
    /// block above it by a hairline.
    ///
    /// The first block is the header and the last is the footer: there is no
    /// separate method for either, because a card that reads top to bottom is
    /// the whole convention and a method that could be called out of order
    /// would be the first thing to break it.
    pub fn block(mut self, content: impl Into<Element<'a, Message>>) -> Self {
        self.blocks.push(Block::Padded(content.into()));
        self
    }

    /// Same, but only when there is something to show.
    pub fn maybe(self, content: Option<impl Into<Element<'a, Message>>>) -> Self {
        match content {
            Some(content) => self.block(content),
            None => self,
        }
    }

    /// The block that scrolls. At most one per card, because two scroll regions
    /// in one 420px-wide panel is a scrollbar hunt, not a layout.
    ///
    /// The list is as tall as its content up to [`LIST_HEIGHT`] and scrolls
    /// past it, so three notifications open a three-notification card and three
    /// hundred open the same card with a thumb in it. Nothing here estimates a
    /// height: the scroll region shrinks to what its content laid out to, which
    /// is the one measurement that cannot disagree with what is drawn.
    pub fn list(mut self, content: impl Into<Element<'a, Message>>) -> Self {
        self.blocks.push(Block::List(content.into()));
        self
    }

    pub fn build(self) -> Element<'a, Message> {
        let mut card = widget::Column::new().width(Length::Fill);
        for (index, block) in self.blocks.into_iter().enumerate() {
            if index > 0 {
                card = card.push(widget::divider::horizontal::default());
            }
            card = card.push(match block {
                Block::Padded(content) => content
                    .apply(widget::container)
                    .padding(Padding {
                        top: PAD_Y,
                        right: PAD_X,
                        bottom: PAD_Y,
                        left: PAD_X,
                    })
                    .width(Length::Fill)
                    .into(),
                Block::List(content) => scroll(content),
            });
        }
        card.into()
    }
}

/// The list block: full bleed, its content padded, its scrollbar in the gutter
/// that padding would have been.
///
/// `Length::Shrink` on the scrollable is what makes the card fit its content:
/// iced lays a vertical scrollable's content out with no height limit at all
/// and then resolves the scrollable itself against the limit it was given, so
/// the region is `min(content, LIST_HEIGHT)` tall and scrolls exactly when that
/// cap bites. The cap has to arrive as a limit from outside, which is what the
/// wrapping container is for.
fn scroll<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    content
        .apply(widget::container)
        // No right padding: the scrollbar gutter is exactly that wide.
        .padding(Padding {
            top: PAD_Y,
            right: 0.0,
            bottom: PAD_Y,
            left: PAD_X,
        })
        .width(Length::Fill)
        .apply(widget::scrollable)
        .direction(scrollbar())
        // `Minimal` paints the thumb and nothing else. The default,
        // `Permanent`, also paints a rail — from the COSMIC desktop theme
        // rather than from this bar's palette, which is a grey slab down the
        // side of a Catppuccin card.
        .class(cosmic::theme::iced::Scrollable::Minimal)
        .height(Length::Shrink)
        .width(Length::Fill)
        .apply(widget::container)
        .max_height(LIST_HEIGHT)
        .width(Length::Fill)
        .into()
}

fn scrollbar() -> Direction {
    Direction::Vertical(
        Scrollbar::new()
            .width(THUMB)
            .scroller_width(THUMB)
            .margin(THUMB_MARGIN)
            // Embeds the scrollbar: it takes layout space instead of floating
            // over the rows. `spacing` is the gap it leaves between itself and
            // the content, and the content's own padding is already zero on
            // that side, so the gutter is exactly the thumb plus its margins.
            .spacing(0.0),
    )
}

/// A block whose content sits on the left with its actions on the right: the
/// shape of a header, of a footer, and of a list row.
pub fn split<'a>(
    left: impl Into<Element<'a, Message>>,
    actions: impl IntoIterator<Item = Element<'a, Message>>,
) -> Row<'a> {
    let mut row = widget::Row::new()
        .push(left.into().apply(widget::container).width(Length::Fill))
        .spacing(ROW_GAP)
        .align_y(Alignment::Center);
    for action in actions {
        row = row.push(action);
    }
    row
}

/// A column of items inside a block: rows of a list, lines of a section.
pub fn column<'a>() -> Column<'a> {
    widget::Column::new().spacing(GAP).width(Length::Fill)
}

/// A column of lines belonging to one item — a name and the detail under it.
pub fn lines<'a>() -> Column<'a> {
    widget::Column::new().spacing(LINE_GAP).width(Length::Fill)
}

/// What this popup is about, at the top of the card: the adapter, the device,
/// the queue. The largest text in the card, and the only text at this size.
pub fn title<'a>(text: impl Into<Cow<'a, str>> + 'a, ctx: &Ctx) -> Text<'a> {
    crate::theme::text(text)
        .size(ctx.font_size)
        .class(cosmic::theme::Text::Color(ctx.palette.fg()))
}

/// The label over a group of rows: `connected`, `nearby`, `history`. Small and
/// faint, because it is a signpost and not content.
pub fn section<'a>(text: impl Into<Cow<'a, str>> + 'a, ctx: &Ctx) -> Text<'a> {
    crate::theme::text(text)
        .size(ctx.small())
        .class(cosmic::theme::Text::Color(ctx.palette.overlay0))
}

/// A row's own text: the name of the thing the row is about.
pub fn item<'a>(text: impl Into<Cow<'a, str>> + 'a, ctx: &Ctx) -> Text<'a> {
    crate::theme::text(text).size(ctx.body())
}

/// The detail under a row's name, or the state a footer reports.
pub fn detail<'a>(text: impl Into<Cow<'a, str>> + 'a, ctx: &Ctx) -> Text<'a> {
    crate::theme::text(text)
        .size(ctx.small())
        .class(cosmic::theme::Text::Color(ctx.palette.muted()))
}

/// How loud an action is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// One verb among several: `check now`, `scan`, `activate`.
    Plain,
    /// The one thing this popup is really offering: `update`.
    Accent,
    /// Destructive: `cancel`, `disconnect`, `log out`.
    Danger,
}

impl Chip {
    fn class(self, palette: Palette) -> cosmic::theme::Button {
        match self {
            Self::Plain => crate::theme::chip(palette),
            Self::Accent => crate::theme::chip_accent(palette),
            Self::Danger => crate::theme::chip_danger(palette),
        }
    }
}

/// Vertical padding inside a chip. The horizontal one is [`ROW_GAP`], so a
/// chip's label keeps the same distance from its edge that two chips keep from
/// each other.
const CHIP_PAD: f32 = 3.0;

/// An inline action. `None` draws it disabled, which still says the affordance
/// exists — a cancel already requested, an update with nothing to update.
///
/// Built from `button::custom` rather than `button::text` because
/// `button::text` renders libcosmic's own interface font at its own fixed size:
/// chips built that way are a different typeface at a different size from the
/// popup they sit in, and they cannot show a nerd-font glyph at all.
pub fn chip<'a>(
    label: impl Into<Cow<'a, str>> + 'a,
    style: Chip,
    ctx: &Ctx,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    button(
        crate::theme::text(label).size(ctx.small()).into(),
        style,
        ctx,
        on_press,
    )
}

/// A chip whose label is a nerd-font glyph: the actions with no word short
/// enough to fit beside three others.
pub fn icon_chip<'a>(
    glyph: impl Into<Cow<'a, str>> + 'a,
    style: Chip,
    ctx: &Ctx,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    button(
        crate::theme::icon_text(glyph).size(ctx.small()).into(),
        style,
        ctx,
        on_press,
    )
}

fn button<'a>(
    label: Element<'a, Message>,
    style: Chip,
    ctx: &Ctx,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    widget::button::custom(label)
        .class(style.class(ctx.palette))
        .padding([CHIP_PAD, ROW_GAP])
        .on_press_maybe(on_press)
        .into()
}

/// Padding inside a row that lights up: enough that the highlight has a border
/// around its text instead of a seam against it, and no more, because the
/// difference is also how far that row's text sits in from the text of a block
/// that has no rows to light.
const ROW_INSET_X: f32 = 6.0;
const ROW_INSET_Y: f32 = 3.0;

/// A row you can click: a launcher target, a device to switch to, a
/// notification, a power action. It spans the block and fades between the
/// card's own colour and a lift away from it, the way a menu item does.
///
/// The fade is the point. A popup row is as wide as the card, so switching
/// between two flat greys on the frame the pointer arrives is the most visible
/// thing the bar can do; `fill` paints the background at every state and the
/// button on top paints nothing but the text colour.
pub fn row<'a>(
    content: impl Into<Element<'a, Message>>,
    palette: Palette,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let pressable = on_press.is_some();
    let button = widget::button::custom(content.into())
        .width(Length::Fill)
        .padding([ROW_INSET_Y, ROW_INSET_X])
        .class(crate::theme::cell(palette.fg(), crate::theme::ROW_CORNERS))
        .on_press_maybe(on_press);
    // A row with nothing to press is a line of text that happens to be laid out
    // like a row: lighting up under the pointer would promise a click it does
    // not have. `fill` follows the pointer over its own bounds and knows
    // nothing about the button inside it, so the decision has to be here.
    match pressable {
        true => crate::fill::fill(
            button,
            crate::theme::row_fill(palette),
            crate::theme::ROW_CORNERS,
        ),
        false => button.into(),
    }
}

/// The same row, armed: painted like a destructive chip because it is one click
/// from doing the thing it names. No hover fade — a row that is already solid
/// red has nothing left to say about the pointer being over it.
pub fn row_armed<'a>(
    content: impl Into<Element<'a, Message>>,
    palette: Palette,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    widget::button::custom(content.into())
        .width(Length::Fill)
        .padding([ROW_INSET_Y, ROW_INSET_X])
        .class(crate::theme::chip_danger(palette))
        .on_press_maybe(on_press)
        .into()
}
