use serde::{Deserialize, Serialize};

use crate::Action;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

impl Node {
    /// Create a node with the given id and role; every other field starts
    /// empty, ready for the chainable builder methods below.
    pub fn new(id: impl Into<String>, role: Role) -> Self {
        Self {
            id: NodeId(id.into()),
            role,
            label: None,
            value: None,
            focused: false,
            actions: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Set the human-readable label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the current value.
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// Set whether this node currently has input focus.
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Advertise one action as currently available.
    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }

    /// Advertise several actions as currently available.
    pub fn actions(mut self, actions: impl IntoIterator<Item = Action>) -> Self {
        self.actions.extend(actions);
        self
    }

    /// Append a child node.
    pub fn child(mut self, child: Node) -> Self {
        self.children.push(child);
        self
    }

    /// Append several child nodes.
    pub fn children(mut self, children: impl IntoIterator<Item = Node>) -> Self {
        self.children.extend(children);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ROLES: [Role; 18] = [
        Role::App,
        Role::Pane,
        Role::List,
        Role::ListItem,
        Role::Table,
        Role::Row,
        Role::Cell,
        Role::TextInput,
        Role::Button,
        Role::Checkbox,
        Role::Tabs,
        Role::Tab,
        Role::Text,
        Role::ProgressBar,
        Role::Dialog,
        Role::Menu,
        Role::MenuItem,
        Role::Other,
    ];

    #[test]
    fn every_role_roundtrips() {
        for role in ALL_ROLES {
            let json = serde_json::to_string(&role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(back, role, "role {role:?} via {json}");
        }
    }

    #[test]
    fn roles_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&Role::ListItem).unwrap(),
            r#""list_item""#
        );
        assert_eq!(
            serde_json::to_string(&Role::ProgressBar).unwrap(),
            r#""progress_bar""#
        );
    }

    #[test]
    fn builder_fills_all_fields() {
        let node = Node::new("list", Role::List)
            .label("Tasks")
            .value("2 of 5 done")
            .focused(true)
            .action(Action::Select)
            .actions([Action::Scroll, Action::Custom("archive".into())])
            .child(Node::new("item-1", Role::ListItem).label("Buy milk"))
            .children([
                Node::new("item-2", Role::ListItem),
                Node::new("item-3", Role::ListItem),
            ]);

        assert_eq!(node.id, NodeId("list".into()));
        assert_eq!(node.role, Role::List);
        assert_eq!(node.label.as_deref(), Some("Tasks"));
        assert_eq!(node.value.as_deref(), Some("2 of 5 done"));
        assert!(node.focused);
        assert_eq!(
            node.actions,
            vec![
                Action::Select,
                Action::Scroll,
                Action::Custom("archive".into())
            ]
        );
        assert_eq!(node.children.len(), 3);
        assert_eq!(node.children[0].label.as_deref(), Some("Buy milk"));
        assert!(!node.children[1].focused);
    }

    #[test]
    fn nested_node_roundtrips() {
        let node = Node::new("root", Role::App).child(
            Node::new("pane", Role::Pane).child(Node::new("input", Role::TextInput).focused(true)),
        );
        let json = serde_json::to_string(&node).unwrap();
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back, node);
    }

    #[test]
    fn empty_optional_fields_are_omitted() {
        let json = serde_json::to_string(&Node::new("n", Role::Text)).unwrap();
        assert!(!json.contains("label"), "json: {json}");
        assert!(!json.contains("value"), "json: {json}");
        assert!(!json.contains("actions"), "json: {json}");
        assert!(!json.contains("children"), "json: {json}");
    }
}
