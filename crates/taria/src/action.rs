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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
                    Action::Custom(map.next_value()?)
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AgentInput {
    /// Invoke an advertised action on a node.
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
    Key { key: String },
    /// Literal text to type, lowered by the adapter into one key event per
    /// character. One message instead of one round trip per character.
    Text { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action beside the exact JSON it must serialize to. Version 1 is
    /// frozen, so these strings are the format itself. A hand-written
    /// deserializer also makes this the only thing keeping `Action::from_wire`
    /// in step with `Serialize`.
    fn action_json() -> Vec<(Action, &'static str)> {
        vec![
            (Action::Activate, r#""activate""#),
            (Action::Focus, r#""focus""#),
            (Action::Select, r#""select""#),
            (Action::Toggle, r#""toggle""#),
            (Action::Scroll, r#""scroll""#),
            (Action::SetValue, r#""set_value""#),
            (Action::Dismiss, r#""dismiss""#),
            (Action::Custom("archive".into()), r#"{"custom":"archive"}"#),
        ]
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
            let back: Action = serde_json::from_str(json).unwrap();
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
}
