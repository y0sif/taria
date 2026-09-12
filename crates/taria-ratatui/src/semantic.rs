//! The [`sem`] wrapper: attach a semantic node to any ratatui widget.

use std::cell::RefCell;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use taria::Node;

use crate::FrameRecorder;

/// Wrap a widget with its semantic [`Node`] so rendering it also records the
/// node into the current frame.
///
/// The returned [`Semantic`] is itself a [`Widget`]; render it exactly like
/// the widget it wraps (e.g. via `Frame::render_widget`). The node is
/// recorded in render order, interleaved with any [`FrameRecorder::push`]
/// calls.
///
/// ```no_run
/// # use ratatui::widgets::Paragraph;
/// # use taria::{Node, Role};
/// # use taria_ratatui::{TariaLayer, sem};
/// # fn main() -> std::io::Result<()> {
/// // Never fails: without a socket the layer is inert and this code path
/// // is unchanged.
/// let mut layer = TariaLayer::bind_or_disabled("demo");
/// let mut terminal = ratatui::init();
/// let rec = layer.frame();
/// terminal.draw(|frame| {
///     frame.render_widget(
///         sem(
///             &rec,
///             Paragraph::new("hello"),
///             Node::new("greeting", Role::Text).label("hello"),
///         ),
///         frame.area(),
///     );
/// })?;
/// rec.publish();
/// # Ok(())
/// # }
/// ```
pub fn sem<'rec, W: Widget>(
    rec: &'rec FrameRecorder<'_>,
    widget: W,
    node: Node,
) -> Semantic<'rec, W> {
    Semantic {
        widget,
        node,
        sink: rec.sink(),
    }
}

/// A widget paired with its semantic node; created by [`sem`].
pub struct Semantic<'a, W> {
    widget: W,
    node: Node,
    sink: &'a RefCell<Vec<Node>>,
}

impl<W: Widget> Widget for Semantic<'_, W> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // The borrow is released before the inner widget renders, so nested
        // `Semantic`s recording into the same frame cannot conflict. The
        // fallible form keeps the library panic-free regardless.
        if let Ok(mut nodes) = self.sink.try_borrow_mut() {
            nodes.push(self.node);
        }
        self.widget.render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;
    use taria::{Node, Role};

    use super::sem;
    use crate::test_util::{TestLayer, bind_test_layer};

    /// Bind a layer on a throwaway socket path cleaned up on drop.
    fn test_layer(label: &str) -> TestLayer {
        bind_test_layer("taria-sem-", label)
    }

    #[test]
    fn renders_inner_widget_and_records_node() {
        let mut layer = test_layer("semantic");
        let rec = layer.frame();

        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        // `&str` implements `Widget`, keeping the test dependency-free.
        sem(&rec, "hello", Node::new("greeting", Role::Text).label("hi")).render(area, &mut buf);

        assert_eq!(buf, Buffer::with_lines(["hello"]));

        rec.publish();
        let root = layer.latest_snapshot().unwrap().root;
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].id.0, "greeting");
        assert_eq!(root.children[0].label.as_deref(), Some("hi"));
    }

    #[test]
    fn sem_and_push_interleave_in_call_order() {
        let mut layer = test_layer("interleave");
        let mut rec = layer.frame();

        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);

        rec.push(Node::new("first", Role::Text));
        sem(&rec, "x", Node::new("second", Role::Text)).render(area, &mut buf);
        rec.push(Node::new("third", Role::Text));
        rec.publish();

        let root = layer.latest_snapshot().unwrap().root;
        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["first", "second", "third"]);
    }
}
