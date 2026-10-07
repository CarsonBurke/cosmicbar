//! Popup anchors in visible surface coordinates, including scrolled bar regions.

use std::cell::Cell;

use cosmic::iced::Subscription;
use cosmic::iced::advanced::widget::{Operation, Tree, tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::futures::{SinkExt, StreamExt, channel::mpsc};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};

use crate::bar::{Message, TrackedRect};

#[derive(Debug, Clone)]
pub enum Update {
    Init(Tracker),
    Rectangle((TrackedRect, Option<Rectangle>)),
}

#[derive(Debug, Clone)]
pub struct Tracker {
    sender: mpsc::UnboundedSender<(TrackedRect, Option<Rectangle>)>,
}

impl Tracker {
    pub fn container<'a>(
        &self,
        key: TrackedRect,
        content: impl Into<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        Element::new(Tracked {
            key,
            content: content.into(),
            tracker: self.clone(),
        })
    }
}

pub fn subscription() -> Subscription<Message> {
    Subscription::run(|| {
        cosmic::iced::stream::channel(4, async |mut output| {
            let (sender, mut updates) = mpsc::unbounded();
            if output
                .send(Message::Rect(Update::Init(Tracker { sender })))
                .await
                .is_err()
            {
                return;
            }
            while let Some(update) = updates.next().await {
                if output
                    .send(Message::Rect(Update::Rectangle(update)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
    })
}

struct Tracked<'a> {
    key: TrackedRect,
    content: Element<'a, Message>,
    tracker: Tracker,
}

#[derive(Default)]
struct State {
    // Outer None means no report yet; the rectangle None means clipped out.
    last: Cell<Option<(TrackedRect, Option<Rectangle>)>>,
}

fn visible_bounds(layout: Layout<'_>, viewport: &Rectangle) -> Option<Rectangle> {
    layout
        .bounds()
        .intersection(viewport)
        .filter(|bounds| bounds.width > 0.0 && bounds.height > 0.0)
        .map(|bounds| Rectangle {
            x: bounds.x - layout.virtual_offset().x,
            y: bounds.y - layout.virtual_offset().y,
            ..bounds
        })
}

impl Widget<Message, Theme, Renderer> for Tracked<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
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
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Publish before the click message so a click immediately after a wheel
        // event cannot open its popup using geometry from the previous frame.
        let bounds = visible_bounds(layout, viewport);
        let last = &tree.state.downcast_ref::<State>().last;
        let changed = last.replace(Some((self.key, bounds))) != Some((self.key, bounds));
        let clicking = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::ButtonReleased(_))
                | Event::Touch(
                    cosmic::iced::touch::Event::FingerPressed { .. }
                        | cosmic::iced::touch::Event::FingerLifted { .. }
                )
        ) && cursor.is_over(layout.bounds());
        // A draw may have cached this geometry while its subscription message
        // is still queued. Clicks need their own direct report before Toggle.
        if changed || clicking {
            shell.publish(Message::Rect(Update::Rectangle((self.key, bounds))));
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
        let bounds = visible_bounds(layout, viewport);
        let last = &tree.state.downcast_ref::<State>().last;
        if last.replace(Some((self.key, bounds))) != Some((self.key, bounds)) {
            let _ = self.tracker.sender.unbounded_send((self.key, bounds));
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

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::Point;

    #[test]
    fn scrolled_anchor_matches_visible_part_of_cell() {
        let node = layout::Node::new(Size::new(200.0, 24.0)).move_to(Point::new(150.0, 0.0));
        let layout = Layout::new(&node).with_virtual_offset(Vector::new(100.0, 0.0));
        let viewport = Rectangle {
            x: 180.0,
            y: 0.0,
            width: 100.0,
            height: 24.0,
        };
        assert_eq!(
            visible_bounds(layout, &viewport),
            Some(Rectangle {
                x: 80.0,
                y: 0.0,
                width: 100.0,
                height: 24.0
            })
        );
        let hidden = Rectangle {
            x: 400.0,
            ..viewport
        };
        assert_eq!(visible_bounds(layout, &hidden), None);
    }

    fn button(
        width: f32,
        parent: cosmic::iced::window::Id,
        module: crate::modules::ModuleId,
    ) -> Element<'static, Message> {
        cosmic::widget::button::custom(
            cosmic::widget::space::horizontal()
                .width(Length::Fixed(width))
                .height(Length::Fixed(24.0)),
        )
        .padding(0)
        .on_press(Message::Toggle(parent, module))
        .into()
    }

    #[test]
    fn reused_same_geometry_reports_its_new_module_key() {
        use crate::modules::ModuleId;
        let parent = cosmic::iced::window::Id::unique();
        let (sender, _updates) = mpsc::unbounded();
        let tracker = Tracker { sender };
        let mut old = tracker.container(
            (parent, ModuleId::Cpu),
            button(100.0, parent, ModuleId::Cpu),
        );
        let mut tree = Tree::new(&old);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = old.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 24.0)),
        );
        let viewport = Rectangle::new(Point::ORIGIN, node.size());
        let event = Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(10.0, 10.0),
        });
        let mut messages = Vec::new();
        old.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(10.0, 10.0)),
            &renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages.clear();
        // Matching cached bounds may have come from a draw whose asynchronous
        // report has not reached the app. A click still publishes them directly.
        old.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(10.0, 10.0)),
            &renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(matches!(
            messages.first(),
            Some(Message::Rect(Update::Rectangle((
                (_, ModuleId::Cpu),
                Some(_)
            ))))
        ));
        messages.clear();
        let mut replacement = tracker.container(
            (parent, ModuleId::Memory),
            button(100.0, parent, ModuleId::Memory),
        );
        replacement.as_widget_mut().diff(&mut tree);
        replacement.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(10.0, 10.0)),
            &renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(
            matches!(messages.first(), Some(Message::Rect(Update::Rectangle(((id, ModuleId::Memory), Some(_))))) if *id == parent)
        );
    }

    #[test]
    fn click_after_scroll_publishes_visible_anchor_before_toggle() {
        use crate::modules::ModuleId;
        let parent = cosmic::iced::window::Id::unique();
        let (sender, _updates) = mpsc::unbounded();
        let tracker = Tracker { sender };
        let left = cosmic::widget::Row::new()
            .push(tracker.container(
                (parent, ModuleId::Cpu),
                button(200.0, parent, ModuleId::Cpu),
            ))
            .push(tracker.container(
                (parent, ModuleId::Memory),
                button(200.0, parent, ModuleId::Memory),
            ))
            .into();
        let mut content = crate::regions::regions(
            [
                left,
                button(50.0, parent, ModuleId::Date),
                button(50.0, parent, ModuleId::Power),
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
        let viewport = Rectangle::new(Point::ORIGIN, node.size());
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -400.0, y: 0.0 },
            }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            content.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(20.0, 10.0)),
                &renderer,
                &mut cosmic::iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        let toggle = messages
            .iter()
            .position(|message| matches!(message, Message::Toggle(_, ModuleId::Memory)))
            .expect("hidden cell becomes reachable");
        assert!(messages[..toggle].iter().any(|message| matches!(message,
            Message::Rect(Update::Rectangle(((_, ModuleId::Memory), Some(rect)))) if rect.x == 0.0 && rect.width == 88.0)));
    }

    #[test]
    fn cached_scrolled_touch_anchor_is_published_using_transformed_cursor() {
        use crate::modules::ModuleId;
        let parent = cosmic::iced::window::Id::unique();
        let key = (parent, ModuleId::Memory);
        let (sender, _updates) = mpsc::unbounded();
        let tracker = Tracker { sender };
        let mut content = tracker.container(key, button(200.0, parent, ModuleId::Memory));
        let mut tree = Tree::new(&content);
        let renderer = Renderer::new(cosmic::iced::Font::DEFAULT, cosmic::iced::Pixels(16.0));
        let node = content
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(200.0, 24.0)),
            )
            .move_to(Point::new(150.0, 0.0));
        let layout = Layout::new(&node).with_virtual_offset(Vector::new(100.0, 0.0));
        let viewport = Rectangle {
            x: 180.0,
            y: 0.0,
            width: 100.0,
            height: 24.0,
        };
        tree.state
            .downcast_ref::<State>()
            .last
            .set(Some((key, visible_bounds(layout, &viewport))));
        let mut messages = Vec::new();
        for event in [
            Event::Touch(cosmic::iced::touch::Event::FingerPressed {
                id: cosmic::iced::touch::Finger(1),
                position: Point::new(90.0, 10.0),
            }),
            Event::Touch(cosmic::iced::touch::Event::FingerLifted {
                id: cosmic::iced::touch::Finger(1),
                position: Point::new(90.0, 10.0),
            }),
        ] {
            content.as_widget_mut().update(
                &mut tree,
                &event,
                layout,
                mouse::Cursor::Available(Point::new(190.0, 10.0)),
                &renderer,
                &mut cosmic::iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert!(
            matches!(messages.first(), Some(Message::Rect(Update::Rectangle(((_, ModuleId::Memory), Some(rect))))) if rect.x == 80.0 && rect.width == 100.0)
        );
        assert!(matches!(
            messages.last(),
            Some(Message::Toggle(_, ModuleId::Memory))
        ));
    }
}
