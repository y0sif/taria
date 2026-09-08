use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::NodeId;

/// An action a node advertises as currently available.
///
/// Built-ins are a snake_case string on the wire, [`Custom`](Self::Custom) is
/// `{"custom":"name"}`. An action name the reader does not know arrives as
/// [`Custom`](Self::Custom) carrying that name, rather than failing the
/// [`Snapshot`](crate::Snapshot) the action is nested in: a built-in added
/// later is still a name an agent can advertise, echo back, and have the app
/// recognize. This is also what `taria-mcp` already does with an action name an
/// agent supplies.
///
/// The two spellings converge on the way in: `{"custom":"activate"}` reads as
/// [`Activate`](Self::Activate), not as `Custom("activate")`, so an action
/// promoted to a built-in still reaches the peer that promoted it. The cost is
/// that a hand-built `Custom` holding a built-in name does not survive a round
/// trip, which is correct: it was never a distinct action, only the older
/// spelling of one.
///
/// `#[non_exhaustive]` says to the compiler what [`Custom`](Self::Custom) says
/// to the parser: this vocabulary keeps growing. An action promoted to a
/// built-in is additive on the wire, and the attribute is what makes it
/// additive in Rust too, so a peer that matches on actions keeps compiling
/// across a taria upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Action {
    Activate,
    Focus,
    Select,
    Toggle,
    Scroll,
    SetValue,
    Dismiss,
    /// App-specific action, described by a keybinding-independent name.
    Custom(String),
}

impl Action {
    /// Map a wire action name onto a built-in, degrading an unrecognized name
    /// to [`Custom`](Self::Custom).
    ///
    /// The names must stay in step with what the derived [`Serialize`] emits;
    /// the `every_action_roundtrips` test is what enforces that.
    fn from_wire(name: &str) -> Self {
        match name {
            "activate" => Action::Activate,
            "focus" => Action::Focus,
            "select" => Action::Select,
            "toggle" => Action::Toggle,
            "scroll" => Action::Scroll,
            "set_value" => Action::SetValue,
            "dismiss" => Action::Dismiss,
            other => Action::Custom(other.to_string()),
        }
    }
}

/// Hand-written so an unknown action name becomes [`Action::Custom`] instead of
/// an error. `#[serde(other)]` cannot express this: it is only available on
/// internally and adjacently tagged enums, and this one is externally tagged.
impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ActionVisitor;

        impl<'de> Visitor<'de> for ActionVisitor {
            type Value = Action;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"an action name or {"custom":"name"}"#)
            }

            fn visit_str<E>(self, name: &str) -> Result<Action, E>
            where
                E: de::Error,
            {
                Ok(Action::from_wire(name))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Action, A::Error>
            where
                A: MapAccess<'de>,
            {
                let Some(name) = map.next_key::<String>()? else {
                    return Err(de::Error::invalid_length(0, &self));
                };
                let action = if name == "custom" {
                    // Fold the envelope back onto the built-ins rather than
                    // trusting it, so `{"custom":"activate"}` and `"activate"`
                    // are the same action. Without this an action name cannot
                    // survive graduating to a built-in: an older peer reads
                    // the new name as `Custom`, correctly, and echoes it back
                    // in this form, and the newer peer's own variant never
                    // fires, so the act is accepted and silently does nothing.
                    Action::from_wire(&map.next_value::<String>()?)
                } else {
                    // A variant added later may carry a payload this build has
                    // no field for. Its name is still the useful part, so keep
                    // that and discard the payload.
                    map.next_value::<IgnoredAny>()?;
                    Action::from_wire(&name)
                };
                // Drain the rest: serde_json rejects a map the visitor left
                // half-read, and a longer object is exactly what a later
                // version might send.
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(action)
            }
        }

        deserializer.deserialize_any(ActionVisitor)
    }
}

/// Input submitted by an agent against a published snapshot.
///
/// `#[non_exhaustive]` because a new way for an agent to address an app (a
/// paste, a pointer event) is an additive message on the wire: an app reads a
/// `kind` it does not know as [`Unknown`](Self::Unknown). Without the
/// attribute the same addition breaks the build of every app that matches on
/// this enum to apply agent input, which is every app using an adapter.
///
/// Each variant carrying fields is marked too, so a field added to one of them
/// is additive in Rust as well as on the wire. For a peer that means two
/// things: build inputs with [`act`](Self::act), [`key`](Self::key) and
/// [`text`](Self::text) rather than by struct literal, and end a destructuring
/// pattern with `..`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum AgentInput {
    /// Invoke an advertised action on a node.
    #[non_exhaustive]
    Act {
        node: NodeId,
        action: Action,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// Raw key fallback for apps or regions without semantic coverage.
    ///
    /// The string follows the grammar in [`key`](crate::key), which both peers
    /// parse with the same code.
    #[non_exhaustive]
    Key { key: String },
    /// Literal text to type, lowered by the adapter into one key event per
    /// character. One message instead of one round trip per character.
    #[non_exhaustive]
    Text { text: String },
    /// An input whose `kind` this build does not recognize.
    ///
    /// It carries nothing beyond that fact. The tag it arrived under is gone
    /// and so is whatever it asked for, because there is no field here to keep
    /// them in. What survives is the [`InputId`](crate::wire::InputId) on the
    /// [`BridgeToApp::Input`](crate::wire::BridgeToApp::Input) around it, and
    /// that is the whole point of the variant: without it one input kind added
    /// in a later version 1 release fails the entire line, an app built before
    /// that kind skips the line, and the agent waits out a timeout for an ack
    /// that was never possible.
    ///
    /// So the right answer to one is an
    /// [`Ignored`](crate::wire::InputStatus::Ignored) ack, not a dropped line.
    /// The app did do nothing with the input, which is exactly what `Ignored`
    /// means, and the agent learns it in one round trip instead of a timeout.
    ///
    /// A sender never builds this deliberately, which is why it has no
    /// constructor and no fields: it exists only as something a reader
    /// produces.
    #[serde(other)]
    Unknown,
}

impl AgentInput {
    /// Build an [`Act`](Self::Act) input.
    ///
    /// `value` is the argument of an action that takes one, the text for
    /// [`Action::SetValue`] or the row for a select, and `None` for the
    /// actions that do not. It is one constructor taking an option rather than
    /// a pair of constructors because the only thing that builds an act is a
    /// bridge relaying what an agent asked for, and an optional argument is
    /// already what it holds: splitting this in two would make that caller
    /// match on the option to rebuild the same value. It also keeps every
    /// field of the variant reachable through one entry point, so a peer
    /// author has nothing to discover.
    pub fn act(node: NodeId, action: Action, value: Option<String>) -> Self {
        Self::Act {
            node,
            action,
            value,
        }
    }

    /// Build a [`Key`](Self::Key) input.
    ///
    /// Nothing here parses the string. A key that does not match the grammar
    /// in [`key`](crate::key) is worth rejecting before it costs a round trip,
    /// but that is the bridge's call at the point it accepts the key from an
    /// agent, not this constructor's.
    pub fn key(key: impl Into<String>) -> Self {
        Self::Key { key: key.into() }
    }

    /// Build a [`Text`](Self::Text) input.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action beside the exact JSON it must serialize to. Version 1 is
    /// frozen, so these strings are the format itself. A hand-written
    /// deserializer also makes this the only thing keeping `Action::from_wire`
    /// in step with `Serialize`.
    ///
    /// Built by walking an exhaustive `match`, so an action added later cannot
    /// be left out: its arm, naming its JSON and the action that follows it,
    /// has to be written before this compiles. Left out, it would serialize as
    /// its own name and deserialize as [`Action::Custom`] between two peers on
    /// the *same* version, silently, with nothing here failing.
    fn action_json() -> Vec<(Action, String)> {
        let mut table: Vec<(Action, String)> = Vec::new();
        let mut action = Some(Action::Activate);
        while let Some(current) = action {
            // A chain linked back on itself would push forever. Stopping
            // leaves the coverage check below to report it.
            if table.iter().any(|(seen, _)| *seen == current) {
                break;
            }
            let (json, next) = match &current {
                Action::Activate => (r#""activate""#.to_string(), Some(Action::Focus)),
                Action::Focus => (r#""focus""#.to_string(), Some(Action::Select)),
                Action::Select => (r#""select""#.to_string(), Some(Action::Toggle)),
                Action::Toggle => (r#""toggle""#.to_string(), Some(Action::Scroll)),
                Action::Scroll => (r#""scroll""#.to_string(), Some(Action::SetValue)),
                Action::SetValue => (r#""set_value""#.to_string(), Some(Action::Dismiss)),
                Action::Dismiss => (
                    r#""dismiss""#.to_string(),
                    Some(Action::Custom("archive".into())),
                ),
                // The only variant with a payload, so the only one whose JSON
                // is built from the value rather than fixed.
                Action::Custom(name) => (format!(r#"{{"custom":"{name}"}}"#), None),
            };
            table.push((current, json));
            action = next;
        }
        table
    }

    /// The compiler forces every action to have an arm; this forces the walk
    /// to reach every arm, so a new action linked in as a dead end cannot
    /// quietly cut the rest of the vocabulary out of the tests below.
    #[test]
    fn the_action_table_walks_the_whole_vocabulary() {
        let table = action_json();
        assert!(
            matches!(table.last(), Some((Action::Custom(_), _))),
            "the walk must end at the last action, not partway: {table:?}"
        );
    }

    #[test]
    fn every_action_serializes_to_its_frozen_json() {
        for (action, expected) in action_json() {
            assert_eq!(
                serde_json::to_string(&action).unwrap(),
                expected,
                "action {action:?}"
            );
        }
    }

    #[test]
    fn every_action_roundtrips() {
        for (action, json) in action_json() {
            let back: Action = serde_json::from_str(&json).unwrap();
            assert_eq!(back, action, "action {action:?} via {json}");
        }
    }

    #[test]
    fn unknown_action_name_becomes_custom() {
        // A built-in added in a later version arrives under its real name,
        // which is what an agent needs to advertise and echo it back.
        assert_eq!(
            serde_json::from_str::<Action>(r#""set_range""#).unwrap(),
            Action::Custom("set_range".into())
        );
        // Including the tag name itself, which was never a bare string.
        assert_eq!(
            serde_json::from_str::<Action>(r#""custom""#).unwrap(),
            Action::Custom("custom".into())
        );
    }

    #[test]
    fn unknown_action_object_keeps_its_name_and_drops_its_payload() {
        // A later variant may carry fields this build has nowhere to put.
        assert_eq!(
            serde_json::from_str::<Action>(r#"{"set_range":{"from":1,"to":9}}"#).unwrap(),
            Action::Custom("set_range".into())
        );
        // A built-in in its object form is what serde accepted before this
        // deserializer was written by hand, so it still parses.
        assert_eq!(
            serde_json::from_str::<Action>(r#"{"activate":null}"#).unwrap(),
            Action::Activate
        );
        // And a longer object must not leave the map half-read.
        assert_eq!(
            serde_json::from_str::<Action>(r#"{"custom":"archive","added_later":true}"#).unwrap(),
            Action::Custom("archive".into())
        );
    }

    #[test]
    fn unknown_action_degrades_inside_an_agent_input() {
        // `AgentInput` is internally tagged, so the action is deserialized
        // from a buffered value rather than straight off the reader. The
        // fallback has to survive that detour, or an app would still reject
        // the inputs a newer bridge sends it.
        let input: AgentInput =
            serde_json::from_str(r#"{"kind":"act","node":"btn","action":"set_range"}"#).unwrap();
        assert_eq!(
            input,
            AgentInput::Act {
                node: NodeId("btn".into()),
                action: Action::Custom("set_range".into()),
                value: None,
            }
        );
    }

    #[test]
    fn the_custom_envelope_folds_onto_every_built_in() {
        // Reading direction: the envelope form of a built-in name is that
        // built-in, not a `Custom` shadowing it. Driven off the frozen table
        // so a built-in added later is covered the moment it is listed there.
        for (action, json) in action_json() {
            // `Custom`'s own JSON is already an envelope, and the name it
            // carries is deliberately not a built-in.
            let Some(name) = json.strip_prefix('"').and_then(|j| j.strip_suffix('"')) else {
                continue;
            };
            let enveloped = format!(r#"{{"custom":"{name}"}}"#);
            assert_eq!(
                serde_json::from_str::<Action>(&enveloped).unwrap(),
                action,
                "{enveloped} must parse as {action:?}"
            );
        }
    }

    #[test]
    fn an_echoed_custom_reaches_the_built_in_it_names() {
        // Writing direction, and the whole reason for the fold. An older peer
        // that has never heard of `dismiss` reads it as `Custom`, keeps the
        // name, and sends it back in the only form it has. The peer that does
        // know the name has to see its own variant, or the act is accepted and
        // nothing happens.
        let echoed = Action::Custom("dismiss".into());
        let json = serde_json::to_string(&echoed).unwrap();
        assert_eq!(json, r#"{"custom":"dismiss"}"#);
        assert_eq!(
            serde_json::from_str::<Action>(&json).unwrap(),
            Action::Dismiss
        );

        // And through the buffered detour an internally tagged `AgentInput`
        // takes, which is how such an echo actually arrives.
        let input: AgentInput =
            serde_json::from_str(r#"{"kind":"act","node":"btn","action":{"custom":"dismiss"}}"#)
                .unwrap();
        assert_eq!(
            input,
            AgentInput::Act {
                node: NodeId("btn".into()),
                action: Action::Dismiss,
                value: None,
            }
        );
    }

    #[test]
    fn malformed_action_is_still_an_error() {
        // Degrading unknown names must not turn into accepting anything: a
        // shape no version of this format produces is a real parse failure.
        assert!(serde_json::from_str::<Action>("7").is_err());
        assert!(serde_json::from_str::<Action>(r#"{"custom":7}"#).is_err());
        assert!(serde_json::from_str::<Action>("{}").is_err());
    }

    #[test]
    fn every_agent_input_roundtrips() {
        let inputs = [
            AgentInput::Act {
                node: NodeId("input-1".into()),
                action: Action::SetValue,
                value: Some("hello".into()),
            },
            AgentInput::Act {
                node: NodeId("btn-1".into()),
                action: Action::Custom("archive".into()),
                value: None,
            },
            AgentInput::Key {
                key: "ctrl+c".into(),
            },
            AgentInput::Text {
                text: "buy milk".into(),
            },
        ];
        for input in inputs {
            let json = serde_json::to_string(&input).unwrap();
            let back: AgentInput = serde_json::from_str(&json).unwrap();
            assert_eq!(back, input, "input {input:?} via {json}");
        }
    }

    #[test]
    fn agent_input_uses_kind_tag() {
        let cases = [
            (
                AgentInput::Key { key: "q".into() },
                r#"{"kind":"key","key":"q"}"#,
            ),
            (
                AgentInput::Text { text: "hi".into() },
                r#"{"kind":"text","text":"hi"}"#,
            ),
            (
                AgentInput::Act {
                    node: NodeId("btn-1".into()),
                    action: Action::Activate,
                    value: None,
                },
                r#"{"kind":"act","node":"btn-1","action":"activate"}"#,
            ),
        ];
        for (input, expected) in cases {
            let json = serde_json::to_string(&input).unwrap();
            assert_eq!(json, expected, "input: {input:?}");
        }
    }

    #[test]
    fn the_constructors_reach_every_field() {
        // The variants are `#[non_exhaustive]`, so outside this crate these
        // are the only way to build one. Each must reach every field.
        assert_eq!(
            AgentInput::act(
                NodeId("input-1".into()),
                Action::SetValue,
                Some("hello".into())
            ),
            AgentInput::Act {
                node: NodeId("input-1".into()),
                action: Action::SetValue,
                value: Some("hello".into()),
            }
        );
        assert_eq!(
            AgentInput::act(NodeId("btn-1".into()), Action::Activate, None),
            AgentInput::Act {
                node: NodeId("btn-1".into()),
                action: Action::Activate,
                value: None,
            }
        );
        assert_eq!(
            AgentInput::key("ctrl+c"),
            AgentInput::Key {
                key: "ctrl+c".into()
            }
        );
        assert_eq!(
            AgentInput::text("buy milk"),
            AgentInput::Text {
                text: "buy milk".into()
            }
        );
    }

    #[test]
    fn an_unrecognized_kind_reads_as_unknown() {
        // Every shape a later version 1 release might send under a kind this
        // build has never heard of, including one carrying nested objects.
        for line in [
            r#"{"kind":"paste","text":"hello"}"#,
            r#"{"kind":"pointer","at":{"x":3,"y":9},"button":"left"}"#,
            r#"{"kind":"unknown"}"#,
        ] {
            assert_eq!(
                serde_json::from_str::<AgentInput>(line).unwrap(),
                AgentInput::Unknown,
                "{line}"
            );
        }

        // Its own spelling, which is all a round trip through this build can
        // produce: the kind it stood for is not recoverable.
        assert_eq!(
            serde_json::to_string(&AgentInput::Unknown).unwrap(),
            r#"{"kind":"unknown"}"#
        );
    }

    #[test]
    fn a_malformed_input_is_still_an_error() {
        // The fallback must not turn into accepting anything: an input with no
        // kind at all, or a known kind missing the field that kind is made of,
        // is a real parse failure and not something to ack.
        assert!(serde_json::from_str::<AgentInput>(r#"{"text":"hello"}"#).is_err());
        assert!(serde_json::from_str::<AgentInput>(r#"{"kind":"key"}"#).is_err());
        assert!(serde_json::from_str::<AgentInput>("7").is_err());
    }
}
