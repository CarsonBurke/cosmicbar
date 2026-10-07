//! Measured bar regions: centered when possible, constrained to the free gap.
//!
//! Each region has its own horizontal scroll viewport. If content cannot fit,
//! large regions share a width cap while small regions keep their natural size;
//! scrolling reveals the rest without painting or clicking into a neighbour.

use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::widget::scrollable::{Direction, Scrollbar};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme, widget};
use std::cell::Cell;
use std::rc::Rc;

use crate::bar::Message;

const GAP: f32 = 6.0;

pub fn regions<'a>(content: [Element<'a, Message>; 3], height: f32) -> Element<'a, Message> {
    let captures: [Rc<Cell<bool>>; 3] = std::array::from_fn(|_| Rc::new(Cell::new(false)));
    Element::new(Regions {
        children: content
            .into_iter()
            .enumerate()
            .map(|(index, content)| {
                let region = widget::scrollable(Element::new(WheelProbe {
                    content,
                    captured: captures[index].clone(),
                }))
                .direction(Direction::Horizontal(
                    Scrollbar::new().width(3.0).scroller_width(3.0).margin(0.0),
                ))
                .class(cosmic::theme::iced::Scrollable::Minimal)
                .width(Length::Shrink)
                .height(Length::Fixed(height));
                // Preserve access to the rightmost controls when that edge
                // overflows; the other regions start at their leftmost item.
                if index == 2 {
                    region.anchor_right()
                } else {
                    region
                }
                .into()
            })
            .collect(),
        height,
        captures,
    })
}

struct Regions<'a> {
    children: Vec<Element<'a, Message>>,
    height: f32,
    captures: [Rc<Cell<bool>>; 3],
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
fn placement(width: f32, natural: [f32; 3]) -> Placement {
    let count = natural.iter().filter(|&&width| width > 0.0).count();
    let gap = if count > 1 {
        GAP.min(width / (count - 1) as f32)
    } else {
        0.0
    };
    let available = (width - gap * count.saturating_sub(1) as f32).max(0.0);
    let mut sorted = natural;
    sorted.sort_by(f32::total_cmp);
    let mut remaining = available;
    let mut cap = 0.0;
    for (index, natural) in sorted.into_iter().enumerate() {
        cap = remaining / (3 - index) as f32;
        if natural > cap {
            break;
        }
        remaining -= natural;
    }
    let widths = natural.map(|natural| natural.min(cap));
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
        let natural = std::array::from_fn(|index| nodes[index].size().width);
        let placement = placement(size.width, natural);
        for (index, ((child, state), node)) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(&mut nodes)
            .enumerate()
        {
            if placement.widths[index] < natural[index] {
                *node = child.as_widget_mut().layout(
                    state,
                    renderer,
                    &layout::Limits::new(
                        Size::ZERO,
                        Size::new(placement.widths[index], size.height),
                    ),
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
        for (index, ((child, state), bounds)) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .enumerate()
        {
            self.captures[index].set(false);
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
            // Ordinary wheels reveal overflowing passive content too. Module
            // wheel bindings get first refusal, so volume/brightness gestures
            // keep their normal meaning. Horizontal gestures remain native.
            if !self.captures[index].get()
                && cursor.is_over(bounds.bounds())
                && bounds
                    .children()
                    .next()
                    .is_some_and(|content| content.bounds().width > bounds.bounds().width)
            {
                let delta = match event {
                    Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Lines { x: 0.0, y },
                    }) => Some(mouse::ScrollDelta::Lines {
                        x: if index == 2 { -*y } else { *y },
                        y: 0.0,
                    }),
                    Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Pixels { x: 0.0, y },
                    }) => Some(mouse::ScrollDelta::Pixels {
                        x: if index == 2 { -*y } else { *y },
                        y: 0.0,
                    }),
                    _ => None,
                };
                if let Some(delta) = delta {
                    // The native wrapper may have captured the unmoved original
                    // wheel. Give the remapped event a fresh capture status.
                    let mut messages = Vec::new();
                    let mut remapped = Shell::new(&mut messages);
                    child.as_widget_mut().update(
                        state,
                        &Event::Mouse(mouse::Event::WheelScrolled { delta }),
                        bounds.with_virtual_offset(layout.virtual_offset()),
                        cursor,
                        renderer,
                        clipboard,
                        &mut remapped,
                        viewport,
                    );
                    shell.merge(remapped, |message| message);
                }
            }
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
        let plan = placement(1000.0, [100.0, 200.0, 250.0]);
        assert_eq!(plan.x, [0.0, 400.0, 750.0]);
        assert_eq!(plan.widths, [100.0, 200.0, 250.0]);
    }

    #[test]
    fn asymmetric_edges_shift_center_without_shrinking_content_that_fits() {
        let plan = placement(952.0, [60.0, 200.0, 600.0]);
        assert_eq!(plan.x, [0.0, 146.0, 352.0]);
        assert_eq!(plan.widths, [60.0, 200.0, 600.0]);
        let mirror = placement(952.0, [600.0, 200.0, 60.0]);
        assert_eq!(mirror.x, [0.0, 606.0, 892.0]);
    }

    #[test]
    fn overflow_preserves_small_regions_and_caps_larger_regions_without_overlap() {
        let plan = placement(600.0, [50.0, 400.0, 800.0]);
        assert_eq!(plan.widths, [50.0, 269.0, 269.0]);
        assert_eq!(plan.x, [0.0, 56.0, 331.0]);
        for width in [0.0, 1.0, 4.0, 12.0, 50.0, 200.0, 1200.0] {
            for natural in [
                [0.0, 0.0, 0.0],
                [200.0, 0.0, 400.0],
                [0.0, 200.0, 400.0],
                [200.0, 400.0, 0.0],
                [200.0, 400.0, 800.0],
            ] {
                let plan = placement(width, natural);
                for i in 0..3 {
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

    #[test]
    fn measured_widget_preserves_right_click_target_when_center_shifts_left() {
        let mut content = regions(
            [
                button(60.0, Message::ClosePopup),
                button(200.0, Message::Control(crate::control::Command::Reload)),
                button(600.0, Message::Control(crate::control::Command::Close)),
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
        let left = widget::Row::new()
            .push(button(200.0, Message::ClosePopup))
            .push(button(
                200.0,
                Message::Control(crate::control::Command::Reload),
            ))
            .into();
        let mut content = regions(
            [
                left,
                button(50.0, Message::Control(crate::control::Command::Close)),
                button(
                    50.0,
                    Message::Control(crate::control::Command::BrightnessMode(None)),
                ),
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
        let left = widget::Row::new()
            .push(button(200.0, Message::ClosePopup))
            .push(button(
                200.0,
                Message::Control(crate::control::Command::Reload),
            ))
            .into();
        let mut content = regions(
            [
                left,
                button(50.0, Message::ClosePopup),
                button(50.0, Message::ClosePopup),
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
                left,
                button(50.0, Message::ClosePopup),
                button(50.0, Message::ClosePopup),
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
}
