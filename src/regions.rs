//! Measured bar regions: centered when possible, constrained to the free gap.
//!
//! A region scrolls as a whole when its content cannot fit, unless it holds an
//! island that grows without bound (the app list): then only that island gives
//! up width, scrolling inside its own pill, while its neighbours stay put. The
//! left region overflows first; only once it is down to its floor do the other
//! regions shrink, large regions sharing a width cap while small ones keep
//! their natural size. Scrolling reveals the rest without painting or clicking
//! into a neighbour.

use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::widget::scrollable::{Direction, Scrollbar};
use cosmic::iced::{Alignment, Event, Length, Point, Rectangle, Size, Vector};
use cosmic::{Apply, Element, Renderer, Theme, widget};
use std::cell::Cell;
use std::rc::Rc;

use crate::bar::Message;

/// Between regions, at most; a crowded bar closes it up.
const GAP: f32 = 6.0;
/// Between neighbouring islands within a region.
const SPACING: f32 = 6.0;

/// One region's islands, left to right.
#[derive(Default)]
pub struct Region<'a> {
    islands: Vec<Element<'a, Message>>,
    overflow: Option<usize>,
    /// The natural width of the overflow island's scrolling content.
    scrolled: Rc<Cell<f32>>,
}

impl<'a> Region<'a> {
    /// `overflows` marks the island that gives up width when the region cannot
    /// fit. Its content has to scroll on its own, inside [`Region::overflow`].
    pub fn push(&mut self, island: Element<'a, Message>, overflows: bool) {
        if overflows {
            self.overflow = Some(self.islands.len());
        }
        self.islands.push(island);
    }

    /// The scroll viewport for an overflowing island's content, placed inside
    /// the island's padding so the pill keeps both of its rounded ends.
    pub fn overflow(&self, content: Element<'a, Message>, height: f32) -> Element<'a, Message> {
        let mut viewport = Viewport::new(
            content
                .apply(widget::container)
                .height(Length::Fixed(height))
                .align_y(Alignment::Center)
                .into(),
            height,
            false,
        );
        viewport.natural = Some(self.scrolled.clone());
        Element::new(viewport)
    }
}

/// A region of one island that does not overflow.
impl<'a> From<Element<'a, Message>> for Region<'a> {
    fn from(island: Element<'a, Message>) -> Self {
        let mut region = Self::default();
        region.push(island, false);
        region
    }
}

pub fn regions<'a>(content: [Region<'a>; 3], height: f32) -> Element<'a, Message> {
    let squeezed = std::array::from_fn(|index| content[index].overflow.is_some());
    Element::new(Regions {
        squeezed,
        children: content
            .into_iter()
            .enumerate()
            .map(|(index, region)| match region.overflow {
                // A region viewport measures its content unbounded, so the
                // island inside could never learn how much room it has.
                Some(overflow) => Element::new(Squeeze {
                    islands: region.islands,
                    overflow,
                    scrolled: region.scrolled,
                    height,
                }),
                None => Element::new(Viewport::new(
                    widget::Row::with_children(region.islands)
                        .spacing(SPACING)
                        .align_y(Alignment::Center)
                        .height(Length::Fixed(height))
                        .into(),
                    height,
                    // Preserve access to the rightmost controls when that edge
                    // overflows; the other regions start at their leftmost item.
                    index == 2,
                )),
            })
            .collect(),
        height,
    })
}

struct Regions<'a> {
    children: Vec<Element<'a, Message>>,
    /// Which regions hold an overflow island, and so have a floor of their own.
    squeezed: [bool; 3],
    height: f32,
}

/// A horizontal scroll area that an ordinary vertical wheel scrolls too.
struct Viewport<'a> {
    scrollable: Element<'a, Message>,
    captured: Rc<Cell<bool>>,
    /// Starts at the rightmost item, and so reads the wheel the other way round.
    anchor_right: bool,
    /// Where to report the content's natural width, which a scrollable
    /// measures however little room it is given.
    natural: Option<Rc<Cell<f32>>>,
}

impl<'a> Viewport<'a> {
    fn new(content: Element<'a, Message>, height: f32, anchor_right: bool) -> Self {
        let captured = Rc::new(Cell::new(false));
        let scrollable = widget::scrollable(Element::new(WheelProbe {
            content,
            captured: captured.clone(),
        }))
        .direction(Direction::Horizontal(
            Scrollbar::new().width(3.0).scroller_width(3.0).margin(0.0),
        ))
        .class(cosmic::theme::iced::Scrollable::Minimal)
        .width(Length::Shrink)
        .height(Length::Fixed(height));
        Self {
            scrollable: if anchor_right {
                scrollable.anchor_right()
            } else {
                scrollable
            }
            .into(),
            captured,
            anchor_right,
            natural: None,
        }
    }
}

impl Widget<Message, Theme, Renderer> for Viewport<'_> {
    fn size(&self) -> Size<Length> {
        self.scrollable.as_widget().size()
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.scrollable)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.scrollable));
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .scrollable
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        if let (Some(natural), Some(content)) = (&self.natural, node.children().first()) {
            natural.set(content.size().width);
        }
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.scrollable
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.captured.set(false);
        self.scrollable.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        // Ordinary wheels reveal overflowing passive content too. Module wheel
        // bindings get first refusal, so volume/brightness gestures keep their
        // normal meaning. Horizontal gestures remain native.
        if self.captured.get()
            || !cursor.is_over(layout.bounds())
            || !layout
                .children()
                .next()
                .is_some_and(|content| content.bounds().width > layout.bounds().width)
        {
            return;
        }
        let sign = if self.anchor_right { -1.0 } else { 1.0 };
        let delta = match event {
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y },
            }) => mouse::ScrollDelta::Lines {
                x: sign * y,
                y: 0.0,
            },
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y },
            }) => mouse::ScrollDelta::Pixels {
                x: sign * y,
                y: 0.0,
            },
            _ => return,
        };
        // The native wrapper may have captured the unmoved original wheel.
        // Give the remapped event a fresh capture status.
        let mut messages = Vec::new();
        let mut remapped = Shell::new(&mut messages);
        self.scrollable.as_widget_mut().update(
            &mut tree.children[0],
            &Event::Mouse(mouse::Event::WheelScrolled { delta }),
            layout,
            cursor,
            renderer,
            clipboard,
            &mut remapped,
            viewport,
        );
        shell.merge(remapped, |message| message);
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.scrollable.as_widget().mouse_interaction(
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
        self.scrollable.as_widget().draw(
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
        self.scrollable.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// A region whose overflow island takes up the slack: every other island keeps
/// its natural width, and that one is handed whatever is left.
struct Squeeze<'a> {
    islands: Vec<Element<'a, Message>>,
    overflow: usize,
    scrolled: Rc<Cell<f32>>,
    height: f32,
}

impl Widget<Message, Theme, Renderer> for Squeeze<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Fixed(self.height))
    }
    fn children(&self) -> Vec<Tree> {
        self.islands.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.islands);
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let bounded = |width| layout::Limits::new(Size::ZERO, Size::new(width, self.height));
        let mut nodes: Vec<_> = self
            .islands
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(island, state)| {
                island
                    .as_widget_mut()
                    .layout(state, renderer, &bounded(f32::INFINITY))
            })
            .collect();
        let fixed: f32 = nodes
            .iter()
            .enumerate()
            .filter(|&(index, _)| index != self.overflow)
            .map(|(_, node)| node.size().width + SPACING)
            .sum();
        let room = limits.max().width - fixed;
        if nodes[self.overflow].size().width > room {
            let island = self.islands[self.overflow].as_widget_mut();
            let state = &mut tree.children[self.overflow];
            // Never narrower than its own padding plus one bar-height cell, so
            // the island stays a pill with something in it to scroll. Short of
            // room, padding gives way before content does, so the padding can
            // only be read off the unbounded pass.
            let scrolled = self.scrolled.get();
            let chrome = nodes[self.overflow].size().width - scrolled;
            let floor = chrome + scrolled.min(self.height);
            nodes[self.overflow] = island.layout(state, renderer, &bounded(room.max(floor)));
        }
        let mut x = 0.0;
        for node in &mut nodes {
            node.move_to_mut(Point::new(x, (self.height - node.size().height) / 2.0));
            x += node.size().width + SPACING;
        }
        // Not clamped to the limits: laid out with no room at all, this is the
        // floor the bar keeps for the region. Only a bar too narrow for the
        // floors of every region sees one spill past the width it was given.
        layout::Node::with_children(Size::new((x - SPACING).max(0.0), self.height), nodes)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let offset = layout.virtual_offset();
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((island, state), layout) in self
                .islands
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                island.as_widget_mut().operate(
                    state,
                    layout.with_virtual_offset(offset),
                    renderer,
                    operation,
                );
            }
        });
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let offset = layout.virtual_offset();
        for ((island, state), layout) in self
            .islands
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            island.as_widget_mut().update(
                state,
                event,
                layout.with_virtual_offset(offset),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.islands
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((island, state), child)| {
                island.as_widget().mouse_interaction(
                    state,
                    child.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
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
        for ((island, state), child) in self
            .islands
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            island.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child.with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.islands,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

// The pinned Scrollable captures a wheel when it initializes its transaction,
// even if that wheel moved no horizontal pixels. Observe capture *inside* the
// viewport so the fallback respects module bindings, rather than that wrapper.
struct WheelProbe<'a> {
    content: Element<'a, Message>,
    captured: Rc<Cell<bool>>,
}

impl Widget<Message, Theme, Renderer> for WheelProbe<'_> {
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
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
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
        self.captured.set(shell.is_event_captured());
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

#[derive(Debug)]
struct Placement {
    x: [f32; 3],
    widths: [f32; 3],
}

/// Widths come from the widgets' real layout, including font shaping and padding.
/// `floor` is the width each region keeps however crowded the bar is, so it
/// stays visible and can still be scrolled.
fn placement(width: f32, natural: [f32; 3], floor: [f32; 3]) -> Placement {
    let count = natural.iter().filter(|&&width| width > 0.0).count();
    let gap = if count > 1 {
        GAP.min(width / (count - 1) as f32)
    } else {
        0.0
    };
    let available = (width - gap * count.saturating_sub(1) as f32).max(0.0);
    let floor: [f32; 3] = std::array::from_fn(|index| floor[index].min(natural[index]));
    // The left region holds the app list, which grows with every window, so it
    // overflows first: the center and right regions are fitted to what it
    // leaves above its floor, and it takes whatever they do not use.
    let [center_width, right_width] = share(
        available - floor[0].min(available),
        [natural[1], natural[2]],
        [floor[1], floor[2]],
    );
    let widths = [
        natural[0].min((available - center_width - right_width).max(0.0)),
        center_width,
        right_width,
    ];
    let left = if widths[0] > 0.0 {
        widths[0] + gap
    } else {
        0.0
    };
    let right = width - widths[2] - if widths[2] > 0.0 { gap } else { 0.0 };
    // Floating point summation can leave the two edges a fraction apart when
    // all three regions exactly fill the bar; never give clamp reversed bounds.
    let center = ((width - widths[1]) / 2.0).clamp(left, (right - widths[1]).max(left));
    Placement {
        x: [0.0, center, (width - widths[2]).max(0.0)],
        widths,
    }
}

/// Fits `natural` widths into `available` under one shared cap: regions under
/// it keep their natural width, the rest share what is left, and none goes
/// below its floor (`floor <= natural`) while the floors fit at all.
fn share<const N: usize>(available: f32, natural: [f32; N], floor: [f32; N]) -> [f32; N] {
    let capped = |cap: f32| -> [f32; N] {
        std::array::from_fn(|index| cap.clamp(floor[index], natural[index]))
    };
    let total = |cap: f32| capped(cap).iter().sum::<f32>();
    let floors: f32 = floor.iter().sum();
    if floors >= available {
        // Not even the floors fit: shrink them alike rather than overlap.
        let scale = if floors > 0.0 {
            available / floors
        } else {
            0.0
        };
        return floor.map(|floor| floor * scale);
    }
    // The capped total grows linearly between consecutive floors and natural
    // widths, so find the stretch `available` falls in and solve it there.
    let mut points: Vec<f32> = floor.iter().chain(&natural).copied().collect();
    points.sort_by(f32::total_cmp);
    let mut low = 0.0;
    for high in points {
        if total(high) >= available {
            let growing = (0..N)
                .filter(|&index| floor[index] <= low && natural[index] >= high)
                .count();
            return capped(low + (available - total(low)) / growing.max(1) as f32);
        }
        low = high;
    }
    natural
}

impl Widget<Message, Theme, Renderer> for Regions<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fixed(self.height))
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Length::Fill, Length::Fixed(self.height), Size::ZERO);
        // Horizontal scrollables measure their contents without compressing
        // them, so a long region cannot disguise itself as one that fits.
        let measure = layout::Limits::new(Size::ZERO, Size::new(f32::INFINITY, size.height));
        let mut nodes: Vec<_> = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(child, state)| child.as_widget_mut().layout(state, renderer, &measure))
            .collect();
        let natural: [f32; 3] = std::array::from_fn(|index| nodes[index].size().width);
        let overflowing =
            natural.iter().sum::<f32>() + GAP * natural.len().saturating_sub(1) as f32 > size.width;
        // An overflow island's region has a floor: what it lays out to with no
        // room at all, its neighbours at their natural width. A viewport
        // scrolls instead, but the left region still keeps one bar-height
        // cell, room for the launcher badge or a single app icon.
        let mut floor = [0.0; 3];
        if overflowing {
            let squeezed = layout::Limits::new(Size::ZERO, Size::new(0.0, size.height));
            for (index, (child, state)) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .enumerate()
                .filter(|&(index, _)| self.squeezed[index])
            {
                floor[index] = child
                    .as_widget_mut()
                    .layout(state, renderer, &squeezed)
                    .size()
                    .width;
            }
            floor[0] = floor[0].max(self.height);
        }
        let placement = placement(size.width, natural, floor);
        for (index, ((child, state), node)) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(&mut nodes)
            .enumerate()
        {
            let constrained = placement.widths[index] < natural[index];
            // Measuring a floor left that region laid out for no room, and
            // widgets such as text keep their last layout in their state.
            if constrained || (overflowing && self.squeezed[index]) {
                let width = if constrained {
                    placement.widths[index]
                } else {
                    f32::INFINITY
                };
                *node = child.as_widget_mut().layout(
                    state,
                    renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(width, size.height)),
                );
            }
            node.move_to_mut(Point::new(
                placement.x[index],
                (size.height - node.size().height) / 2.0,
            ));
        }
        layout::Node::with_children(size, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, state), bounds) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child.as_widget_mut().operate(
                    state,
                    bounds.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        for ((child, state), bounds) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state,
                event,
                bounds.with_virtual_offset(layout.virtual_offset()),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), bounds)| {
                child.as_widget().mouse_interaction(
                    state,
                    bounds.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
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
        for ((child, state), bounds) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                bounds.with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(width: f32, message: Message) -> Element<'static, Message> {
        widget::button::custom(
            widget::space::horizontal()
                .width(Length::Fixed(width))
                .height(Length::Fixed(24.0)),
        )
        .padding(0)
        .on_press(message)
        .into()
    }

    fn dispatch(
        content: &mut Element<'_, Message>,
        tree: &mut Tree,
        node: &layout::Node,
        renderer: &Renderer,
        event: &Event,
        position: Point,
        messages: &mut Vec<Message>,
    ) {
        content.as_widget_mut().update(
            tree,
            event,
            Layout::new(node),
            mouse::Cursor::Available(position),
            renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(messages),
            &Rectangle::new(Point::ORIGIN, node.size()),
        );
    }

    #[test]
    fn center_stays_at_output_midpoint_when_it_fits() {
        let plan = placement(1000.0, [100.0, 200.0, 250.0], [24.0, 0.0, 0.0]);
        assert_eq!(plan.x, [0.0, 400.0, 750.0]);
        assert_eq!(plan.widths, [100.0, 200.0, 250.0]);
    }

    #[test]
    fn asymmetric_edges_shift_center_without_shrinking_content_that_fits() {
        let plan = placement(952.0, [60.0, 200.0, 600.0], [24.0, 0.0, 0.0]);
        assert_eq!(plan.x, [0.0, 146.0, 352.0]);
        assert_eq!(plan.widths, [60.0, 200.0, 600.0]);
        let mirror = placement(952.0, [600.0, 200.0, 60.0], [24.0, 0.0, 0.0]);
        assert_eq!(mirror.x, [0.0, 606.0, 892.0]);
    }

    #[test]
    fn left_region_overflows_before_center_or_right_shrink() {
        // A long app list used to take an even share and squeeze the center.
        let plan = placement(1000.0, [700.0, 450.0, 250.0], [24.0, 0.0, 0.0]);
        assert_eq!(plan.widths, [288.0, 450.0, 250.0]);
        assert_eq!(plan.x, [0.0, 294.0, 750.0]);
    }

    #[test]
    fn edges_share_a_cap_once_the_left_region_is_down_to_its_floor() {
        let plan = placement(600.0, [50.0, 400.0, 800.0], [24.0, 0.0, 0.0]);
        assert_eq!(plan.widths, [24.0, 282.0, 282.0]);
        assert_eq!(plan.x, [0.0, 30.0, 318.0]);
        // A left region already under the floor keeps its natural width.
        let plan = placement(600.0, [20.0, 400.0, 800.0], [24.0, 0.0, 0.0]);
        assert_eq!(plan.widths, [20.0, 284.0, 284.0]);
    }

    #[test]
    fn a_floor_keeps_its_region_out_of_the_shared_cap() {
        // An app list placed on the right keeps its neighbours' width as well.
        let plan = placement(600.0, [50.0, 400.0, 800.0], [24.0, 300.0, 0.0]);
        assert_eq!(plan.widths, [24.0, 300.0, 264.0]);
        // Floors that cannot all fit shrink alike instead of overlapping.
        let plan = placement(212.0, [50.0, 400.0, 800.0], [0.0, 300.0, 100.0]);
        assert_eq!(plan.widths, [0.0, 150.0, 50.0]);
    }

    #[test]
    fn overflow_never_overlaps_or_leaves_the_bar() {
        for width in [0.0, 1.0, 4.0, 12.0, 50.0, 200.0, 1200.0] {
            for natural in [
                [0.0, 0.0, 0.0],
                [200.0, 0.0, 400.0],
                [0.0, 200.0, 400.0],
                [200.0, 400.0, 0.0],
                [200.0, 400.0, 800.0],
                [900.0, 400.0, 300.0],
            ] {
                for floor in [
                    [0.0, 0.0, 0.0],
                    [24.0, 0.0, 0.0],
                    [2000.0, 0.0, 0.0],
                    [24.0, 150.0, 150.0],
                    [0.0, 2000.0, 2000.0],
                ] {
                    let plan = placement(width, natural, floor);
                    for i in 0..3 {
                        assert!(plan.widths[i] >= 0.0);
                        assert!(plan.x[i] >= 0.0);
                        assert!(plan.x[i] + plan.widths[i] <= width + 0.001);
                        for j in i + 1..3 {
                            if plan.widths[i] > 0.0 && plan.widths[j] > 0.0 {
                                assert!(plan.x[i] + plan.widths[i] <= plan.x[j] + 0.001);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn measured_widget_preserves_right_click_target_when_center_shifts_left() {
        let mut content = regions(
            [
                button(60.0, Message::ClosePopup).into(),
                button(200.0, Message::Control(crate::control::Command::Reload)).into(),
                button(600.0, Message::Control(crate::control::Command::Close)).into(),
            ],
            24.0,
        );
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(952.0, 24.0)),
        );
        let children: Vec<_> = Layout::new(&node)
            .children()
            .map(|layout| layout.bounds())
            .collect();
        assert_eq!(children[1].x, 146.0);
        assert_eq!(children[1].width, 200.0);
        assert_eq!(children[2].x, 352.0);
        let mut messages = Vec::new();
        // The old Stack painted the center over this part of the right region.
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            dispatch(
                &mut content,
                &mut tree,
                &node,
                &renderer,
                &event,
                Point::new(400.0, 10.0),
                &mut messages,
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            messages[0],
            Message::Control(crate::control::Command::Close)
        ));
    }

    #[test]
    fn overflowing_region_clips_clicks_and_scrolls_to_hidden_controls() {
        let left: Element<'_, Message> = widget::Row::new()
            .push(button(200.0, Message::ClosePopup))
            .push(button(
                200.0,
                Message::Control(crate::control::Command::Reload),
            ))
            .into();
        let mut content = regions(
            [
                left.into(),
                button(50.0, Message::Control(crate::control::Command::Close)).into(),
                button(
                    50.0,
                    Message::Control(crate::control::Command::BrightnessMode(None)),
                )
                .into(),
            ],
            24.0,
        );
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 24.0)),
        );
        assert_eq!(node.children()[0].size().width, 88.0);
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            dispatch(
                &mut content,
                &mut tree,
                &node,
                &renderer,
                &event,
                Point::new(110.0, 10.0),
                &mut messages,
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            messages[0],
            Message::Control(crate::control::Command::Close)
        ));
        messages.clear();
        dispatch(
            &mut content,
            &mut tree,
            &node,
            &renderer,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -400.0, y: 0.0 },
            }),
            Point::new(20.0, 10.0),
            &mut messages,
        );
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            dispatch(
                &mut content,
                &mut tree,
                &node,
                &renderer,
                &event,
                Point::new(20.0, 10.0),
                &mut messages,
            );
        }
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            messages[0],
            Message::Control(crate::control::Command::Reload)
        ));
    }

    #[test]
    fn ordinary_vertical_wheel_reveals_overflowing_passive_controls() {
        let left: Element<'_, Message> = widget::Row::new()
            .push(button(200.0, Message::ClosePopup))
            .push(button(
                200.0,
                Message::Control(crate::control::Command::Reload),
            ))
            .into();
        let mut content = regions(
            [
                left.into(),
                button(50.0, Message::ClosePopup).into(),
                button(50.0, Message::ClosePopup).into(),
            ],
            24.0,
        );
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 24.0)),
        );
        let mut messages = Vec::new();
        dispatch(
            &mut content,
            &mut tree,
            &node,
            &renderer,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -400.0 },
            }),
            Point::new(20.0, 10.0),
            &mut messages,
        );
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            dispatch(
                &mut content,
                &mut tree,
                &node,
                &renderer,
                &event,
                Point::new(20.0, 10.0),
                &mut messages,
            );
        }
        assert!(matches!(
            messages.as_slice(),
            [Message::Control(crate::control::Command::Reload)]
        ));
    }

    #[test]
    fn module_wheel_binding_keeps_its_own_action_without_scrolling() {
        let left = crate::modules::pointer::Pointer::new(
            widget::Row::new()
                .push(button(200.0, Message::ClosePopup))
                .push(button(
                    200.0,
                    Message::Control(crate::control::Command::Reload),
                ))
                .into(),
        )
        .on_wheel(|_| Message::Control(crate::control::Command::Close))
        .wrap();
        let mut content = regions(
            [
                left.into(),
                button(50.0, Message::ClosePopup).into(),
                button(50.0, Message::ClosePopup).into(),
            ],
            24.0,
        );
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 24.0)),
        );
        let mut messages = Vec::new();
        dispatch(
            &mut content,
            &mut tree,
            &node,
            &renderer,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -10.0 },
            }),
            Point::new(20.0, 10.0),
            &mut messages,
        );
        assert!(matches!(
            messages.as_slice(),
            [Message::Control(crate::control::Command::Close)]
        ));
        messages.clear();
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            dispatch(
                &mut content,
                &mut tree,
                &node,
                &renderer,
                &event,
                Point::new(20.0, 10.0),
                &mut messages,
            );
        }
        assert!(matches!(messages.as_slice(), [Message::ClosePopup]));
    }

    fn mode(name: &str) -> Message {
        Message::Control(crate::control::Command::BrightnessMode(Some(name.into())))
    }

    fn clicked(
        content: &mut Element<'_, Message>,
        tree: &mut Tree,
        node: &layout::Node,
        renderer: &Renderer,
        position: Point,
    ) -> Vec<Message> {
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            dispatch(
                content,
                tree,
                node,
                renderer,
                &event,
                position,
                &mut messages,
            );
        }
        messages
    }

    #[test]
    fn overflow_island_scrolls_by_itself_while_its_neighbours_stay_put() {
        let strip = ["a", "b", "c", "d"]
            .into_iter()
            .fold(widget::Row::new(), |row, name| {
                row.push(button(50.0, mode(name)))
            })
            .into();
        let mut left = Region::default();
        left.push(button(30.0, mode("launcher")), false);
        left.push(left.overflow(strip, 24.0), true);
        let mut content = regions(
            [
                left,
                button(50.0, Message::ClosePopup).into(),
                button(50.0, Message::ClosePopup).into(),
            ],
            24.0,
        );
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 24.0)),
        );
        // The center and right keep their width; the island takes the rest.
        let left = &node.children()[0];
        assert_eq!(left.size().width, 88.0);
        let islands: Vec<_> = left.children().iter().map(|node| node.bounds()).collect();
        assert_eq!((islands[0].x, islands[0].width), (0.0, 30.0));
        assert_eq!((islands[1].x, islands[1].width), (36.0, 52.0));

        let wheel = Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -400.0 },
        });
        // A wheel over the launcher moves nothing: it is not in a scrollport.
        dispatch(
            &mut content,
            &mut tree,
            &node,
            &renderer,
            &wheel,
            Point::new(10.0, 10.0),
            &mut Vec::new(),
        );
        assert!(matches!(
            clicked(&mut content, &mut tree, &node, &renderer, Point::new(40.0, 10.0)).as_slice(),
            [Message::Control(crate::control::Command::BrightnessMode(Some(name)))] if name == "a"
        ));
        // Over the island it reaches the last item, and the launcher stays.
        dispatch(
            &mut content,
            &mut tree,
            &node,
            &renderer,
            &wheel,
            Point::new(60.0, 10.0),
            &mut Vec::new(),
        );
        assert!(matches!(
            clicked(&mut content, &mut tree, &node, &renderer, Point::new(80.0, 10.0)).as_slice(),
            [Message::Control(crate::control::Command::BrightnessMode(Some(name)))] if name == "d"
        ));
        assert!(matches!(
            clicked(&mut content, &mut tree, &node, &renderer, Point::new(10.0, 10.0)).as_slice(),
            [Message::Control(crate::control::Command::BrightnessMode(Some(name)))] if name == "launcher"
        ));
    }

    #[test]
    fn overflow_island_keeps_a_cell_when_the_bar_is_crowded() {
        let mut left = Region::default();
        left.push(button(30.0, Message::ClosePopup), false);
        // Padded like a bar cell: the floor is that padding plus one cell.
        let strip = left
            .overflow(button(400.0, Message::ClosePopup), 24.0)
            .apply(widget::container)
            .padding([0.0, 10.0])
            .into();
        left.push(strip, true);
        let mut content = regions(
            [
                left,
                button(400.0, Message::ClosePopup).into(),
                button(400.0, Message::ClosePopup).into(),
            ],
            24.0,
        );
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 24.0)),
        );
        // Launcher, spacing, padding and one bar-height cell; the edges share
        // the rest.
        let widths: Vec<_> = node
            .children()
            .iter()
            .map(|node| node.size().width)
            .collect();
        assert_eq!(widths, [80.0, 104.0, 104.0]);
        let strip = &node.children()[0].children()[1];
        assert_eq!(strip.size().width, 44.0);
        assert_eq!(strip.children()[0].size().width, 24.0);
    }
}
