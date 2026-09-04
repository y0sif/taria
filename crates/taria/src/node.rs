use serde::{Deserialize, Serialize};

/// Stable identifier for a node within one app run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Semantic role of a widget, the TUI analogue of an ARIA role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    App,
    Pane,
    List,
    ListItem,
    Table,
    Row,
    Cell,
    TextInput,
    Button,
    Checkbox,
    Tabs,
    Tab,
    Text,
    ProgressBar,
    Dialog,
    Menu,
    MenuItem,
    Other,
}

/// One widget in the semantic tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub role: Role,
    /// Human-readable label (list title, button text, input placeholder).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Current value (input contents, selected item, checkbox state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub focused: bool,
    /// Actions an agent may invoke on this node right now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<crate::Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}
