//! The bar's own cell background, so hover and press fade instead of snapping.
//!
//! `cosmic::widget::button` picks one of four flat styles per frame: there is no
//! transition hook, and its hover style is chosen from the cursor position it is
//! handed at draw time. So the bar's clickable surfaces - module cells, taskbar
//! items, workspace pills - keep the button for its click semantics but let it
//! paint nothing, and this wrapper paints the background underneath: `base`
//! always, fading toward `over` while the pointer is inside and `pressed` while
//! a left button is down on it.
//!
//! The fade drives itself. `iced::animation::Animation` interpolates against the
//! `Instant` each `RedrawRequested` carries, and a frame that is still animating
//! asks for the next one - the same loop `libcosmic`'s circular progress widget
//! runs, so the bar sleeps again as soon as a fade settles.

use cosmic::iced::advanced::widget::{Operation, Tree, tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::animation::{Animation, Easing};
use cosmic::iced::border::Radius;
use cosmic::iced::time::Instant;
use cosmic::iced::{
    Background, Border, Color, Event as IcedEvent, Length, Rectangle, Shadow, Size, Vector, window,
};
use cosmic::{Element, Renderer, Theme};

use crate::bar::Message;

/// How long a cell takes to reach the hover colour. Short enough to feel like
/// the pointer is moving it, long enough to read as a fade at 60Hz.
const HOVER_MS: u64 = 120;
/// A press is an answer to a click and has to look immediate.
const PRESS_MS: u64 = 60;

/// Colours a clickable surface fades between.
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    /// Painted whenever the pointer is elsewhere. `None` for a cell that is
    /// transparent at rest, which is most of them: the island behind paints.
    pub base: Option<Color>,
    /// Reached while the pointer is inside.
    pub over: Option<Color>,
    /// Reached while a left button is held inside.
    pub pressed: Option<Color>,
}

/// Paint `fill` behind `content`, fading between its colours as the pointer
/// arrives, presses and leaves. `radius` matches the corner the cell occupies in
/// its island, so a merged neighbour keeps square inner corners.
pub fn fill<'a>(
    content: impl Into<Element<'a, Message>>,
    fill: Fill,
    radius: [f32; 4],
) -> Element<'a, Message> {
    Element::new(FillBox {
        content: content.into(),
        fill,
        radius: radius.into(),
        spot: None,
    })
}

/// Paint the fill as a circle of `diameter`, centred in the cell, while the
/// pointer is tracked across the whole cell. A cell is as tall as the bar, so a
/// workspace dot lit by its own bounds would be a bar-tall oval; the click
/// target still has to be the full cell, which is why this is not padding.
pub fn spot<'a>(
    content: impl Into<Element<'a, Message>>,
    fill: Fill,
    diameter: f32,
) -> Element<'a, Message> {
    Element::new(FillBox {
        content: content.into(),
        fill,
        radius: Radius::from(diameter / 2.0),
        spot: Some(diameter),
    })
}

struct FillBox<'a> {
    content: Element<'a, Message>,
    fill: Fill,
    radius: Radius,
    /// Diameter of the lit circle, when the cell paints a spot instead of
    /// filling itself.
    spot: Option<f32>,
}

struct State {
    /// Pointer inside this cell's bounds.
    inside: bool,
    /// Left button held after landing inside.
    down: bool,
    hover: Animation<bool>,
    press: Animation<bool>,
    /// Fade positions as of the last frame, so `draw` needs no clock.
    shown: (f32, f32),
}

impl State {
    fn new() -> Self {
        Self {
            inside: false,
            down: false,
            hover: Animation::new(false)
                .duration(std::time::Duration::from_millis(HOVER_MS))
                .easing(Easing::EaseOut),
            press: Animation::new(false)
                .duration(std::time::Duration::from_millis(PRESS_MS))
                .easing(Easing::EaseOut),
            shown: (0.0, 0.0),
        }
    }

    /// Fade positions at `at`, and whether a further frame is owed.
    fn sample(&self, at: Instant) -> ((f32, f32), bool) {
        (
            (
                self.hover.interpolate(0.0, 1.0, at),
                self.press.interpolate(0.0, 1.0, at),
            ),
            self.hover.is_animating(at) || self.press.is_animating(at),
        )
    }
}

/// Straight channel mix, alpha included: a `None` colour is the transparent
/// version of whatever it is mixing with, so a cell that is invisible at rest
/// fades in instead of popping.
fn mix(from: Option<Color>, to: Option<Color>, amount: f32) -> Option<Color> {
    let (from, to) = match (from, to) {
        (None, None) => return None,
        (Some(from), None) => (from, Color { a: 0.0, ..from }),
        (None, Some(to)) => (Color { a: 0.0, ..to }, to),
        (Some(from), Some(to)) => (from, to),
    };
    Some(Color {
        r: from.r + (to.r - from.r) * amount,
        g: from.g + (to.g - from.g) * amount,
        b: from.b + (to.b - from.b) * amount,
        a: from.a + (to.a - from.a) * amount,
    })
}

impl Widget<Message, Theme, Renderer> for FillBox<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::new())
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
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();
        match event {
            // A frame: advance the fade, and ask for another while it runs.
            IcedEvent::Window(window::Event::RedrawRequested(now)) => {
                let (shown, animating) = state.sample(*now);
                if shown != state.shown {
                    state.shown = shown;
                }
                if animating {
                    shell.request_redraw();
                }
            }
            // Motion is the only event carrying a fresh position; a leave is the
            // only notice that the pointer is gone, and on a 24px layer surface
            // it is usually the only one that arrives.
            IcedEvent::Mouse(mouse::Event::CursorMoved { .. })
            | IcedEvent::Mouse(mouse::Event::CursorLeft) => {
                let inside = !matches!(event, IcedEvent::Mouse(mouse::Event::CursorLeft))
                    && cursor.is_over(bounds);
                if inside != state.inside {
                    state.inside = inside;
                    state.hover.go_mut(inside, Instant::now());
                    // A pointer that leaves mid-press cannot still be pressing.
                    if !inside && state.down {
                        state.down = false;
                        state.press.go_mut(false, Instant::now());
                    }
                    shell.request_redraw();
                }
            }
            IcedEvent::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if cursor.is_over(bounds) && !state.down {
                    state.down = true;
                    state.press.go_mut(true, Instant::now());
                    shell.request_redraw();
                }
            }
            // Wherever the release lands, this cell is no longer held.
            IcedEvent::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.down {
                    state.down = false;
                    state.press.go_mut(false, Instant::now());
                    shell.request_redraw();
                }
            }
            _ => {}
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
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

        let state = tree.state.downcast_ref::<State>();
        let (hover, press) = state.shown;
        let color = mix(
            mix(self.fill.base, self.fill.over, hover),
            self.fill.pressed,
            press,
        );
        if let Some(color) = color.filter(|color| color.a > 0.0) {
            let bounds = layout.bounds();
            let bounds = match self.spot {
                Some(diameter) => Rectangle {
                    x: bounds.x + (bounds.width - diameter) / 2.0,
                    y: bounds.y + (bounds.height - diameter) / 2.0,
                    width: diameter,
                    height: diameter,
                },
                None => bounds,
            };
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    border: Border {
                        radius: self.radius,
                        width: 0.0,
                        color: Color::TRANSPARENT,
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(color),
            );
        }
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
