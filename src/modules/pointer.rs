//! Wheel and right-click over a bar cell, without swallowing the click.
//!
//! `cosmic::widget::mouse_area` cannot do this job. The bar wraps a clickable
//! module in its own button, and `mouse_area` captures *every* left press — even
//! with no `on_press` set — which would stop that button from ever opening the
//! popup; it also publishes `on_scroll` twice per wheel event. This wrapper
//! forwards everything to its content and claims only the events it was given,
//! and only when the content has not already claimed them.

use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::{Event as IcedEvent, Length, Rectangle, Size, Vector};
use cosmic::Element;

use crate::bar::Message;

/// A high-resolution wheel reports pixels; this is one notch's worth.
const PIXELS_PER_NOTCH: f32 = 50.0;

pub struct Pointer<'a> {
    content: Element<'a, Message>,
    on_wheel: Option<fn(f32) -> Message>,
    on_right: Option<Message>,
}

impl<'a> Pointer<'a> {
    pub fn new(content: Element<'a, Message>) -> Self {
        Self {
            content,
            on_wheel: None,
            on_right: None,
        }
    }

    /// Wheel notches, positive away from the user.
    pub fn on_wheel(mut self, on_wheel: fn(f32) -> Message) -> Self {
        self.on_wheel = Some(on_wheel);
        self
    }

    pub fn on_right(mut self, on_right: Message) -> Self {
        self.on_right = Some(on_right);
        self
    }

    /// Nothing to wrap around: a cell with neither binding stays the plain
    /// element, so no extra widget node is laid out or diffed per frame.
    pub fn wrap(self) -> Element<'a, Message> {
        if self.on_wheel.is_none() && self.on_right.is_none() {
            self.content
        } else {
            Element::new(self)
        }
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Pointer<'_> {
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
        renderer: &cosmic::Renderer,
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
        renderer: &cosmic::Renderer,
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
        renderer: &cosmic::Renderer,
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
        if shell.is_event_captured() || !cursor.is_over(layout.bounds()) {
            return;
        }
        match event {
            IcedEvent::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(on_wheel) = self.on_wheel else {
                    return;
                };
                let notches = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => y / PIXELS_PER_NOTCH,
                };
                if notches != 0.0 {
                    shell.publish(on_wheel(notches));
                    shell.capture_event();
                }
            }
            IcedEvent::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                if let Some(on_right) = self.on_right.clone() {
                    shell.publish(on_right);
                    shell.capture_event();
                }
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
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
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<Pointer<'a>> for Element<'a, Message> {
    fn from(pointer: Pointer<'a>) -> Self {
        Element::new(pointer)
    }
}
