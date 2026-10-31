use serde::{Deserialize, Serialize};

use crate::wire::InputId;
use crate::{Node, PROTOCOL_VERSION, state};

/// One published state of the app's semantic tree.
///
/// `#[non_exhaustive]` for the same reason as [`Node`]: new optional fields
/// are the format's cheapest additive change, and [`new`](Self::new) already
/// builds one, so closing the struct literal costs a caller nothing.
/// [`after_input`](Self::after_input) is the first such field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Snapshot {
    /// The [`PROTOCOL_VERSION`] the snapshot is
    /// written in; version negotiation happens on the hello, not here.
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
    ///
    /// The one resend that keeps its `seq` is a stamp moving: an adapter that
    /// stamps sends its latest tree again, unchanged and under the same `seq`,
    /// when the app has dequeued an input since that tree went out. Only
    /// [`after_input`](Self::after_input) differs, so a reader that ignores
    /// that field sees the same snapshot twice.
    pub seq: u64,
    /// The newest input the app had dequeued on this connection when it
    /// published this tree, or `None` when it had dequeued none.
    ///
    /// A stamp says the tree was published after the app took that input off
    /// its queue, and so after every input the bridge sent before it. A
    /// bridge that sent input `N` credits a tree to it only once the stamp
    /// reaches `N` in its own send order: a tree stamped lower was published
    /// before the app had `N` in hand, whenever it arrived. Without the stamp
    /// a bridge cannot tell the two apart, and a slow app's tree is credited
    /// to whatever input was sent next.
    ///
    /// A stamp covers the dequeue, not the effect. An app that publishes
    /// before it applies what it dequeued sends a stamped tree that lacks the
    /// effect, which is why the integration guide asks for the first tree
    /// published after a drain to carry it.
    ///
    /// Stamps belong to one connection, because ids belong to one bridge
    /// process: the first snapshot a new connection receives is unstamped.
    /// Dropped inputs, inputs of a kind the app cannot read, and inputs the
    /// app discards as stale never stamp, since none was dequeued. A peer
    /// that stamps says so in its hello
    /// ([`AppToBridge::Hello`](crate::wire::AppToBridge::Hello)'s `stamps`).
    ///
    /// Omitted when unset, and written before `root`. Read leniently: a value
    /// that is not an unsigned integer reads as unset rather than failing the
    /// snapshot. Set it with [`after`](Self::after).
    #[serde(
        default,
        deserialize_with = "state::lenient_input_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub after_input: Option<InputId>,
    /// The root of the tree.
    pub root: Node,
}

impl Snapshot {
    /// Create a snapshot at the crate's current [`PROTOCOL_VERSION`], with no
    /// stamp.
    pub fn new(seq: u64, root: Node) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            seq,
            after_input: None,
            root,
        }
    }

    /// Stamp the snapshot as published after the app dequeued `id`; see
    /// [`after_input`](Self::after_input).
    pub fn after(mut self, id: InputId) -> Self {
        self.after_input = Some(id);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::AppToBridge;
    use crate::{Action, Role};

    #[test]
    fn new_fills_protocol_version() {
        let snapshot = Snapshot::new(42, Node::new("root", Role::App));
        assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
        assert_eq!(snapshot.seq, 42);
        assert_eq!(snapshot.after_input, None);
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

        let stamped = snapshot.after(InputId::new(9));
        let json = serde_json::to_string(&stamped).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, stamped);
    }

    /// An unstamped snapshot is the line 0.3.3 wrote, byte for byte, and a
    /// stamp goes between `seq` and `root`.
    #[test]
    fn the_stamp_is_omitted_when_unset_and_written_before_the_root() {
        let root = Node::new("app", Role::App);
        let line =
            |snapshot: Snapshot| serde_json::to_string(&AppToBridge::Snapshot(snapshot)).unwrap();
        assert_eq!(
            line(Snapshot::new(2, root.clone())),
            r#"{"type":"snapshot","protocol_version":1,"seq":2,"root":{"id":"app","role":"app","focused":false}}"#
        );
        assert_eq!(
            line(Snapshot::new(2, root).after(InputId::new(7))),
            r#"{"type":"snapshot","protocol_version":1,"seq":2,"after_input":7,"root":{"id":"app","role":"app","focused":false}}"#
        );
    }

    /// A stamp of the wrong shape costs the stamp, not the tree.
    #[test]
    fn a_wrong_shaped_stamp_reads_as_unset() {
        for stamp in ["\"7\"", "-1", "7.5", "null", "true", "[7]", "{\"id\":7}"] {
            let line = format!(
                r#"{{"protocol_version":1,"seq":2,"after_input":{stamp},"root":{{"id":"app","role":"app"}}}}"#
            );
            let snapshot: Snapshot = serde_json::from_str(&line).expect(&line);
            assert_eq!(snapshot.after_input, None, "{stamp}");
            assert_eq!(snapshot.seq, 2, "{stamp}");
        }
        let line = r#"{"protocol_version":1,"seq":2,"after_input":18446744073709551615,"root":{"id":"app","role":"app"}}"#;
        let snapshot: Snapshot = serde_json::from_str(line).unwrap();
        assert_eq!(snapshot.after_input, Some(InputId::new(u64::MAX)));
    }
}
