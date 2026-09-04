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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
