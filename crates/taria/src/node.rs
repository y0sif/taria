use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Action;

/// Stable identifier for a node within one app run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

/// Semantic role of a widget, the TUI analogue of an ARIA role.
///
/// A role name the reader does not know deserializes to [`Other`](Self::Other)
/// rather than failing. The alternative is worse than it looks: a role is
/// nested inside a [`Node`], so rejecting it rejects the whole
/// [`Snapshot`](crate::Snapshot) that carries it, and a peer that skips
/// unparseable lines then goes on serving its last tree with no error anywhere.
/// Degrading one node's role is what lets an app that learned a new role keep
/// talking to an agent built before it.
///
/// `#[non_exhaustive]` says the same thing to the compiler that the fallback
/// says to the parser: this vocabulary is expected to keep growing. A role
/// added later is additive on the wire, and the attribute is what makes it
/// additive in Rust too, so an adapter that matches on roles keeps compiling
/// across a taria upgrade instead of breaking once per new widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Role {
    App,
    Pane,
    List,
    ListItem,
    /// Hierarchical collection: a file browser, a repository's working tree, a
    /// database schema sidebar. Choose this over [`List`](Self::List) when an
    /// entry can own entries of its own, and [`List`](Self::List) when the
    /// rows are flat. Naming a flat list a tree sends an agent looking for
    /// structure to expand that is not there; naming a tree a list hides the
    /// nesting that decides what an agent has actually seen.
    Tree,
    /// One entry in a [`Tree`](Self::Tree), with its own entries as children,
    /// so the shape of the node tree is the shape of the widget's.
    TreeItem,
    Table,
    Row,
    Cell,
    TextInput,
    Button,
    Checkbox,
    /// A control that holds one choice out of a fixed set: a dropdown, a radio
    /// group, a settings picker. Choose this over [`List`](Self::List) when
    /// the point is to commit to a value rather than to browse rows, and put
    /// the committed choice in the node's value, so an agent can read the
    /// current setting without walking the children. A list reports where a
    /// cursor sits; a select reports what the app will use.
    Select,
    /// One choice inside a [`Select`](Self::Select). Distinct from
    /// [`ListItem`](Self::ListItem) because activating it sets the parent's
    /// value rather than moving a cursor within it.
    Option,
    Tabs,
    Tab,
    /// A reference to somewhere else: an OSC 8 terminal hyperlink, or a path,
    /// URL or issue number the app opens when it is activated. Put the
    /// destination in the node's value, so an agent can read where it leads
    /// without following it.
    Link,
    Text,
    /// Append-only stream of lines: command output, a journal tail, a model's
    /// streaming response. Distinct from [`Text`](Self::Text) because the
    /// content grows at the end, which tells an agent the value it read is a
    /// prefix of what is there now rather than the whole of it.
    Log,
    /// An embedded terminal emulator, a pty another program is drawing into.
    /// Its contents are a screen rather than a semantic tree, so an agent
    /// should read the value as opaque text and drive it with keys.
    Terminal,
    /// A picture rendered into cells, whether through a terminal image
    /// protocol or as block characters. An agent cannot see it, so the label
    /// is all it gets: say what the image is of, not that it is an image.
    Image,
    /// A data visualization: sparkline, bar chart, histogram, time series. The
    /// rendering is unreadable to an agent, so put the numbers that carry the
    /// meaning (the latest sample, the peak, the unit) in the node's value.
    Chart,
    ProgressBar,
    /// A transient notice: a spinner, a throbber, a toast, a "saving" line
    /// that appears and clears itself. Distinct from
    /// [`ProgressBar`](Self::ProgressBar), which reports a known fraction of a
    /// known total; a status says work is happening without saying how much of
    /// it is left, and is the right role when there is no fraction to report.
    /// Publishing it is what makes it observable at all: a toast that appears
    /// and vanishes between two reads is invisible to an agent, which then
    /// reads the app as having done nothing.
    Status,
    /// The scroll position of a scrollable region, and how much of that region
    /// is on screen. Worth publishing rather than dropping as decoration: it
    /// is the only thing telling an agent that the pane it just read has more
    /// content past the edge. Put the position in the node's value.
    Scrollbar,
    Dialog,
    Menu,
    MenuItem,
    Other,
}

impl Role {
    /// Map a wire role name onto a variant, degrading an unrecognized name to
    /// [`Other`](Self::Other).
    ///
    /// The names must stay in step with what the derived [`Serialize`] emits;
    /// the `every_role_roundtrips` test is what enforces that.
    fn from_wire(name: &str) -> Self {
        match name {
            "app" => Role::App,
            "pane" => Role::Pane,
            "list" => Role::List,
            "list_item" => Role::ListItem,
            "tree" => Role::Tree,
            "tree_item" => Role::TreeItem,
            "table" => Role::Table,
            "row" => Role::Row,
            "cell" => Role::Cell,
            "text_input" => Role::TextInput,
            "button" => Role::Button,
            "checkbox" => Role::Checkbox,
            "select" => Role::Select,
            "option" => Role::Option,
            "tabs" => Role::Tabs,
            "tab" => Role::Tab,
            "link" => Role::Link,
            "text" => Role::Text,
            "log" => Role::Log,
            "terminal" => Role::Terminal,
            "image" => Role::Image,
            "chart" => Role::Chart,
            "progress_bar" => Role::ProgressBar,
            "status" => Role::Status,
            "scrollbar" => Role::Scrollbar,
            "dialog" => Role::Dialog,
            "menu" => Role::Menu,
            "menu_item" => Role::MenuItem,
            _ => Role::Other,
        }
    }
}

/// Hand-written so an unknown role name becomes [`Role::Other`] instead of an
/// error. `#[serde(other)]` cannot express this: it is only available on
/// internally and adjacently tagged enums, and this one is a plain string.
impl<'de> Deserialize<'de> for Role {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RoleVisitor;

        impl<'de> Visitor<'de> for RoleVisitor {
            type Value = Role;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a role name")
            }

            fn visit_str<E>(self, name: &str) -> Result<Role, E>
            where
                E: de::Error,
            {
                Ok(Role::from_wire(name))
            }

            /// A role is always a bare string on the wire, but serde's derived
            /// deserializer also accepted `{"app":null}`, so this one does too
            /// rather than narrow what a peer may send.
            fn visit_map<A>(self, mut map: A) -> Result<Role, A::Error>
            where
                A: MapAccess<'de>,
            {
                let Some(name) = map.next_key::<String>()? else {
                    return Err(de::Error::invalid_length(0, &self));
                };
                map.next_value::<IgnoredAny>()?;
                // Drain the rest: serde_json rejects a map the visitor left
                // half-read.
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Role::from_wire(&name))
            }
        }

        deserializer.deserialize_any(RoleVisitor)
    }
}

/// One widget in the semantic tree.
///
/// `#[non_exhaustive]` because a node is where new optional fields land, and
/// an optional field is the cheapest additive change the format has. Nothing
/// outside this crate loses anything to it: [`new`](Self::new) plus the
/// chainable setters below already reach every field, so a struct literal was
/// never the way to build one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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

    /// Every role beside the exact JSON it must serialize to. Version 1 is
    /// frozen, so these strings are the format itself, not a detail of how it
    /// happens to be derived today. A hand-written deserializer also makes
    /// this the only thing keeping `Role::from_wire` in step with `Serialize`.
    ///
    /// Built by walking an exhaustive `match`, so a role added later cannot be
    /// left out: its arm, naming its JSON and the role that follows it, has to
    /// be written before this compiles. Left out, it would serialize as its own
    /// name and deserialize as [`Role::Other`] between two peers on the *same*
    /// version, silently, with nothing here failing.
    fn role_json() -> Vec<(Role, &'static str)> {
        let mut table: Vec<(Role, &'static str)> = Vec::new();
        let mut role = Some(Role::App);
        while let Some(current) = role {
            // A chain linked back on itself would push forever. Stopping
            // leaves the coverage check below to report it.
            if table.iter().any(|(seen, _)| *seen == current) {
                break;
            }
            let (json, next) = match current {
                Role::App => (r#""app""#, Some(Role::Pane)),
                Role::Pane => (r#""pane""#, Some(Role::List)),
                Role::List => (r#""list""#, Some(Role::ListItem)),
                Role::ListItem => (r#""list_item""#, Some(Role::Tree)),
                Role::Tree => (r#""tree""#, Some(Role::TreeItem)),
                Role::TreeItem => (r#""tree_item""#, Some(Role::Table)),
                Role::Table => (r#""table""#, Some(Role::Row)),
                Role::Row => (r#""row""#, Some(Role::Cell)),
                Role::Cell => (r#""cell""#, Some(Role::TextInput)),
                Role::TextInput => (r#""text_input""#, Some(Role::Button)),
                Role::Button => (r#""button""#, Some(Role::Checkbox)),
                Role::Checkbox => (r#""checkbox""#, Some(Role::Select)),
                Role::Select => (r#""select""#, Some(Role::Option)),
                Role::Option => (r#""option""#, Some(Role::Tabs)),
                Role::Tabs => (r#""tabs""#, Some(Role::Tab)),
                Role::Tab => (r#""tab""#, Some(Role::Link)),
                Role::Link => (r#""link""#, Some(Role::Text)),
                Role::Text => (r#""text""#, Some(Role::Log)),
                Role::Log => (r#""log""#, Some(Role::Terminal)),
                Role::Terminal => (r#""terminal""#, Some(Role::Image)),
                Role::Image => (r#""image""#, Some(Role::Chart)),
                Role::Chart => (r#""chart""#, Some(Role::ProgressBar)),
                Role::ProgressBar => (r#""progress_bar""#, Some(Role::Status)),
                Role::Status => (r#""status""#, Some(Role::Scrollbar)),
                Role::Scrollbar => (r#""scrollbar""#, Some(Role::Dialog)),
                Role::Dialog => (r#""dialog""#, Some(Role::Menu)),
                Role::Menu => (r#""menu""#, Some(Role::MenuItem)),
                Role::MenuItem => (r#""menu_item""#, Some(Role::Other)),
                Role::Other => (r#""other""#, None),
            };
            table.push((current, json));
            role = next;
        }
        table
    }

    /// The compiler forces every role to have an arm; this forces the walk to
    /// reach every arm, so a new role linked in as a dead end cannot quietly
    /// cut the rest of the vocabulary out of the tests below.
    #[test]
    fn the_role_table_walks_the_whole_vocabulary() {
        let table = role_json();
        assert_eq!(
            table.last().map(|(role, _)| *role),
            Some(Role::Other),
            "the walk must end at the last role, not partway: {table:?}"
        );
    }

    #[test]
    fn every_role_serializes_to_its_frozen_json() {
        for (role, expected) in role_json() {
            assert_eq!(
                serde_json::to_string(&role).unwrap(),
                expected,
                "role {role:?}"
            );
        }
    }

    #[test]
    fn every_role_roundtrips() {
        for (role, json) in role_json() {
            let back: Role = serde_json::from_str(json).unwrap();
            assert_eq!(back, role, "role {role:?} via {json}");
        }
    }

    #[test]
    fn unknown_role_degrades_to_other() {
        // A role added in a later version. It must cost this node its role,
        // and nothing else.
        let node: Node =
            serde_json::from_str(r#"{"id":"n","role":"sparkline","focused":false,"label":"cpu"}"#)
                .unwrap();
        assert_eq!(node.role, Role::Other);
        assert_eq!(node.label.as_deref(), Some("cpu"));
    }

    #[test]
    fn role_object_form_still_parses() {
        // What serde's derived deserializer accepted beside the bare string,
        // kept so the hand-written one narrows nothing.
        assert_eq!(
            serde_json::from_str::<Role>(r#"{"button":null}"#).unwrap(),
            Role::Button
        );
        assert_eq!(
            serde_json::from_str::<Role>(r#"{"sparkline":null}"#).unwrap(),
            Role::Other
        );
    }

    #[test]
    fn malformed_role_is_still_an_error() {
        // Degrading unknown names must not turn into accepting anything.
        assert!(serde_json::from_str::<Role>("7").is_err());
        assert!(serde_json::from_str::<Role>("{}").is_err());
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
