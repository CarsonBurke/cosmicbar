//! Keeps hover feedback honest on a layer surface.
//!
//! `cosmic::widget::button` decides between its active and hovered styles from
//! the cursor position it is handed at draw time
//! (`libcosmic/src/widget/button/widget.rs`, `is_mouse_over`), and the wayland
//! backend only refreshes that position on pointer *motion*: a
//! `wl_pointer.leave` updates the seat but leaves the window's last cursor
//! position untouched. On a normal window that never shows, because the pointer
//! can only leave across a border where a motion event lands first. On a 24px
//! layer surface the pointer leaves in one jump, and every button it was over
//! keeps painting itself hovered until something moves the cursor back.
//!
//! This wrapper sits at the root of a surface and hands its whole subtree the
//! cursor only while the pointer really is inside: events still carry the true
//! cursor, so clicks, drags and wheels are untouched, but drawing and cursor
//! shape see `Unavailable` once the pointer is gone. One node per surface fixes
//! every button underneath, module cells, workspace pills, taskbar items and
//! popup rows alike.

use cosmic::iced::advanced::widget::{Operation, Tree, tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::{Event as IcedEvent, Length, Point, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};

use crate::bar::Message;

/// Wrap a surface's content so hover feedback follows the real pointer.
pub fn guard<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Guard {
        content: content.into(),
    })
}


struct Guard<'a> {
    content: Element<'a, Message>,
}

/// Where the pointer is according to this surface's own events.
enum Seen {
    /// No pointer event yet: the first frame is drawn before any arrives, and a
    /// surface that appears under a resting pointer must still light the cell it
    /// is under, so trust whatever cursor the runtime hands over.
    Unseen,
    /// The position the last motion carried.
    At(Point),
    /// The pointer left.
    Gone,
}

impl Guard<'_> {
    /// The cursor as the subtree should *see* it: the position this surface was
    /// last told about, not the one the runtime kept from an earlier frame.
    fn shown(&self, tree: &Tree, cursor: mouse::Cursor) -> mouse::Cursor {
        match tree.state.downcast_ref::<Seen>() {
            Seen::Unseen => cursor,
            Seen::At(position) => mouse::Cursor::Available(*position),
            Seen::Gone => mouse::Cursor::Unavailable,
        }
    }
}

impl Widget<Message, Theme, Renderer> for Guard<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Seen>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Seen::Unseen)
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
        // Motion is the only event that carries a fresh position, and a leave
        // is the only notice that the pointer is gone; both arrive before the
        // frame that would paint a stale hover, and before the runtime asks what
        // cursor shape this surface wants.
        let seen = match event {
            IcedEvent::Mouse(mouse::Event::CursorLeft) => Some(Seen::Gone),
            IcedEvent::Mouse(mouse::Event::CursorMoved { position }) => {
                Some(Seen::At(*position))
            }
            _ => None,
        };
        if let Some(seen) = seen {
            *tree.state.downcast_mut::<Seen>() = seen;
            // A compositor only sends motion for the surface under the pointer,
            // so this asks for a frame exactly while the user is on the bar.
            shell.request_redraw();
        }
        // The subtree always gets the true cursor for events: a press outside
        // our bounds is not ours to hide, and a drag that leaves the surface
        // still belongs to the widget that started it.
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
            self.shown(tree, cursor),
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
            self.shown(tree, cursor),
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
