use std::error::Error;
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

/// Deepest node tree a peer is expected to parse, counting the snapshot root
/// as level 1.
///
/// A [`Snapshot`](crate::Snapshot) travels as one JSON object, and JSON
/// parsers bound how far they will recurse into one. `serde_json`, which both
/// reference peers use, stops at 128 nested values, and every [`Node`] costs
/// two of them: its own object and its `children` array. Measured through a
/// whole `{"type":"snapshot",...}` line rather than a bare node, 63 nested
/// nodes parse and 64 fail.
///
/// Nothing reports crossing that ceiling. The reader skips the line it cannot
/// parse, exactly as it skips a truncated one, so a deep first snapshot leaves
/// a bridge saying it has no tree yet while the app is connected and healthy,
/// and a deep later snapshot leaves it serving the last shallow tree with
/// nothing marking it stale. Writing is worse than reading: serialization
/// recurses per level too, and a tree thousands of levels deep exhausts the
/// stack and aborts the process from inside a library thread.
///
/// This limit is a little under half the measured ceiling. The slack pays for
/// the envelope a transport wraps around a snapshot, for a root an adapter
/// adds above the nodes an app hands it, and for a peer whose parser is
/// stricter than `serde_json`. It is still far past any hand-built widget
/// tree: an app, its tabs, a pane, a list and its rows is six levels.
///
/// The trees that reach it are generated from data rather than written out,
/// and [`Role::Tree`] invites the obvious one, a browser over a deep
/// directory. Such an app should publish the expanded path instead of the
/// whole structure, which is what a tree widget draws anyway: the rows the
/// user can currently see. That makes the snapshot the size of the screen
/// rather than the size of the data behind it.
///
/// This crate states the limit and offers [`Node::check_depth`]. It enforces
/// nothing, because what to do about a tree that is too deep, truncate it,
/// skip the publish, or tell the app, is the adapter's to decide.
pub const MAX_NODE_DEPTH: usize = 32;

/// A node tree deeper than [`MAX_NODE_DEPTH`].
///
/// Keeps the depth measured and the id of a node found at it, because an app
/// that built the tree out of data has no other way to tell which branch ran
/// away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeTooDeep {
    depth: usize,
    deepest: NodeId,
}

impl TreeTooDeep {
    /// Depth measured, counting the root as level 1.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Id of a node at that depth. Several nodes can share the deepest level;
    /// this is whichever the walk reached first, which is enough to find the
    /// branch.
    pub fn deepest(&self) -> &NodeId {
        &self.deepest
    }
}

impl fmt::Display for TreeTooDeep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "node tree is {} levels deep at node `{}`, over the {MAX_NODE_DEPTH} level limit for a taria snapshot",
            self.depth, self.deepest.0
        )
    }
}

impl Error for TreeTooDeep {}

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
    /// Whether a raw key would land on this node.
    ///
    /// `#[serde(default)]` and still always serialized, so nothing on the
    /// wire moves today. What it buys is the other direction: a peer that
    /// omits the field is accepted, which an older peer cannot tell from one
    /// that sent `false`, so dropping it later stays an additive change
    /// rather than a version bump. At most one node of a tree is focused, so
    /// the field is `false` on every other node and costs a 500-node tree
    /// around 8 KB of `"focused":false` per publish, against an agent's
    /// output budget. Version 1 is frozen, which is why accepting absence has
    /// to land now: relaxing what is accepted is only free while no peer yet
    /// relies on it.
    #[serde(default)]
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

    /// Depth of the tree rooted here, counting this node as level 1.
    ///
    /// Walks with a vector rather than by recursing, so the one input it has
    /// to survive, a tree deeper than the call stack, is exactly the one it
    /// was written for. Its memory is the tree's width, not its depth.
    pub fn depth(&self) -> usize {
        self.deepest().0
    }

    /// Check this tree against [`MAX_NODE_DEPTH`] before publishing it.
    ///
    /// The failure it catches is silent at the peer, so an adapter that
    /// publishes a generated tree should call this and report the error to
    /// the app rather than letting the snapshot go out and disappear.
    pub fn check_depth(&self) -> Result<(), TreeTooDeep> {
        let (depth, deepest) = self.deepest();
        if depth > MAX_NODE_DEPTH {
            return Err(TreeTooDeep {
                depth,
                deepest: deepest.clone(),
            });
        }
        Ok(())
    }

    /// Depth of this tree paired with the id of a node found at it, measured
    /// in one iterative walk so both callers above pay for only one.
    fn deepest(&self) -> (usize, &NodeId) {
        let mut deepest = (1, &self.id);
        let mut pending = vec![(self, 1usize)];
        while let Some((node, level)) = pending.pop() {
            if level > deepest.0 {
                deepest = (level, &node.id);
            }
            for child in &node.children {
                pending.push((child, level + 1));
            }
        }
        deepest
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

    /// Absence reads as `false`, and the field is still written. The pairing
    /// is the point: an omitted `focused` becomes possible for a later peer
    /// without a version bump, and no peer sees the wire change today.
    #[test]
    fn focused_may_be_absent_and_is_still_serialized() {
        let node: Node = serde_json::from_str(r#"{"id":"n","role":"text"}"#).unwrap();
        assert!(!node.focused);

        let json = serde_json::to_string(&Node::new("n", Role::Text)).unwrap();
        assert!(
            json.contains(r#""focused":false"#),
            "the field still goes on the wire: {json}"
        );
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

    /// A chain of `depth` nodes, one child each: the root is `n1` and the
    /// deepest node is `n{depth}`.
    fn chain(depth: usize) -> Node {
        let mut node = Node::new(format!("n{depth}"), Role::TreeItem);
        for level in (1..depth).rev() {
            node = Node::new(format!("n{level}"), Role::TreeItem).child(node);
        }
        node
    }

    /// Drop a tree without recursing, so a test may build one deeper than the
    /// call stack. `Node`'s own `Drop` walks the children recursively, which
    /// is the second half of why deep trees are a hazard rather than merely a
    /// parsing limit.
    fn drop_iteratively(root: Node) {
        let mut pending = vec![root];
        while let Some(mut node) = pending.pop() {
            pending.append(&mut node.children);
        }
    }

    #[test]
    fn depth_counts_the_root_and_follows_the_longest_branch() {
        assert_eq!(Node::new("leaf", Role::Text).depth(), 1);
        assert_eq!(chain(9).depth(), 9);

        // Two branches of different lengths: the longer one is the depth.
        let root = Node::new("root", Role::App)
            .child(Node::new("shallow", Role::Text))
            .child(chain(3));
        assert_eq!(root.depth(), 4);
    }

    #[test]
    fn check_depth_passes_at_the_limit_and_names_the_node_past_it() {
        assert!(chain(MAX_NODE_DEPTH).check_depth().is_ok());

        let err = chain(MAX_NODE_DEPTH + 1).check_depth().unwrap_err();
        assert_eq!(err.depth(), MAX_NODE_DEPTH + 1);
        assert_eq!(err.deepest(), &NodeId(format!("n{}", MAX_NODE_DEPTH + 1)));

        let message = err.to_string();
        assert!(message.contains(&MAX_NODE_DEPTH.to_string()), "{message}");
        assert!(
            message.contains(&format!("n{}", MAX_NODE_DEPTH + 1)),
            "{message}"
        );
    }

    #[test]
    fn a_tree_deeper_than_the_call_stack_is_still_measurable() {
        // The checker guards against trees a recursive walk cannot survive, so
        // it must survive one itself. A recursive `depth` overflows a test
        // thread's stack well before this.
        let deep = chain(100_000);
        assert_eq!(deep.depth(), 100_000);
        assert!(deep.check_depth().is_err());
        drop_iteratively(deep);
    }

    /// Where [`MAX_NODE_DEPTH`] comes from. `serde_json` stops at 128 nested
    /// values and each node costs two, so through a whole snapshot message 63
    /// nested nodes parse and 64 fail. This asserts headroom rather than that
    /// exact number: a `serde_json` that raises its limit must not fail the
    /// build, and one that lowers it under the constant must.
    #[test]
    fn the_depth_limit_has_headroom_under_the_parser() {
        use crate::Snapshot;
        use crate::wire::AppToBridge;

        let parses = |depth: usize| {
            let msg = AppToBridge::Snapshot(Snapshot::new(1, chain(depth)));
            let line = serde_json::to_string(&msg).unwrap();
            serde_json::from_str::<AppToBridge>(&line).is_ok()
        };

        assert!(parses(MAX_NODE_DEPTH), "a tree at the limit must parse");

        if let Some(ceiling) = (MAX_NODE_DEPTH..=256).find(|&depth| !parses(depth)) {
            assert!(
                ceiling >= MAX_NODE_DEPTH + 16,
                "only {} levels between the limit and the parser, which stops at {ceiling}",
                ceiling - MAX_NODE_DEPTH
            );
        }
    }
}
