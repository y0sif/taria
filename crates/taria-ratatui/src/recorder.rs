//! Per-frame recording of the semantic tree.

use std::cell::RefCell;
use std::mem;

use taria::Node;

use crate::TariaLayer;
use crate::layer::drop_iteratively;

/// Records the semantic nodes of one rendered frame.
///
/// Created by [`TariaLayer::frame`]. Record top-level nodes explicitly with
/// [`push`](Self::push), or implicitly by rendering widgets wrapped with
/// [`sem`](crate::sem); order is preserved either way. Finish the frame with
/// [`publish`](Self::publish).
pub struct FrameRecorder<'a> {
    layer: &'a mut TariaLayer,
    nodes: Recorded,
}

/// The nodes a frame has recorded so far.
///
/// Interior mutability so [`sem`](crate::sem) can record through a shared
/// reference from inside `Widget::render`. Single-threaded by construction
/// (the render path), so a `RefCell` suffices.
///
/// A type of its own, with no lifetime, so that freeing what it holds needs
/// no `Drop` on [`FrameRecorder`]. One there would keep the recorder's
/// borrow of the layer alive until the recorder goes out of scope rather
/// than until its last use, and an app that reads the layer after recording
/// a frame it then leaves unpublished would stop compiling.
struct Recorded(RefCell<Vec<Node>>);

impl Drop for Recorded {
    /// Free, without recursing, a frame that was recorded and never
    /// published, as the layer frees every tree it is handed: what was
    /// recorded can be deeper than the call stack.
    fn drop(&mut self) {
        mem::take(self.0.get_mut())
            .into_iter()
            .for_each(drop_iteratively);
    }
}

impl<'a> FrameRecorder<'a> {
    pub(crate) fn new(layer: &'a mut TariaLayer) -> Self {
        Self {
            layer,
            nodes: Recorded(RefCell::new(Vec::new())),
        }
    }

    /// Record one top-level node for this frame. Apps build nodes with
    /// taria's fluent builders, including manual children for composite
    /// widgets like dialogs.
    pub fn push(&mut self, node: Node) {
        self.nodes.0.get_mut().push(node);
    }

    /// Where [`sem`](crate::sem)-wrapped widgets record their nodes.
    pub(crate) fn sink(&self) -> &RefCell<Vec<Node>> {
        &self.nodes.0
    }

    /// Publish the recorded frame as a new snapshot.
    ///
    /// The recorded nodes become children of an auto-generated root node
    /// (`id: "app"`, role `App`, labeled with the app label). The root is
    /// marked focused only when no recorded node (or descendant) is, so a
    /// snapshot always carries focus somewhere. `seq` increments per
    /// published snapshot. A frame whose tree is the one the previous frame
    /// built is skipped before anything is cut or measured, and one that is
    /// cut to the snapshot already sent is not sent again, unless the app
    /// has dequeued an input since that snapshot went out: then it is sent
    /// again as it is, stamped with the input (see
    /// [`TariaLayer::publish`](crate::TariaLayer::publish), under Stamps).
    /// This never blocks the render path.
    ///
    /// A tree deeper than [`taria::MAX_NODE_DEPTH`] is cut rather than sent
    /// or skipped, because a snapshot past that depth does not arrive at the
    /// bridge at all and nothing says so. A tree too large for one line of
    /// [`taria::wire::MAX_LINE_BYTES`] is then trimmed to fit, because a
    /// longer line costs the app its bridge on every reconnect. See
    /// [`TariaLayer::publish`](crate::TariaLayer::publish) for why, and
    /// [`TariaLayer::stats`](crate::TariaLayer::stats) for how an app finds
    /// out.
    pub fn publish(mut self) {
        let nodes = mem::take(self.nodes.0.get_mut());
        self.layer.publish_nodes(nodes);
    }
}

#[cfg(test)]
mod tests {
    use taria::{Node, NodeId, Role};

    use crate::test_util::{TestLayer, bind_test_layer};

    /// Bind a layer on a throwaway socket path cleaned up on drop.
    fn test_layer(label: &str) -> TestLayer {
        bind_test_layer("taria-rec-", label)
    }

    fn child_ids(root: &Node) -> Vec<&str> {
        root.children.iter().map(|c| c.id.0.as_str()).collect()
    }

    #[test]
    fn push_preserves_order() {
        let mut layer = test_layer("order");
        let mut rec = layer.frame();
        rec.push(Node::new("first", Role::Text));
        rec.push(Node::new("second", Role::List));
        rec.push(Node::new("third", Role::Button));
        rec.publish();

        let snapshot = layer.latest_snapshot().unwrap();
        assert_eq!(child_ids(&snapshot.root), ["first", "second", "third"]);
    }

    #[test]
    fn publish_wraps_nodes_in_auto_root() {
        let mut layer = test_layer("root");
        let mut rec = layer.frame();
        rec.push(Node::new("pane", Role::Pane));
        rec.publish();

        let root = layer.latest_snapshot().unwrap().root;
        assert_eq!(root.id, NodeId("app".into()));
        assert_eq!(root.role, Role::App);
        assert_eq!(root.label.as_deref(), Some("root"));
        assert_eq!(root.children.len(), 1);
    }

    #[test]
    fn root_focused_only_when_no_recorded_node_is() {
        let mut layer = test_layer("focus");

        let mut rec = layer.frame();
        rec.push(Node::new("pane", Role::Pane));
        rec.publish();
        assert!(layer.latest_snapshot().unwrap().root.focused);

        let mut rec = layer.frame();
        rec.push(
            Node::new("pane", Role::Pane).child(Node::new("input", Role::TextInput).focused(true)),
        );
        rec.publish();
        let root = layer.latest_snapshot().unwrap().root;
        assert!(!root.focused, "deep focus must unfocus the auto root");
    }

    #[test]
    fn seq_increments_per_published_snapshot() {
        let mut layer = test_layer("seq");

        let mut rec = layer.frame();
        rec.push(Node::new("a", Role::Text));
        rec.publish();
        assert_eq!(layer.latest_snapshot().unwrap().seq, 1);

        let mut rec = layer.frame();
        rec.push(Node::new("b", Role::Text));
        rec.publish();
        assert_eq!(layer.latest_snapshot().unwrap().seq, 2);
    }

    #[test]
    fn identical_snapshot_is_deduped() {
        let mut layer = test_layer("dedup");

        let mut rec = layer.frame();
        rec.push(Node::new("a", Role::Text).label("same"));
        rec.publish();
        assert_eq!(layer.latest_snapshot().unwrap().seq, 1);

        // Identical tree: no new snapshot, seq unchanged.
        let mut rec = layer.frame();
        rec.push(Node::new("a", Role::Text).label("same"));
        rec.publish();
        assert_eq!(layer.latest_snapshot().unwrap().seq, 1);

        // Any difference publishes again.
        let mut rec = layer.frame();
        rec.push(Node::new("a", Role::Text).label("changed"));
        rec.publish();
        assert_eq!(layer.latest_snapshot().unwrap().seq, 2);
    }

    /// Compiled rather than run for what it checks: a recorder holds the
    /// layer only until its last use, as it did in 0.3.0. A `Drop` on
    /// [`FrameRecorder`](crate::FrameRecorder) itself would hold it to the
    /// end of the scope, and reading the layer before giving up on a frame
    /// would be rejected as a second borrow (E0502).
    #[test]
    fn a_recorder_left_unpublished_holds_the_layer_only_until_its_last_use() {
        let mut layer = test_layer("borrow");
        for paused in [true, false] {
            let node = Node::new("a", Role::Text);
            let mut rec = layer.frame();
            rec.push(node);
            if paused {
                layer.stats();
                continue;
            }
            rec.publish();
        }
        assert_eq!(layer.latest_snapshot().unwrap().seq, 1);
    }

    #[test]
    fn empty_frame_publishes_bare_root() {
        let mut layer = test_layer("empty");
        layer.frame().publish();

        let root = layer.latest_snapshot().unwrap().root;
        assert!(root.children.is_empty());
        assert!(root.focused);
    }
}
