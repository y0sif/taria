//! Per-frame recording of the semantic tree.

use std::cell::RefCell;

use taria::Node;

use crate::TariaLayer;

/// Records the semantic nodes of one rendered frame.
///
/// Created by [`TariaLayer::frame`]. Record top-level nodes explicitly with
/// [`push`](Self::push), or implicitly by rendering widgets wrapped with
/// [`sem`](crate::sem); order is preserved either way. Finish the frame with
/// [`publish`](Self::publish).
pub struct FrameRecorder<'a> {
    layer: &'a mut TariaLayer,
    /// Interior mutability so [`sem`](crate::sem) can record through a shared
    /// reference from inside `Widget::render`. Single-threaded by
    /// construction (the render path), so a `RefCell` suffices.
    nodes: RefCell<Vec<Node>>,
}

impl<'a> FrameRecorder<'a> {
    pub(crate) fn new(layer: &'a mut TariaLayer) -> Self {
        Self {
            layer,
            nodes: RefCell::new(Vec::new()),
        }
    }

    /// Record one top-level node for this frame. Apps build nodes with
    /// taria's fluent builders, including manual children for composite
    /// widgets like dialogs.
    pub fn push(&mut self, node: Node) {
        self.nodes.get_mut().push(node);
    }

    /// Where [`sem`](crate::sem)-wrapped widgets record their nodes.
    pub(crate) fn sink(&self) -> &RefCell<Vec<Node>> {
        &self.nodes
    }

    /// Publish the recorded frame as a new snapshot.
    ///
    /// The recorded nodes become children of an auto-generated root node
    /// (`id: "app"`, role `App`, labeled with the app label). The root is
    /// marked focused only when no recorded node (or descendant) is, so a
    /// snapshot always carries focus somewhere. `seq` increments per
    /// published snapshot; a frame identical to the previous one is skipped
    /// entirely. This never blocks the render path.
    pub fn publish(self) {
        let Self { layer, nodes } = self;
        layer.publish_nodes(nodes.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use taria::{Node, NodeId, Role};

    use crate::TariaLayer;

    /// Bind a layer on a unique throwaway socket path.
    fn test_layer(label: &str) -> TariaLayer {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir: PathBuf = std::env::temp_dir().join(format!("taria-rec-{}", std::process::id()));
        TariaLayer::bind_at(label, dir.join(format!("{label}-{n}.sock"))).unwrap()
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

    #[test]
    fn empty_frame_publishes_bare_root() {
        let mut layer = test_layer("empty");
        layer.frame().publish();

        let root = layer.latest_snapshot().unwrap().root;
        assert!(root.children.is_empty());
        assert!(root.focused);
    }
}
