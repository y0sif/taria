use serde::{Deserialize, Serialize};

use crate::{Node, PROTOCOL_VERSION};

/// One published state of the app's semantic tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol_version: u32,
    /// Monotonic sequence number so agents can detect staleness.
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
