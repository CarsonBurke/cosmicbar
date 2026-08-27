//! Palette and widget styling.
//!
//! The bar carries its own theme: outside a COSMIC session there is no
//! cosmic-settings-daemon to read, and the look has to match the existing
//! Catppuccin Mocha waybar. Semantic names mirror the ones the waybar CSS
//! used (`main-bg`, `accent`, `warning`, ...) so the two bars are comparable.

use cosmic::iced::font::{Family, Weight};
use cosmic::iced::{Background, Border, Color, Font, Shadow};
use cosmic::widget::container;

/// Radius of a module island, in logical pixels.
pub const ISLAND_RADIUS: f32 = 16.0;
/// How far a hovered and a pressed cell move toward the text colour. Measured
/// against the waybar CSS, whose `@hover-bg` sat about a fifth of the way from
/// `surface0` to the text; a fifth of the way from *any* island is too loud on
/// the light roles, so the bar lifts less and does it everywhere.
pub const HOVER_LIFT: f32 = 0.07;
pub const PRESS_LIFT: f32 = 0.14;

const fn c(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// Catppuccin Mocha, plus the semantic aliases the bar uses.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub crust: Color,
    pub mantle: Color,
    pub base: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub text: Color,
    pub subtext0: Color,
    pub overlay0: Color,
    pub lavender: Color,
    pub blue: Color,
    pub green: Color,
    pub yellow: Color,
    pub peach: Color,
    pub red: Color,
    pub mauve: Color,
    pub teal: Color,
}

impl Palette {
    pub const MOCHA: Self = Self {
        crust: c(0x11, 0x11, 0x1b),
        mantle: c(0x18, 0x18, 0x25),
        base: c(0x1e, 0x1e, 0x2e),
        surface0: c(0x31, 0x32, 0x44),
        surface1: c(0x45, 0x47, 0x5a),
        text: c(0xcd, 0xd6, 0xf4),
        subtext0: c(0xa6, 0xad, 0xc8),
        overlay0: c(0x6c, 0x70, 0x86),
        lavender: c(0xb4, 0xbe, 0xfe),
        blue: c(0x89, 0xb4, 0xfa),
        green: c(0xa6, 0xe3, 0xa1),
        yellow: c(0xf9, 0xe2, 0xaf),
        peach: c(0xfa, 0xb3, 0x87),
        red: c(0xf3, 0x8b, 0xa8),
        mauve: c(0xcb, 0xa6, 0xf7),
        teal: c(0x94, 0xe2, 0xd5),
    };

    pub const LATTE: Self = Self {
        crust: c(0xdc, 0xe0, 0xe8),
        mantle: c(0xe6, 0xe9, 0xef),
        base: c(0xef, 0xf1, 0xf5),
        surface0: c(0xcc, 0xd0, 0xda),
        surface1: c(0xbc, 0xc0, 0xcc),
        text: c(0x4c, 0x4f, 0x69),
        subtext0: c(0x6c, 0x6f, 0x85),
        overlay0: c(0x9c, 0xa0, 0xb0),
        lavender: c(0x72, 0x87, 0xfd),
        blue: c(0x1e, 0x66, 0xf5),
        green: c(0x40, 0xa0, 0x2b),
        yellow: c(0xdf, 0x8e, 0x1d),
        peach: c(0xfe, 0x64, 0x0b),
        red: c(0xd2, 0x0f, 0x39),
        mauve: c(0x88, 0x39, 0xef),
        teal: c(0x17, 0x92, 0x99),
    };

    pub fn by_name(name: &str) -> Self {
        match name {
            "catppuccin-latte" | "latte" => Self::LATTE,
            _ => Self::MOCHA,
        }
    }

    /// Bar background.
    pub fn bar_bg(&self) -> Color {
        self.crust
    }

    /// Every island's background. One step above the bar behind it, so a pill
    /// reads as a pill without any island being lighter than another.
    pub fn island(&self) -> Color {
        self.mantle
    }

    pub fn fg(&self) -> Color {
        self.text
    }

    pub fn accent(&self) -> Color {
        self.lavender
    }

    pub fn muted(&self) -> Color {
        self.subtext0
    }

    /// Hover and press fills, as a lift away from whatever the cell is sitting
    /// on. waybar's CSS named one flat `@hover-bg` (`surface1`) for every
    /// module; on a 24px strip that reads as a grey slab on the dark islands
    /// and as nothing at all on `surface0`, which is one of the island roles.
    /// A proportional lift is always visible and never louder than the island
    /// it belongs to.
    pub fn hover_over(&self, base: Color) -> Color {
        self.lift(base, HOVER_LIFT)
    }

    pub fn press_over(&self, base: Color) -> Color {
        self.lift(base, PRESS_LIFT)
    }

    /// `base` mixed `amount` of the way toward the text colour.
    fn lift(&self, base: Color, amount: f32) -> Color {
        let mix = |from: f32, to: f32| from + (to - from) * amount;
        Color {
            r: mix(base.r, self.text.r),
            g: mix(base.g, self.text.g),
            b: mix(base.b, self.text.b),
            a: 1.0,
        }
    }
}

/// Where one island ends and the next begins. waybar coloured its modules from a
/// three-step chain (`mantle`/`base`/`surface0`); on a 24px strip that reads as
/// three unrelated greys rather than one bar, so every island here paints the
/// same background and a module only says whether it opens an island or shares
/// its neighbour's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Island {
    /// Opens an island of its own.
    Start,
    /// Shares the island its left neighbour opened, so the two read as one pill
    /// with square corners between them.
    Join,
    /// No island: the module sits directly on the bar background.
    Flat,
}

impl Island {
    /// `None` for a module that paints no island.
    pub fn color(self, palette: &Palette) -> Option<Color> {
        match self {
            Self::Start | Self::Join => Some(palette.island()),
            Self::Flat => None,
        }
    }

    /// What a module actually opens when it has no left neighbour to join: a
    /// region cannot begin with a continuation.
    pub fn opened(self) -> Self {
        match self {
            Self::Join => Self::Start,
            other => other,
        }
    }
}

/// Container style for a module island.
pub fn island(palette: Palette, island: Island) -> cosmic::theme::Container<'static> {
    let background = island.color(&palette);
    cosmic::theme::Container::custom(move |_theme| container::Style {
        text_color: Some(palette.fg()),
        background: background.map(Background::Color),
        border: Border {
            radius: ISLAND_RADIUS.into(),
            ..Default::default()
        },
        shadow: Shadow::default(),
        icon_color: Some(palette.fg()),
        snap: true,
    })
}

/// Popup surface style: a raised card with an accent-free 1px border.
pub fn popup(palette: Palette) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |_theme| container::Style {
        text_color: Some(palette.fg()),
        background: Some(Background::Color(palette.base)),
        border: Border {
            radius: 12.0.into(),
            width: 1.0,
            color: palette.surface1,
        },
        shadow: Shadow::default(),
        icon_color: Some(palette.fg()),
        snap: true,
    })
}

/// The bar surface itself.
pub fn bar(palette: Palette) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |_theme| container::Style {
        text_color: Some(palette.fg()),
        background: Some(Background::Color(palette.bar_bg())),
        border: Border::default(),
        shadow: Shadow::default(),
        icon_color: Some(palette.fg()),
        snap: true,
    })
}

/// Popup radii: rows fill the card's width, chips sit inline in a header.
const ROW_RADIUS: f32 = 8.0;
const CHIP_RADIUS: f32 = 6.0;

/// A full-width row in a popup: a list entry, a device, a launcher target. It is
/// transparent on the card until hovered, the way a menu item is - COSMIC's own
/// `Button::MenuItem` reads its colours from the desktop theme, which is not
/// this bar's palette and paints a grey slab when no COSMIC session is running.
///
/// The row fades through these colours instead of switching between them: it is
/// as wide as the card, so a snap between two greys is the most visible thing
/// the bar could do. The button on top paints nothing (`cell`), exactly as a
/// bar cell does.
pub fn row_fill(palette: Palette) -> crate::fill::Fill {
    crate::fill::Fill {
        base: None,
        over: Some(palette.hover_over(palette.base)),
        pressed: Some(palette.press_over(palette.base)),
    }
}

/// The corner a popup row's fade paints, so a caller does not have to know
/// `ROW_RADIUS`.
pub const ROW_CORNERS: [f32; 4] = [ROW_RADIUS; 4];

/// An inline action in a popup header or footer: `check now`, `dismiss all`,
/// `cancel`. Filled at rest so it reads as a button next to plain text.
pub fn chip(palette: Palette) -> cosmic::theme::Button {
    let base = palette.hover_over(palette.base);
    button(
        palette.fg(),
        Some(base),
        palette.hover_over(base),
        palette.press_over(base),
        CHIP_RADIUS,
    )
}

/// The accented version, for the one action a popup is really offering.
pub fn chip_accent(palette: Palette) -> cosmic::theme::Button {
    button(
        palette.crust,
        Some(palette.accent()),
        palette.hover_over(palette.accent()),
        palette.press_over(palette.accent()),
        CHIP_RADIUS,
    )
}

/// A destructive action, armed and waiting for the second click.
pub fn chip_danger(palette: Palette) -> cosmic::theme::Button {
    button(
        palette.crust,
        Some(palette.red),
        palette.hover_over(palette.red),
        palette.press_over(palette.red),
        CHIP_RADIUS,
    )
}

fn button(
    text_color: Color,
    background: Option<Color>,
    hovered: Color,
    pressed: Color,
    radius: f32,
) -> cosmic::theme::Button {
    let style = move |background: Option<Color>| cosmic::widget::button::Style {
        shadow_offset: cosmic::iced::Vector::ZERO,
        background: background.map(Background::Color),
        overlay: None,
        border_radius: radius.into(),
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: Some(text_color),
        text_color: Some(text_color),
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| style(background)),
        hovered: Box::new(move |_focused, _theme| style(Some(hovered))),
        pressed: Box::new(move |_focused, _theme| style(Some(pressed))),
        disabled: Box::new(move |_theme| cosmic::widget::button::Style {
            // A disabled entry still has to read as text, not as a hole.
            text_color: Some(text_color.scale_alpha(0.5)),
            icon_color: Some(text_color.scale_alpha(0.5)),
            ..style(background)
        }),
    }
}

/// A clickable module cell. Whatever it sits on paints the background, so the
/// cell is transparent until hovered; `radius` keeps the island's outer corners
/// when neighbours are merged into one island, and `base` is the colour behind
/// the cell, which is what the hover lifts away from so the feedback is visible
/// on every island role and inside popups alike.
pub fn module_button(palette: Palette, base: Color, radius: [f32; 4]) -> cosmic::theme::Button {
    let style = move |background: Option<Color>| cosmic::widget::button::Style {
        shadow_offset: cosmic::iced::Vector::ZERO,
        background: background.map(Background::Color),
        overlay: None,
        border_radius: radius.into(),
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: Some(palette.fg()),
        text_color: Some(palette.fg()),
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| style(None)),
        hovered: Box::new(move |_focused, _theme| style(Some(palette.hover_over(base)))),
        pressed: Box::new(move |_focused, _theme| style(Some(palette.press_over(base)))),
        disabled: Box::new(move |_theme| style(None)),
    }
}

/// A clickable surface on the bar: a module cell, a workspace pill, a taskbar
/// item. `fill::fill` paints the background at every state, so the button only
/// carries the text and icon colour and never repaints on hover - which is what
/// makes the fade visible instead of being overwritten by a flat style.
pub fn cell(text_color: Color, radius: [f32; 4]) -> cosmic::theme::Button {
    let style = move || cosmic::widget::button::Style {
        shadow_offset: cosmic::iced::Vector::ZERO,
        background: None,
        overlay: None,
        border_radius: radius.into(),
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: Some(text_color),
        text_color: Some(text_color),
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| style()),
        hovered: Box::new(move |_focused, _theme| style()),
        pressed: Box::new(move |_focused, _theme| style()),
        disabled: Box::new(move |_theme| style()),
    }
}

/// Font used for normal bar text.
///
/// COSMIC resolves this from the desktop's interface-font setting. Keeping the
/// family intact here means the bar follows the system instead of replacing it
/// with a bar-specific face; only the configured weight is adjusted.
pub fn font(bold: bool) -> Font {
    let mut font = cosmic::font::default();
    font.weight = if bold { Weight::Bold } else { Weight::Normal };
    font
}

/// Nerd-font family used for glyph icons.
///
/// The *Mono* variant matters. In the plain `CommitMono Nerd Font`, every icon
/// advances one cell (0.6em) but its ink is drawn at its natural width - 0.5em
/// for a thermometer, a full 1.0em for the wifi fan, which overflows the cell by
/// 0.4em and crowds whatever follows it. The Mono variant squeezes every icon to
/// the cell (ink 0.599em of a 0.600em advance, side bearings 0.001em).
pub fn icon_font(bold: bool) -> Font {
    Font {
        family: Family::Name("CommitMono Nerd Font Mono"),
        weight: if bold { Weight::Bold } else { Weight::Normal },
        ..Font::DEFAULT
    }
}

/// The system text font, set once at startup with the bar's configured weight.
static SYSTEM_FONT: std::sync::OnceLock<Font> = std::sync::OnceLock::new();

pub fn set_font(font: Font) {
    let _ = SYSTEM_FONT.set(font);
}

fn bar_font() -> Font {
    *SYSTEM_FONT.get_or_init(|| font(true))
}

/// The icon font, set once at startup so icon-only widgets use the configured
/// weight without changing the system font used by normal text.
static ICON_FONT: std::sync::OnceLock<Font> = std::sync::OnceLock::new();

pub fn set_icon_font(font: Font) {
    let _ = ICON_FONT.set(font);
}

fn bar_icon_font() -> Font {
    *ICON_FONT.get_or_init(|| icon_font(true))
}

/// Text in the system interface font.
pub fn text<'a>(
    content: impl Into<std::borrow::Cow<'a, str>> + 'a,
) -> cosmic::widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
    cosmic::widget::text(content).font(bar_font())
}

/// Text rendered with the bar's Nerd Font icon face.
pub fn icon_text<'a>(
    content: impl Into<std::borrow::Cow<'a, str>> + 'a,
) -> cosmic::widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
    cosmic::widget::text(content).font(bar_icon_font())
}

/// Gap between a module's glyph *ink* and its system-font text, in logical
/// pixels. A fixed gap keeps icon bearings from changing the spacing between
/// modules.
pub const GLYPH_GAP: f32 = 6.0;

/// Icons in the Mono nerd variant fill exactly one text cell, so at the text's
/// own size they read smaller than the digits beside them; this brings the ink
/// back to the height a Nerd Font icon normally has.
const GLYPH_SCALE: f32 = 1.2;

/// A module's bar label: nerd-font glyph, gap, text. Both halves share one
/// class, so a call site still colours a single thing.
///
/// The gap is measured to the glyph's ink, not to its cell: a narrow icon in a
/// mono cell carries empty space on its right, and a fixed row spacing would
/// hand a thermometer twice the gap a memory chip gets.
pub fn label<'a, Message: 'a>(
    glyph: impl Into<std::borrow::Cow<'a, str>> + 'a,
    rest: impl Into<std::borrow::Cow<'a, str>> + 'a,
    size: f32,
    class: cosmic::theme::Text,
) -> cosmic::Element<'a, Message> {
    let glyph = glyph.into();
    let rest = rest.into();
    // Nothing to sit beside: the gap and the empty text run would pad the cell
    // on the right and push the icon off its centre.
    if rest.is_empty() {
        return glyph_only(glyph, size).class(class).into();
    }
    let bearing = crate::glyph::right_bearing(&glyph) * size * GLYPH_SCALE;
    cosmic::widget::Row::new()
        .push(glyph_text(glyph, size).class(class))
        .push(text(rest).size(size).class(class))
        .spacing((GLYPH_GAP - bearing).max(0.0))
        .align_y(cosmic::iced::Alignment::Center)
        .into()
}

/// Advance of one character in the Nerd Font's monospaced icon cell.
const MONO_ADVANCE: f32 = 0.6;

/// Approximate width of `text` at `size`, in logical pixels.
///
/// System text is proportional, so this estimate is intentionally conservative
/// for popup wrapping and reservation calculations.
const SYSTEM_ADVANCE: f32 = 0.6;

pub fn text_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * SYSTEM_ADVANCE * size
}

/// A bar label whose text keeps a fixed field, so the island does not resize as
/// the digits change and shove the centre of the bar around — waybar's
/// `min-length`, without its cost: padding the *string* to that width would put
/// a whole space between the glyph and a two-digit number, and none in front of
/// a three-digit one.
///
/// What the module actually shows is centred inside the reservation, the way a
/// GTK label with a `min-length` sat in its box: a short value then leaves half
/// its slack at each edge of the cell instead of piling all of it against the
/// right one, where it read as a section with more padding than its neighbours.
///
/// `widest` is the longest text the module can produce (`"100°C"` for a
/// temperature), not a guess: too short a field and the label elides itself.
pub fn label_fixed<'a, Message: 'a>(
    glyph: impl Into<std::borrow::Cow<'a, str>> + 'a,
    rest: impl Into<std::borrow::Cow<'a, str>> + 'a,
    widest: &str,
    size: f32,
    class: cosmic::theme::Text,
) -> cosmic::Element<'a, Message> {
    let glyph = glyph.into();
    let bearing = crate::glyph::right_bearing(&glyph) * size * GLYPH_SCALE;
    let gap = (GLYPH_GAP - bearing).max(0.0);
    // Nerd icons retain their fixed cell advance; system text is proportional,
    // so use the same conservative estimate as popup wrapping.
    let widest = MONO_ADVANCE * GLYPH_SCALE * size + gap + text_width(widest, size);
    let row = cosmic::widget::Row::new()
        .push(glyph_text(glyph, size).class(class))
        .push(text(rest).size(size).class(class))
        .spacing(gap)
        .align_y(cosmic::iced::Alignment::Center);
    cosmic::widget::container(row)
        .width(cosmic::iced::Length::Fixed(widest.ceil()))
        .align_x(cosmic::iced::Alignment::Center)
        .into()
}

/// A standalone glyph, sized like the ones inside labels: the cells that are
/// nothing but an icon (launcher, power, the idle inhibitor) have to match.
pub fn glyph_text<'a>(
    glyph: impl Into<std::borrow::Cow<'a, str>> + 'a,
    size: f32,
) -> cosmic::widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
    icon_text(glyph).size(size * GLYPH_SCALE)
}

/// Icon-only cells: a lone glyph has no digits to sit level with, so it is drawn
/// at the size a Nerd Font icon is meant to have instead of the size that keeps
/// its ink in line with text. At the label size a bell or a wifi arc alone in a
/// cell reads as shrunken.
const GLYPH_ONLY_SCALE: f32 = 1.45;

pub fn glyph_only<'a>(
    glyph: impl Into<std::borrow::Cow<'a, str>> + 'a,
    size: f32,
) -> cosmic::widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
    icon_text(glyph).size(size * GLYPH_ONLY_SCALE).line_height(
        cosmic::iced::widget::text::LineHeight::Absolute(cosmic::iced::Pixels(
            size * GLYPH_ONLY_SCALE,
        )),
    )
}
