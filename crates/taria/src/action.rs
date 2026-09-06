use serde::{Deserialize, Serialize};

use crate::NodeId;

/// An action a node advertises as currently available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    Key { key: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_roundtrips() {
        let actions = [
            Action::Activate,
            Action::Focus,
            Action::Select,
            Action::Toggle,
            Action::Scroll,
            Action::SetValue,
            Action::Dismiss,
            Action::Custom("archive_task".into()),
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let back: Action = serde_json::from_str(&json).unwrap();
            assert_eq!(back, action, "action {action:?} via {json}");
        }
    }

    #[test]
    fn custom_action_serializes_as_tagged_object() {
        let json = serde_json::to_string(&Action::Custom("archive".into())).unwrap();
        assert_eq!(json, r#"{"custom":"archive"}"#);
        assert_eq!(
            serde_json::to_string(&Action::SetValue).unwrap(),
            r#""set_value""#
        );
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
        ];
        for input in inputs {
            let json = serde_json::to_string(&input).unwrap();
            let back: AgentInput = serde_json::from_str(&json).unwrap();
            assert_eq!(back, input, "input {input:?} via {json}");
        }
    }

    #[test]
    fn agent_input_uses_kind_tag() {
        let json = serde_json::to_string(&AgentInput::Key { key: "q".into() }).unwrap();
        assert_eq!(json, r#"{"kind":"key","key":"q"}"#);
    }
}
