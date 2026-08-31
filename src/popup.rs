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

use cosmic::iced::advanced::widget::{Operation, Tree, tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::time::Instant;
use cosmic::iced::widget::scrollable::{Direction, Scrollbar};
use cosmic::iced::{
    Alignment, Event as IcedEvent, Length, Padding, Rectangle, Size, Vector, window,
};
use cosmic::widget;
use cosmic::{Apply, Element, Renderer, Theme};

use crate::bar::Message;
use crate::modules::{Ctx, ModuleId};
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

/// Scrollbar geometry. The thumb is slim and floats in the card's own right
/// inset: [`PAD_X`] wide, so a thumb [`THUMB`] wide sits [`THUMB_MARGIN`] clear
/// of the card's edge and never over a row's text.
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

/// The list block: full bleed, its content inset like every other block, and
/// its scrollbar floating in the right inset.
///
/// The gutter is reserved by padding rather than by the scrollbar, because
/// iced only gives an embedded scrollbar layout space while it is *visible*: a
/// list short enough not to scroll would hand that width back to the content,
/// and a row's action chip would land against the card's edge - or past it,
/// since the popup surface is exactly as wide as the card. Padding is there
/// either way, so a list that grows past [`LIST_HEIGHT`] gains a thumb without
/// reflowing a single row.
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
        .padding(Padding {
            top: PAD_Y,
            right: PAD_X,
            bottom: PAD_Y,
            left: PAD_X,
        })
        .width(Length::Fill)
        .apply(widget::scrollable)
        .direction(scrollbar())
        // `Minimal` paints the thumb and nothing else. The default,
        // `Permanent`, also paints a rail - from the COSMIC desktop theme
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
            // Floats over the content's right inset instead of taking layout
            // space: `spacing` is what would embed it, and an embedded bar only
            // reserves its width while the list actually overflows.
            .margin(THUMB_MARGIN),
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

/// A group of actions with nothing to their left: a transport, a player picker,
/// a row of presets. Same gap as [`split`], so a group of chips reads the same
/// wherever it sits.
pub fn actions<'a>(chips: impl IntoIterator<Item = Element<'a, Message>>) -> Row<'a> {
    let mut row = widget::Row::new()
        .spacing(ROW_GAP)
        .align_y(Alignment::Center);
    for chip in chips {
        row = row.push(chip);
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

/// Standard external-app action for popup headers: an unobtrusive
/// open-in-new glyph, always plain rather than a primary call to action.
pub fn popout<'a>(ctx: &Ctx, on_press: Option<Message>) -> Element<'a, Message> {
    // nf-md-open_in_new
    icon_chip("\u{f03cc}", Chip::Plain, ctx, on_press)
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

/// Opening and content-switch motion. The child keeps its final layout for the
/// whole animation; only the renderer translation changes, so no text is
/// reshaped and autosize is not invalidated on every frame.
const ENTER_MS: f32 = 140.0;
const ENTER_OFFSET: f32 = 8.0;

pub fn transition<'a>(
    content: impl Into<Element<'a, Message>>,
    key: ModuleId,
) -> Element<'a, Message> {
    Element::new(PopupTransition {
        content: content.into(),
        key,
    })
}

struct PopupTransition<'a> {
    content: Element<'a, Message>,
    key: ModuleId,
}

struct TransitionState {
    key: ModuleId,
    started: Instant,
    offset: f32,
}

impl TransitionState {
    fn new(key: ModuleId) -> Self {
        Self {
            key,
            started: Instant::now(),
            offset: -ENTER_OFFSET,
        }
    }

    fn restart(&mut self, key: ModuleId) {
        *self = Self::new(key);
    }

    fn sample(&self, at: Instant) -> (f32, bool) {
        let elapsed = at
            .checked_duration_since(self.started)
            .unwrap_or_default()
            .as_secs_f32()
            * 1_000.0;
        let progress = (elapsed / ENTER_MS).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        (-ENTER_OFFSET * (1.0 - eased), progress < 1.0)
    }
}

impl Widget<Message, Theme, Renderer> for PopupTransition<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<TransitionState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(TransitionState::new(self.key))
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<TransitionState>();
        if state.key != self.key {
            state.restart(self.key);
        }
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &IcedEvent,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let IcedEvent::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<TransitionState>();
            let (offset, animating) = state.sample(*now);
            state.offset = offset;
            if animating {
                shell.request_redraw();
            }
        }
        let offset = tree.state.downcast_ref::<TransitionState>().offset;
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor - Vector::new(0.0, offset),
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let offset = tree.state.downcast_ref::<TransitionState>().offset;
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor - Vector::new(0.0, offset),
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;

        let offset = tree.state.downcast_ref::<TransitionState>().offset;
        let cursor = cursor - Vector::new(0.0, offset);
        renderer.with_layer(layout.bounds(), |renderer| {
            renderer.with_translation(Vector::new(0.0, offset), |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    layout,
                    cursor,
                    viewport,
                );
            });
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let offset = tree.state.downcast_ref::<TransitionState>().offset;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            Vector::new(translation.x, translation.y + offset),
        )
    }
}
