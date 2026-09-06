//! Prefixed node ids, so an app can get back from a [`NodeId`](crate::NodeId)
//! to the thing it named.
//!
//! Node ids are strings on the wire, but an app almost always derives them from
//! something it owns: a task index, a row key, a pane name. Without help every
//! app hand-rolls the same pair of `format!("task-{i}")` and
//! `strip_prefix("task-")` calls, and the two drift the moment one of them is
//! edited. An [`IdSpace`] is the single place that spelling lives.
//!
//! Ids are built as `<prefix>-<key>`. The separator is `-`; keys may contain it
//! themselves, so `"task-1-2"` has the key `"1-2"`.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdSpace {
    prefix: &'static str,
}

impl IdSpace {
    /// Create a space whose ids all start with `prefix`.
    pub const fn new(prefix: &'static str) -> Self {
        Self { prefix }
    }

    /// The prefix these ids carry, without the separator.
    pub fn prefix(&self) -> &'static str {
        self.prefix
    }

    /// Build the id for `key`: `IdSpace::new("task").id(7)` is `"task-7"`.
    pub fn id(&self, key: impl fmt::Display) -> String {
        format!("{}{SEPARATOR}{key}", self.prefix)
    }

    /// Recover the key from an id, or `None` if the id belongs to another
    /// space.
    ///
    /// A bare prefix with no separator is not in this space (`"task"` is
    /// `None`), but an empty key is (`"task-"` is `Some("")`), because that is
    /// an id this space can produce.
    pub fn key<'a>(&self, id: &'a str) -> Option<&'a str> {
        id.strip_prefix(self.prefix)?.strip_prefix(SEPARATOR)
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
