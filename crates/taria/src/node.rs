use std::error::Error;
use std::fmt;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Action;
use crate::state::{self, Checked, Number};

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
    /// The application root.
    App,
    /// A region of the screen.
    Pane,
    /// A flat collection of rows; it reports where a cursor sits.
    List,
    /// One row of a [`List`](Self::List).
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
    /// A table of [`Row`](Self::Row)s.
    Table,
    /// One row of a [`Table`](Self::Table).
    Row,
    /// One cell of a [`Row`](Self::Row).
    Cell,
    /// A field the app types into.
    TextInput,
    /// A control whose primary action is to be pressed.
    Button,
    /// A two-state control, whose state goes in [`Node::checked`].
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
    /// A tab bar holding [`Tab`](Self::Tab) children.
    Tabs,
    /// One tab in a [`Tabs`](Self::Tabs) bar.
    Tab,
    /// A reference to somewhere else: an OSC 8 terminal hyperlink, or a path,
    /// URL or issue number the app opens when it is activated. Put the
    /// destination in the node's value, so an agent can read where it leads
    /// without following it.
    Link,
    /// Static text.
    Text,
    /// Append-only stream of lines: command output, a journal tail, a model's
    /// streaming response. Distinct from [`Text`](Self::Text) because the
    /// content grows at the end, which tells an agent the value it read is a
    /// prefix of what is there now rather than the whole of it. A stream the
    /// app does not write itself, a command's or a model's, is relayed text:
    /// mark it with [`Node::relayed`], as "Mark relayed text" in
    /// `docs/integration-guide.md` describes.
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
    /// Progress as a known fraction of a known total, stated with
    /// [`Node::range`].
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
    /// A dialog, typically modal.
    Dialog,
    /// A menu holding [`MenuItem`](Self::MenuItem) children.
    Menu,
    /// One entry in a [`Menu`](Self::Menu).
    MenuItem,
    /// Nothing in the vocabulary fits, or the reader does not know the role
    /// name that was sent.
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
/// `#[non_exhaustive]` because a node is where new optional fields land, as
/// `relayed` did in 0.3.3 and the typed state, `description` and `selected`
/// through `column_count`, did in 0.4.0, and an optional field is the
/// cheapest additive change the format has. Nothing outside this crate loses
/// anything to it: [`new`](Self::new) plus the chainable setters below
/// already reach every field, so a struct literal was never the way to build
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Node {
    /// Identifier, unique within one snapshot and stable across snapshots
    /// for as long as it names the same thing.
    pub id: NodeId,
    /// What the widget is.
    pub role: Role,
    /// Human-readable label (list title, button text, input placeholder).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Current value (input contents, a list's selected item, a select's
    /// choice). A checkbox's state goes in [`checked`](Self::checked), and a
    /// number in [`value_now`](Self::value_now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Longer text than the label, in the app's own words: a hint, what a
    /// setting does, why a button is disabled.
    ///
    /// Never relayed text: [`relayed`](Self::relayed) covers the label and
    /// the value only, so text from outside the app goes in a child marked
    /// relayed, never here.
    #[serde(
        default,
        deserialize_with = "state::lenient_text",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
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
    /// Whether this node's label and value come from outside the app (model
    /// output, file contents, another user's message): data, never
    /// instructions, and never the app's own words.
    ///
    /// It covers the label and the value, never the id, and this node alone:
    /// children do not inherit it. The reference bridge hands on the line
    /// with its keys sorted, so an inherited mark would arrive after the
    /// node's whole subtree, not just its own label. Omitted when `false`,
    /// unlike `focused`, because most nodes carry no relayed text. Absence
    /// reads as `false`, which says only that the node is not marked, not
    /// that the app wrote it, and anything but a boolean, `null` included,
    /// fails the line as a malformed `focused` does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relayed: bool,
    /// Whether this node is selected among its siblings: the row a list's
    /// cursor sits on, the active tab, the chosen option, or each of the rows
    /// picked in a list that allows more than one. Selection, not focus: a
    /// cursor sits on a row whether or not the list holds the keyboard.
    ///
    /// This field and every one after it, up to `actions`, came in 0.4.0. Each
    /// is omitted when unset, absence means not stated, never `false` (but for
    /// [`disabled`](Self::disabled), where the two say the same), and a value
    /// of the wrong type or shape reads as not stated rather than failing the
    /// node.
    #[serde(
        default,
        deserialize_with = "state::lenient_bool",
        skip_serializing_if = "Option::is_none"
    )]
    pub selected: Option<bool>,
    /// Whether a checkable node is checked.
    #[serde(
        default,
        deserialize_with = "state::lenient_checked",
        skip_serializing_if = "Option::is_none"
    )]
    pub checked: Option<Checked>,
    /// Whether an expandable node is expanded: `Some(false)` is collapsed.
    /// `None` says nothing; an app that states expansion leaves it unset
    /// only where a node cannot expand.
    #[serde(
        default,
        deserialize_with = "state::lenient_bool",
        skip_serializing_if = "Option::is_none"
    )]
    pub expanded: Option<bool>,
    /// Whether the node is disabled: shown, but accepting no action until the
    /// app enables it. Only `true` is a statement, so only `true` is ever
    /// sent, and absence, `false`, `null` and a value of any other shape all
    /// read as `false`, which says the node is not marked, without failing
    /// the node.
    ///
    /// It covers this node alone, as [`relayed`](Self::relayed) does:
    /// children do not inherit it. The reference adapter strips every action
    /// from a disabled node when it publishes, and the reference bridge
    /// refuses `act` on one. An empty action list alone never means disabled.
    #[serde(
        default,
        deserialize_with = "state::lenient_flag",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub disabled: bool,
    /// A numeric value: a progress bar's, a slider's, a gauge's.
    #[serde(
        default,
        deserialize_with = "state::lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub value_now: Option<Number>,
    /// The least [`value_now`](Self::value_now) can be.
    #[serde(
        default,
        deserialize_with = "state::lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub value_min: Option<Number>,
    /// The most [`value_now`](Self::value_now) can be.
    #[serde(
        default,
        deserialize_with = "state::lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub value_max: Option<Number>,
    /// This item's position in its set, counting from 1, published or not:
    /// row 40 of 1000 is 40 even when only rows 31 to 60 are published.
    #[serde(
        default,
        deserialize_with = "state::lenient_position",
        skip_serializing_if = "Option::is_none"
    )]
    pub pos_in_set: Option<u32>,
    /// How many items this container holds, published or not, on a list, a
    /// tree, a tree item, tabs, a select or a menu.
    #[serde(
        default,
        deserialize_with = "state::lenient_count",
        skip_serializing_if = "Option::is_none"
    )]
    pub set_size: Option<u32>,
    /// The row a table row or cell is in, counting from 1.
    #[serde(
        default,
        deserialize_with = "state::lenient_position",
        skip_serializing_if = "Option::is_none"
    )]
    pub row: Option<u32>,
    /// The column a table cell is in, counting from 1.
    #[serde(
        default,
        deserialize_with = "state::lenient_position",
        skip_serializing_if = "Option::is_none"
    )]
    pub column: Option<u32>,
    /// How many rows a table holds, published or not.
    #[serde(
        default,
        deserialize_with = "state::lenient_count",
        skip_serializing_if = "Option::is_none"
    )]
    pub row_count: Option<u32>,
    /// How many columns a table holds, published or not.
    #[serde(
        default,
        deserialize_with = "state::lenient_count",
        skip_serializing_if = "Option::is_none"
    )]
    pub column_count: Option<u32>,
    /// Actions an agent may invoke on this node right now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// Child nodes, in order.
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
            description: None,
            focused: false,
            relayed: false,
            selected: None,
            checked: None,
            expanded: None,
            disabled: false,
            value_now: None,
            value_min: None,
            value_max: None,
            pos_in_set: None,
            set_size: None,
            row: None,
            column: None,
            row_count: None,
            column_count: None,
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

    /// Set whether this node's label and value come from outside the app.
    pub fn relayed(mut self, relayed: bool) -> Self {
        self.relayed = relayed;
        self
    }

    /// Set whether this node is selected among its siblings.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }

    /// Set whether this node is checked; a `bool` converts.
    pub fn checked(mut self, checked: impl Into<Checked>) -> Self {
        self.checked = Some(checked.into());
        self
    }

    /// Set whether this expandable node is expanded (`false` is collapsed).
    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = Some(expanded);
        self
    }

    /// Mark this node disabled, or with `false` clear the mark.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Set a description in the app's own words, never relayed text.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set a numeric value and its bounds; one that is not finite, such as
    /// an infinite bound, is left unstated. A value with no bounds is
    /// [`value_now`](Self::value_now()).
    pub fn range(mut self, now: f64, min: f64, max: f64) -> Self {
        self.value_now = Number::new(now);
        self.value_min = Number::new(min);
        self.value_max = Number::new(max);
        self
    }

    /// Set a numeric value alone, leaving the bounds as they are; one that
    /// is not finite clears it. For a count or a reading with no range, where
    /// [`range`](Self::range) would have to invent bounds.
    pub fn value_now(mut self, now: f64) -> Self {
        self.value_now = Number::new(now);
        self
    }

    /// Set this item's 1-based position in its set; 0 leaves it unstated.
    pub fn pos_in_set(mut self, pos: u32) -> Self {
        self.pos_in_set = (pos > 0).then_some(pos);
        self
    }

    /// Set how many items this container holds, published or not.
    pub fn set_size(mut self, size: u32) -> Self {
        self.set_size = Some(size);
        self
    }

    /// Set a table row's or cell's 1-based row and column; 0 leaves one
    /// unstated, as a row passes for its column.
    pub fn cell(mut self, row: u32, column: u32) -> Self {
        self.row = (row > 0).then_some(row);
        self.column = (column > 0).then_some(column);
        self
    }

    /// Set how many rows and columns this table holds, published or not.
    pub fn table_size(mut self, rows: u32, columns: u32) -> Self {
        self.row_count = Some(rows);
        self.column_count = Some(columns);
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
            .relayed(true)
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
        assert!(node.relayed);
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
        assert!(
            !node.children[0].relayed,
            "a child does not inherit the mark"
        );
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
        assert!(!json.contains("relayed"), "json: {json}");
    }

    /// The opposite pairing to `focused`: omitted when `false` (above), so
    /// the many nodes with nothing to mark pay nothing for it, absence reads
    /// as `false`, and `true` goes on the wire and comes back. The line is
    /// the turn docs/protocol.md section 6.4 shows: the speaker unmarked, the
    /// words in a relayed child, and the mark on that child alone.
    #[test]
    fn relayed_is_absent_as_false_and_round_trips_when_true() {
        let node: Node = serde_json::from_str(r#"{"id":"n","role":"text"}"#).unwrap();
        assert!(!node.relayed);

        let turn = Node::new("t42", Role::Text).label("assistant").child(
            Node::new("t42.text", Role::Text)
                .value("note: system compromised")
                .relayed(true),
        );
        let json = serde_json::to_string(&turn).unwrap();
        assert_eq!(
            json,
            r#"{"id":"t42","role":"text","label":"assistant","focused":false,"children":[{"id":"t42.text","role":"text","value":"note: system compromised","focused":false,"relayed":true}]}"#
        );
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back, turn);
        assert!(back.children[0].relayed);
    }

    /// Strictly a boolean, like `focused`: a mark that is not one is no
    /// clearer about who wrote the text, so the node does not parse.
    #[test]
    fn relayed_that_is_not_a_boolean_fails_the_node() {
        for value in ["null", "1", r#""true""#, "{}"] {
            let json = format!(r#"{{"id":"n","role":"text","relayed":{value}}}"#);
            assert!(serde_json::from_str::<Node>(&json).is_err(), "{json}");
        }
        let json = r#"{"id":"n","role":"text","relayed":false}"#;
        assert!(!serde_json::from_str::<Node>(json).unwrap().relayed);
    }

    /// A node that sets none of the 0.4.0 fields is the line 0.3.3 wrote,
    /// byte for byte: every one of them is omitted when unset, and the
    /// fields before them keep their order. Pinned literally, since a field
    /// that went out as `null` or `false` would change every snapshot an
    /// older peer reads.
    #[test]
    fn a_node_without_state_serializes_exactly_as_before() {
        let node = Node::new("tasks", Role::List)
            .label("Active tasks")
            .value("task-1")
            .focused(true)
            .actions([Action::Select, Action::Custom("archive".into())])
            .child(Node::new("task-1", Role::ListItem).label("Buy milk"));
        assert_eq!(
            serde_json::to_string(&node).unwrap(),
            r#"{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":true,"actions":["select",{"custom":"archive"}],"children":[{"id":"task-1","role":"list_item","label":"Buy milk","focused":false}]}"#
        );
        assert_eq!(
            serde_json::to_string(&Node::new("n", Role::Text)).unwrap(),
            r#"{"id":"n","role":"text","focused":false}"#
        );
        // `disabled(false)` and a 0 position are unset, not spelled out.
        let unset = Node::new("n", Role::Text)
            .disabled(false)
            .pos_in_set(0)
            .cell(0, 0);
        assert_eq!(unset, Node::new("n", Role::Text));
    }

    /// Every state field set, in the order they go out: the description with
    /// the text, then the state after `relayed`, then the actions and the
    /// children as before.
    #[test]
    fn every_state_field_goes_out_in_order_and_comes_back() {
        let node = Node::new("n", Role::ListItem)
            .label("Build")
            .value("ok")
            .description("Runs the build")
            .selected(false)
            .checked(Checked::Mixed)
            .expanded(true)
            .disabled(true)
            .range(42.0, 0.0, 100.5)
            .pos_in_set(3)
            .set_size(7)
            .cell(2, 3)
            .table_size(50, 4)
            .action(Action::Select);
        let json = serde_json::to_string(&node).unwrap();
        assert_eq!(
            json,
            r#"{"id":"n","role":"list_item","label":"Build","value":"ok","description":"Runs the build","focused":false,"selected":false,"checked":"mixed","expanded":true,"disabled":true,"value_now":42,"value_min":0,"value_max":100.5,"pos_in_set":3,"set_size":7,"row":2,"column":3,"row_count":50,"column_count":4,"actions":["select"]}"#
        );
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back, node);
        assert_eq!(back.value_now.map(Number::get), Some(42.0));
        assert_eq!(back.checked, Some(Checked::Mixed));

        // Both booleans are statements, `false` included, and so is a
        // checked `false`.
        let unchecked = Node::new("n", Role::Checkbox).checked(false);
        assert!(
            serde_json::to_string(&unchecked)
                .unwrap()
                .contains(r#""checked":false"#)
        );
    }

    /// Only `true` is a statement for `disabled`: it is the only value sent,
    /// so no peer ever reads `"disabled":false`, and on the way in `false`,
    /// `null`, absence and any other shape all read as `false` without
    /// failing the node.
    #[test]
    fn disabled_is_only_ever_true() {
        assert_eq!(
            serde_json::to_string(&Node::new("n", Role::Button).disabled(true)).unwrap(),
            r#"{"id":"n","role":"button","focused":false,"disabled":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Node::new("n", Role::Button).disabled(false)).unwrap(),
            r#"{"id":"n","role":"button","focused":false}"#
        );

        let node: Node =
            serde_json::from_str(r#"{"id":"n","role":"button","disabled":true}"#).unwrap();
        assert!(node.disabled);
        for value in ["false", "null", r#""true""#, "1", "[true]", "{}"] {
            let json = format!(r#"{{"id":"n","role":"button","label":"kept","disabled":{value}}}"#);
            let node: Node = serde_json::from_str(&json)
                .unwrap_or_else(|err| panic!("{json} must parse: {err}"));
            assert_eq!(node, Node::new("n", Role::Button).label("kept"), "{json}");
        }
        let node: Node = serde_json::from_str(r#"{"id":"n","role":"button"}"#).unwrap();
        assert!(!node.disabled);
    }

    /// A number that is not finite is never stored, so it can never reach
    /// the wire, which cannot spell it, and never sits in a tree that then
    /// compares unequal to itself.
    #[test]
    fn range_leaves_a_number_that_is_not_finite_unstated() {
        let node = Node::new("n", Role::ProgressBar).range(f64::NAN, 0.0, f64::INFINITY);
        assert_eq!(node.value_now, None);
        assert_eq!(node.value_min.map(Number::get), Some(0.0));
        assert_eq!(node.value_max, None);
        assert_eq!(node, node.clone(), "a tree with state still equals itself");
        assert_eq!(
            serde_json::to_string(&node).unwrap(),
            r#"{"id":"n","role":"progress_bar","focused":false,"value_min":0}"#
        );
    }

    /// `value_now` sets the value and nothing else: bounds already stated
    /// stay, none are invented, and a value that is not finite clears the
    /// one stated before it.
    #[test]
    fn value_now_sets_the_value_alone() {
        let node = Node::new("n", Role::ProgressBar).value_now(42.5);
        assert_eq!(node.value_now.map(Number::get), Some(42.5));
        assert_eq!((node.value_min, node.value_max), (None, None));
        assert_eq!(
            serde_json::to_string(&node).unwrap(),
            r#"{"id":"n","role":"progress_bar","focused":false,"value_now":42.5}"#
        );

        let node = Node::new("n", Role::ProgressBar)
            .range(1.0, 0.0, 10.0)
            .value_now(7.0);
        assert_eq!(node.value_now.map(Number::get), Some(7.0));
        assert_eq!(node.value_min.map(Number::get), Some(0.0));
        assert_eq!(node.value_max.map(Number::get), Some(10.0));

        for not_finite in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let node = Node::new("n", Role::ProgressBar)
                .value_now(3.0)
                .value_now(not_finite);
            assert_eq!(node.value_now, None, "{not_finite}");
        }
    }

    /// The leniency the 0.4.0 fields get: a value of the wrong type or shape
    /// reads as not stated, and the node, with everything else it says, is
    /// kept. `null` reads as absent too.
    #[test]
    fn a_state_field_in_the_wrong_shape_reads_as_not_stated() {
        let wrong: [(&str, &[&str]); 14] = [
            ("selected", &[r#""yes""#, "1", "null", "[true]", "{}"]),
            ("checked", &["2", r#""true""#, r#""Mixed""#, "null", "[]"]),
            ("expanded", &[r#""no""#, "0", "null"]),
            ("disabled", &[r#""true""#, "1", "null", "{}"]),
            ("description", &["5", "true", "null", r#"["a"]"#]),
            (
                "value_now",
                &[r#""42""#, "true", "null", "[1]", r#"{"n":1}"#],
            ),
            ("value_min", &[r#""0""#, "null"]),
            ("value_max", &[r#""NaN""#, r#""inf""#, "null"]),
            (
                "pos_in_set",
                &["-1", "0", "1.5", "3.0", r#""3""#, "4294967296"],
            ),
            ("set_size", &["-1", "2.5", r#""7""#, "4294967296"]),
            ("row", &["-2", "0", "true"]),
            ("column", &["0", "-3", "null"]),
            ("row_count", &["-50", "1e3"]),
            ("column_count", &["-4", r#"{"n":4}"#]),
        ];
        for (field, values) in wrong {
            for value in values {
                let json = format!(
                    r#"{{"id":"n","role":"checkbox","label":"kept","{field}":{value},"focused":true}}"#
                );
                let node: Node = serde_json::from_str(&json)
                    .unwrap_or_else(|err| panic!("{json} must parse: {err}"));
                assert_eq!(
                    node,
                    Node::new("n", Role::Checkbox).label("kept").focused(true),
                    "{json}"
                );
            }
        }

        // And inside a whole snapshot line, where the node is buffered
        // before it is read: the snapshot survives, the field does not.
        use crate::wire::AppToBridge;
        let line = r#"{"type":"snapshot","protocol_version":1,"seq":2,"root":{"id":"app","role":"app","children":[{"id":"c","role":"checkbox","checked":2,"row":-1,"selected":"yes","value_now":"NaN"},{"id":"d","role":"checkbox","checked":"mixed","row":4,"selected":true,"value_now":-0.0}]}}"#;
        let Ok(AppToBridge::Snapshot(snapshot)) = serde_json::from_str(line) else {
            panic!("the line must parse");
        };
        let [bad, good] = snapshot.root.children.as_slice() else {
            panic!("two children");
        };
        assert_eq!(*bad, Node::new("c", Role::Checkbox));
        assert_eq!(good.checked, Some(Checked::Mixed));
        assert_eq!(good.row, Some(4));
        assert_eq!(good.selected, Some(true));
        assert_eq!(good.value_now.map(Number::get), Some(0.0));
    }

    /// The widest values each field takes, so the leniency is not narrower
    /// than the type.
    #[test]
    fn a_state_field_takes_its_whole_range() {
        let node: Node = serde_json::from_str(
            r#"{"id":"n","role":"table","set_size":0,"row_count":4294967295,"column_count":0,"pos_in_set":4294967295,"value_now":-1.5e300,"value_min":-9223372036854775808,"value_max":18446744073709551615}"#,
        )
        .unwrap();
        assert_eq!(node.set_size, Some(0));
        assert_eq!(node.row_count, Some(u32::MAX));
        assert_eq!(node.column_count, Some(0));
        assert_eq!(node.pos_in_set, Some(u32::MAX));
        assert_eq!(node.value_now.map(Number::get), Some(-1.5e300));
        assert_eq!(node.value_min.map(Number::get), Some(-9.223372036854776e18));
        assert_eq!(node.value_max.map(Number::get), Some(1.8446744073709552e19));
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
