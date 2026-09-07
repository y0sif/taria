use serde::{Deserialize, Serialize};

use crate::{Node, PROTOCOL_VERSION};

/// One published state of the app's semantic tree.
///
/// `#[non_exhaustive]` for the same reason as [`Node`]: new optional fields
/// are the format's cheapest additive change, and [`new`](Self::new) already
/// builds one, so closing the struct literal costs a caller nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Snapshot {
    pub protocol_version: u32,
    /// Sequence number, incremented once per published snapshot within one run
    /// of the app.
    ///
    /// It tells which of two snapshots from the same run is the newer one, and
    /// nothing beyond that. A restarted app starts counting again, so a lower
    /// `seq` with different content is still a change and not a stale tree,
    /// and a reader that orders on `seq` across connections orders a fresh
    /// app's first tree before the previous app's last. A frame an adapter
    /// deduplicates, because its tree is identical to the one already
    /// published, does not move it either: it counts publishes, not frames.
    /// Compare whole snapshots to detect change.
    pub seq: u64,
    pub root: Node,
}

impl Snapshot {
    /// Create a snapshot at the crate's current
    /// [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
    pub fn new(seq: u64, root: Node) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            seq,
            root,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Role};

    #[test]
    fn new_fills_protocol_version() {
        let snapshot = Snapshot::new(42, Node::new("root", Role::App));
        assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
        assert_eq!(snapshot.seq, 42);
    }

    #[test]
    fn nested_snapshot_roundtrips() {
        let root = Node::new("root", Role::App).label("demo").children([
            Node::new("list", Role::List)
                .label("Tasks")
                .action(Action::Select)
                .child(
                    Node::new("item-1", Role::ListItem)
                        .label("Buy milk")
                        .focused(true)
                        .actions([Action::Toggle, Action::Custom("archive".into())]),
                ),
            Node::new("input", Role::TextInput).value("draft"),
        ]);
        let snapshot = Snapshot::new(3, root);
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snapshot);
    }
}
