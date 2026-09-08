//! Prefixed node ids, so an app can get back from a [`NodeId`](crate::NodeId)
//! to the thing it named.
//!
//! Node ids are strings on the wire, but an app almost always derives them from
//! something it owns: a task index, a row key, a pane name. Without help every
//! app hand-rolls the same pair of `format!("task-{i}")` and
//! `strip_prefix("task-")` calls, and the two drift the moment one of them is
//! edited. An [`IdSpace`] is the single place that spelling lives.
//!
//! Ids are built as `<prefix>-<key>`. The separator is `-`, and it belongs to
//! the prefix boundary: an id is owned by the text before its *first*
//! separator, and everything after that is the key. Keys may contain the
//! separator themselves, so `"task-1-2"` has the key `"1-2"`; prefixes may not,
//! which is what keeps one id from belonging to two spaces at once.

use std::fmt;
use std::str::FromStr;

/// The separator between prefix and key.
///
/// Kept as one character so [`IdSpace::key`] can strip it without allocating.
const SEPARATOR: char = '-';

/// A namespace for node ids sharing one prefix.
///
/// Const-constructible, so an app can declare its spaces next to its tree
/// builder: `const TASK: IdSpace = IdSpace::new("task");`.
///
/// A prefix must not contain the separator. That is the space's one invariant,
/// and it is what makes an id belong to exactly one space: with it, two spaces
/// with different prefixes can never build the same id, because the text before
/// an id's first separator names the space that built it. Without it,
/// `IdSpace::new("task").id("delete-7")` and `IdSpace::new("task-delete").id(7)`
/// are both `"task-delete-7"`, which puts two nodes under one
/// [`NodeId`](crate::NodeId) in one snapshot and leaves an agent's act resolving
/// by dispatch order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdSpace {
    prefix: &'static str,
}

impl IdSpace {
    /// Create a space whose ids all start with `prefix`.
    ///
    /// `prefix` must not contain the separator. This constructor is `const` and
    /// infallible so a space can stand in a `const` item, and this crate does
    /// not panic, so a prefix that breaks the rule is not rejected here: it
    /// produces a space that owns nothing, whose [`key`](Self::key) and
    /// [`parse`](Self::parse) answer `None` for every id including the ones its
    /// own [`id`](Self::id) built. That is a lookup that fails on the app's
    /// first act rather than a duplicate id an agent silently acts on the wrong
    /// half of. Use [`try_new`](Self::try_new) for a prefix that is not a
    /// literal, and to reject one at compile time.
    pub const fn new(prefix: &'static str) -> Self {
        Self { prefix }
    }

    /// Create a space, or `None` if `prefix` contains the separator.
    ///
    /// The checked form of [`new`](Self::new), for a prefix assembled rather
    /// than written down: a build-time constant, a leaked configuration string.
    /// `const`, so a literal prefix can still be checked where it is declared
    /// and a bad one fails the build rather than the first lookup.
    pub const fn try_new(prefix: &'static str) -> Option<Self> {
        let bytes = prefix.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // The separator is ASCII, so a byte scan cannot split a character
            // or match part of a multi-byte one.
            if bytes[i] == SEPARATOR as u8 {
                return None;
            }
            i += 1;
        }
        Some(Self { prefix })
    }

    /// The prefix these ids carry, without the separator.
    ///
    /// Verbatim, including a prefix that breaks the rule on [`new`](Self::new),
    /// so an app printing this sees what it actually declared.
    pub fn prefix(&self) -> &'static str {
        self.prefix
    }

    /// Build the id for `key`: `IdSpace::new("task").id(7)` is `"task-7"`.
    ///
    /// A prefix containing the separator builds an id owned by another space,
    /// which is the failure [`new`](Self::new) describes.
    pub fn id(&self, key: impl fmt::Display) -> String {
        format!("{}{SEPARATOR}{key}", self.prefix)
    }

    /// Recover the key from an id, or `None` if the id belongs to another
    /// space.
    ///
    /// The id is split at its *first* separator, and the text before it must be
    /// this space's whole prefix. Splitting at the first one is what makes the
    /// answer independent of which other spaces exist: `"task-delete-7"` is the
    /// task space's `"delete-7"` and nothing else's, so a space declared as
    /// `"task-delete"` gets `None` here rather than a second claim on the same
    /// id.
    ///
    /// A bare prefix with no separator is not in this space (`"task"` is
    /// `None`), but an empty key is (`"task-"` is `Some("")`), because that is
    /// an id this space can produce.
    pub fn key<'a>(&self, id: &'a str) -> Option<&'a str> {
        let (head, key) = id.split_once(SEPARATOR)?;
        (head == self.prefix).then_some(key)
    }

    /// Recover the key and parse it, or `None` if the id belongs to another
    /// space or the key does not parse.
    ///
    /// The two failures collapse into one `None` on purpose: a caller looking
    /// up `"task-nope"` takes the same branch either way, and swallowing the
    /// parse error keeps that branch a single `let else`.
    pub fn parse<T: FromStr>(&self, id: &str) -> Option<T> {
        self.key(id)?.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: IdSpace = IdSpace::new("task");

    /// `try_new` is usable where `new` is, which is the point of it being
    /// `const`: a prefix that breaks the rule fails this build rather than the
    /// first lookup at run time.
    const CHECKED: IdSpace = match IdSpace::try_new("task") {
        Some(space) => space,
        None => panic!("`task` holds no separator"),
    };

    #[test]
    fn id_and_key_roundtrip() {
        let cases = ["0", "7", "1-2", ""];
        for key in cases {
            let id = TASK.id(key);
            assert_eq!(TASK.key(&id), Some(key), "key: {key:?}");
        }
        assert_eq!(TASK.id(7), "task-7");
        assert_eq!(TASK.prefix(), "task");
    }

    #[test]
    fn key_rejects_ids_from_other_spaces() {
        let cases = ["task", "other-1", "taskfoo", "", "-1", "Task-1"];
        for id in cases {
            assert_eq!(TASK.key(id), None, "id: {id:?}");
        }
    }

    #[test]
    fn empty_key_is_in_the_space() {
        assert_eq!(TASK.key("task-"), Some(""));
        assert_eq!(TASK.key("task-1-2"), Some("1-2"));
    }

    #[test]
    fn try_new_rejects_prefixes_holding_the_separator() {
        assert_eq!(CHECKED, TASK);
        assert_eq!(IdSpace::try_new("task"), Some(TASK));
        // No separator, so nothing here is ambiguous: `"-7"` is this space's
        // `"7"` and no other space can build it.
        assert_eq!(
            IdSpace::try_new("").map(|space| space.id(7)),
            Some("-7".to_string())
        );
        assert_eq!(IdSpace::try_new("task-delete"), None);
        assert_eq!(IdSpace::try_new("-"), None);
        assert_eq!(IdSpace::try_new("task-"), None);
    }

    /// The pair that used to collide: both spaces build `"task-delete-7"`, and
    /// both used to answer `Some` for it. An id now belongs to the space named
    /// before its first separator, so the `"task-delete"` space owns nothing,
    /// including what its own `id` built. Nothing resolves twice.
    #[test]
    fn a_prefix_holding_the_separator_owns_nothing() {
        const OVERLAP: IdSpace = IdSpace::new("task-delete");

        let id = OVERLAP.id(7);
        assert_eq!(id, TASK.id("delete-7"));
        assert_eq!(OVERLAP.key(&id), None);
        assert_eq!(OVERLAP.parse::<u64>(&id), None);
        assert_eq!(TASK.key(&id), Some("delete-7"));
    }

    #[test]
    fn parse_extracts_typed_keys() {
        assert_eq!(TASK.parse::<u64>("task-7"), Some(7));
        assert_eq!(TASK.parse::<usize>(&TASK.id(42)), Some(42));
    }

    #[test]
    fn parse_returns_none_for_unparseable_or_foreign_keys() {
        assert_eq!(TASK.parse::<u64>("task-nope"), None);
        assert_eq!(TASK.parse::<u64>("task-"), None);
        assert_eq!(TASK.parse::<u64>("task-1-2"), None);
        assert_eq!(TASK.parse::<u64>("other-1"), None);
        assert_eq!(TASK.parse::<u64>("task"), None);
    }
}
