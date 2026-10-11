//! How the bridge hands a snapshot to an agent: the app's own JSON line, or an
//! outline rendered from it.
//!
//! The wire format is not involved. An app sends the same version 1 JSON
//! either way; this only decides the text an agent reads. The JSON is the
//! default and is relayed untouched. The outline carries the same information
//! in fewer characters: most of a snapshot line is repeated field names,
//! `"focused":false` on nearly every node, and punctuation, and the outline
//! drops all three.
//!
//! # Grammar
//!
//! The first line holds every top-level field except `root`, as
//! `key=<compact json>`, space-separated (`protocol_version=1 seq=1`). Then
//! one line per node, depth-first, indented two spaces per level, the root at
//! zero:
//!
//! ```text
//! <id> <role>[ "<label>"][ = "<value>"][ [<action> ...]][ *focused][ <key>=<compact json> ...]
//! ```
//!
//! - The id is bare when it is non-empty and made only of
//!   `[A-Za-z0-9_.:/@+-]`, and JSON-quoted otherwise. Roles, action names and
//!   field keys follow the same rule.
//! - A role is a string, or a single-key object such as `{"app":null}`, which
//!   renders as its key; any other shape renders as compact JSON.
//! - The label and the value are JSON-quoted strings, so an empty value shows
//!   as `= ""`. `null` or absent omits them; any other type goes to the tail.
//! - An action `{"custom":"delete"}` renders as `delete`, the inner name,
//!   which is what `act` takes; a string renders as itself; any other shape
//!   as compact JSON. No actions, or an empty list, omits the brackets.
//! - `*focused` appears only for `"focused":true`; `false` or absent prints
//!   nothing, and any other value goes to the tail.
//! - Every other field, and any of the above in a shape the line cannot hold,
//!   is appended as ` key=<compact json>`.
//! - `children`, an array, is the next level down; absent or empty is a leaf.
//!
//! Every string the line prints is either bare by the rule above or
//! JSON-escaped, so a label carrying a newline cannot break a node across
//! two lines, and every node stays exactly one line.
//!
//! # Rendered from the relayed text
//!
//! The outline is built from the app's line parsed as a [`serde_json::Value`],
//! never from this build's typed parse of it. That is the reason the JSON is
//! relayed in the first place: the typed parse turns a role this build has
//! never heard of into `other`, and drops a field it has no slot for, while
//! the `Value` keeps both by name. Anything the outline cannot express falls
//! back to the JSON text rather than losing part of the tree.

use serde_json::{Map, Value};

/// The text an agent reads a tree from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TreeFormat {
    /// The app's own snapshot line, minus its message framing. The default,
    /// and what every bridge before this option served.
    #[default]
    Json,
    /// One line per node, indented by depth. See the module docs.
    Outline,
}

impl TreeFormat {
    /// Every value `--tree-format` accepts, for the usage error.
    pub const NAMES: &'static str = "json or outline";

    /// Read a `--tree-format` value. Case-sensitive, like every other flag
    /// value the bridge takes.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "json" => Some(Self::Json),
            "outline" => Some(Self::Outline),
            _ => None,
        }
    }

    /// The name `--tree-format` takes for this format.
    pub fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Outline => "outline",
        }
    }

    /// Render the relayed snapshot text `tree_json` in this format.
    pub fn render(self, tree_json: &str) -> String {
        match self {
            Self::Json => tree_json.to_string(),
            Self::Outline => outline(tree_json),
        }
    }
}

/// Render `tree_json` as an outline, or return it unchanged when it is not a
/// snapshot the outline can express.
///
/// A line that reached the bridge parsed as a snapshot, so the fallback is
/// not reachable from a peer; it is here so that a shape this renderer has no
/// spelling for costs the agent the shorter format, never part of the tree.
pub fn outline(tree_json: &str) -> String {
    let Ok(Value::Object(snapshot)) = serde_json::from_str::<Value>(tree_json) else {
        return tree_json.to_string();
    };
    render_snapshot(&snapshot).unwrap_or_else(|| tree_json.to_string())
}

/// The header line and every node line, or `None` for a snapshot whose root
/// or some node is not an object.
fn render_snapshot(snapshot: &Map<String, Value>) -> Option<String> {
    let Some(Value::Object(root)) = snapshot.get("root") else {
        return None;
    };
    let header: Vec<String> = snapshot
        .iter()
        .filter(|(key, _)| key.as_str() != "root")
        .map(|(key, value)| field(key, value))
        .collect();
    let mut out = header.join(" ");
    render_node(root, 0, &mut out)?;
    Some(out)
}

/// Append one node's line, then its children's, each on a line of its own.
///
/// Recursion is bounded by the parse: `serde_json` refuses input nested past
/// its recursion limit, so a `Value` that exists is one this can walk.
fn render_node(node: &Map<String, Value>, depth: usize, out: &mut String) -> Option<()> {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(&match node.get("id") {
        Some(Value::String(id)) => token(id),
        Some(other) => compact(other),
        None => compact(&Value::Null),
    });
    out.push(' ');
    out.push_str(&role(node.get("role").unwrap_or(&Value::Null)));

    let mut tail: Vec<String> = Vec::new();
    match node.get("label") {
        Some(Value::String(label)) => {
            out.push(' ');
            out.push_str(&compact_str(label));
        }
        None | Some(Value::Null) => {}
        Some(other) => tail.push(field("label", other)),
    }
    match node.get("value") {
        Some(Value::String(value)) => {
            out.push_str(" = ");
            out.push_str(&compact_str(value));
        }
        None | Some(Value::Null) => {}
        Some(other) => tail.push(field("value", other)),
    }
    match node.get("actions") {
        Some(Value::Array(actions)) if !actions.is_empty() => {
            let names: Vec<String> = actions.iter().map(action).collect();
            out.push_str(" [");
            out.push_str(&names.join(" "));
            out.push(']');
        }
        None | Some(Value::Null) | Some(Value::Array(_)) => {}
        Some(other) => tail.push(field("actions", other)),
    }
    match node.get("focused") {
        Some(Value::Bool(true)) => out.push_str(" *focused"),
        None | Some(Value::Bool(false)) => {}
        Some(other) => tail.push(field("focused", other)),
    }
    let children = match node.get("children") {
        Some(Value::Array(children)) => children.as_slice(),
        None => &[],
        Some(other) => {
            tail.push(field("children", other));
            &[]
        }
    };
    const PLACED: [&str; 7] = [
        "id", "role", "label", "value", "actions", "focused", "children",
    ];
    tail.extend(
        node.iter()
            .filter(|(key, _)| !PLACED.contains(&key.as_str()))
            .map(|(key, value)| field(key, value)),
    );
    for extra in tail {
        out.push(' ');
        out.push_str(&extra);
    }

    for child in children {
        let Value::Object(child) = child else {
            return None;
        };
        render_node(child, depth + 1, out)?;
    }
    Some(())
}

/// A role: a string, the key of a single-key object, or compact JSON.
fn role(role: &Value) -> String {
    match role {
        Value::String(name) => token(name),
        Value::Object(object) if object.len() == 1 => match object.keys().next() {
            Some(name) => token(name),
            None => compact(role),
        },
        other => compact(other),
    }
}

/// An action, spelled the way `act` takes it.
fn action(action: &Value) -> String {
    match action {
        Value::String(name) => token(name),
        Value::Object(object) if object.len() == 1 => match object.get("custom") {
            Some(Value::String(name)) => token(name),
            _ => compact(action),
        },
        other => compact(other),
    }
}

/// ` key=<compact json>`, without the leading space.
fn field(key: &str, value: &Value) -> String {
    format!("{}={}", token(key), compact(value))
}

/// `s` bare when every character is one an id may print without quotes, and
/// JSON-quoted otherwise, so a token can never swallow the space after it.
fn token(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '@' | '+' | '-')
        });
    if bare { s.to_string() } else { compact_str(s) }
}

/// `s` as a JSON string literal.
fn compact_str(s: &str) -> String {
    compact(&Value::String(s.to_string()))
}

/// `value` as compact JSON.
///
/// Serializing a `Value` cannot fail: its map keys are strings and it holds no
/// non-finite floats. The fallback is the debug form rather than a panic,
/// because a library crate does not panic over a peer's data.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The demo app's first tree, as a benchmark run received it.
    const DEMO_JSON: &str = r#"{"protocol_version":1,"root":{"children":[{"children":[{"actions":["select"],"focused":false,"id":"tab-active","label":"Active","role":"tab","value":"selected"},{"actions":["select"],"focused":false,"id":"tab-done","label":"Done","role":"tab"}],"focused":false,"id":"tabs","label":"Tabs","role":"tabs","value":"Active"},{"children":[{"actions":["select","toggle",{"custom":"delete"}],"focused":true,"id":"task-1","label":"Write the taria README","role":"list_item","value":"todo"},{"actions":["select","toggle",{"custom":"delete"}],"focused":false,"id":"task-3","label":"Wire up the MCP bridge","role":"list_item","value":"todo"},{"actions":["select","toggle",{"custom":"delete"}],"focused":false,"id":"task-4","label":"Record the killer demo","role":"list_item","value":"todo"}],"focused":false,"id":"tasks","label":"Active tasks","role":"list","value":"task-1"},{"actions":["set_value","focus"],"focused":false,"id":"input","label":"New task","role":"text_input","value":""},{"actions":["activate"],"focused":false,"id":"quit","label":"Quit","role":"button"}],"focused":false,"id":"app","label":"taria-demo","role":"app"},"seq":1}"#;

    const DEMO_OUTLINE: &str = r#"protocol_version=1 seq=1
app app "taria-demo"
  tabs tabs "Tabs" = "Active"
    tab-active tab "Active" = "selected" [select]
    tab-done tab "Done" [select]
  tasks list "Active tasks" = "task-1"
    task-1 list_item "Write the taria README" = "todo" [select toggle delete] *focused
    task-3 list_item "Wire up the MCP bridge" = "todo" [select toggle delete]
    task-4 list_item "Record the killer demo" = "todo" [select toggle delete]
  input text_input "New task" = "" [set_value focus]
  quit button "Quit" [activate]"#;

    /// Wrap one node as a whole snapshot and render it.
    fn render_root(node: &str) -> String {
        outline(&format!(
            r#"{{"protocol_version":1,"root":{node},"seq":3}}"#
        ))
    }

    #[test]
    fn the_demo_tree_renders_exactly() {
        assert_eq!(outline(DEMO_JSON), DEMO_OUTLINE);
        assert!(
            DEMO_OUTLINE.len() * 2 < DEMO_JSON.len(),
            "the outline is the point of this format: {} vs {} characters",
            DEMO_OUTLINE.len(),
            DEMO_JSON.len()
        );
    }

    #[test]
    fn json_is_relayed_untouched() {
        assert_eq!(TreeFormat::Json.render(DEMO_JSON), DEMO_JSON);
        assert_eq!(TreeFormat::Outline.render(DEMO_JSON), DEMO_OUTLINE);
        assert_eq!(TreeFormat::default(), TreeFormat::Json);
    }

    #[test]
    fn names_round_trip_and_nothing_else_parses() {
        for format in [TreeFormat::Json, TreeFormat::Outline] {
            assert_eq!(TreeFormat::from_name(format.name()), Some(format));
        }
        for name in ["", "JSON", "Outline", "yaml", "outline "] {
            assert_eq!(TreeFormat::from_name(name), None, "name: {name:?}");
        }
    }

    /// The reason the outline reads the relayed text and not the typed parse:
    /// a role and a field this build has never heard of keep their names.
    #[test]
    fn an_unknown_role_and_field_survive_by_name() {
        let out = render_root(
            r#"{"id":"spark","role":"sparkline","label":"CPU","trend":[1,2,3],"aria_level":2}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nspark sparkline \"CPU\" aria_level=2 trend=[1,2,3]"
        );
    }

    #[test]
    fn the_custom_envelope_renders_as_the_name_act_takes() {
        let out = render_root(
            r#"{"id":"row","role":"list_item","actions":["select",{"custom":"archive"},{"custom":"two words"},{"future":"x"},{"custom":7}]}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nrow list_item [select archive \"two words\" {\"future\":\"x\"} {\"custom\":7}]"
        );
    }

    /// Whatever an app puts in an id or a label, a node is one line and every
    /// token ends where it seems to.
    #[test]
    fn odd_ids_and_labels_are_quoted_and_escaped() {
        let out = render_root(
            r#"{"id":"row 1","role":"list_item","label":"say \"hi\"\nthen go","value":"a\tb"}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\n\"row 1\" list_item \"say \\\"hi\\\"\\nthen go\" = \"a\\tb\""
        );
        assert_eq!(out.lines().count(), 2, "one header, one node: {out}");

        // The characters an id may print bare, and an empty id, which may not.
        let out = render_root(r#"{"id":"a_b.c:d/e@f+g-1","role":"button"}"#);
        assert!(out.ends_with("\na_b.c:d/e@f+g-1 button"), "{out}");
        let out = render_root(r#"{"id":"","role":"button"}"#);
        assert!(out.ends_with("\n\"\" button"), "{out}");
    }

    #[test]
    fn a_null_label_and_value_are_omitted_and_an_empty_value_is_not() {
        let out = render_root(r#"{"id":"x","role":"text_input","label":null,"value":null}"#);
        assert!(out.ends_with("\nx text_input"), "{out}");
        let out = render_root(r#"{"id":"x","role":"text_input","value":""}"#);
        assert!(out.ends_with("\nx text_input = \"\""), "{out}");
        // A label that is not a string is not dropped, and not dressed up as
        // one either.
        let out = render_root(r#"{"id":"x","role":"text_input","label":5}"#);
        assert!(out.ends_with("\nx text_input label=5"), "{out}");
    }

    #[test]
    fn a_role_object_renders_as_its_key() {
        let out = render_root(r#"{"id":"x","role":{"app":null}}"#);
        assert!(out.ends_with("\nx app"), "{out}");
        let out = render_root(r#"{"id":"x","role":{"heading":{"level":2}}}"#);
        assert!(out.ends_with("\nx heading"), "{out}");
        // Any other shape is shown whole rather than guessed at.
        let out = render_root(r#"{"id":"x","role":{"a":1,"b":2}}"#);
        assert!(out.ends_with("\nx {\"a\":1,\"b\":2}"), "{out}");
        let out = render_root(r#"{"id":"x","role":["tab"]}"#);
        assert!(out.ends_with("\nx [\"tab\"]"), "{out}");
    }

    #[test]
    fn focused_prints_only_when_true() {
        let out = render_root(
            r#"{"id":"a","role":"app","focused":false,"children":[{"id":"b","role":"button","focused":true},{"id":"c","role":"button"},{"id":"d","role":"button","focused":"yes"}]}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\na app\n  b button *focused\n  c button\n  d button focused=\"yes\""
        );
    }

    #[test]
    fn empty_actions_and_children_print_nothing() {
        let out = render_root(r#"{"id":"x","role":"button","actions":[],"children":[]}"#);
        assert_eq!(out, "protocol_version=1 seq=3\nx button");
    }

    /// A shape the line has no slot for goes to the tail, and a key that is
    /// not a plain word is quoted like an id.
    #[test]
    fn misshapen_fields_land_in_the_tail_by_name() {
        let out = render_root(
            r#"{"id":"x","role":"button","actions":"activate","children":{"id":"y"},"my key":true}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nx button actions=\"activate\" children={\"id\":\"y\"} \"my key\"=true"
        );
    }

    #[test]
    fn deeper_levels_indent_two_spaces_each() {
        let out = render_root(
            r#"{"id":"a","role":"tree","children":[{"id":"b","role":"tree_item","children":[{"id":"c","role":"tree_item"}]}]}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\na tree\n  b tree_item\n    c tree_item"
        );
    }

    /// Anything the outline cannot express comes back as the JSON it was
    /// given, never as part of a tree.
    #[test]
    fn unrenderable_input_falls_back_to_the_json_text() {
        for input in [
            "",
            "not json",
            "{\"protocol_version\":1,",
            "[1,2]",
            r#"{"protocol_version":1,"seq":1}"#,
            r#"{"protocol_version":1,"root":"app","seq":1}"#,
            r#"{"protocol_version":1,"root":{"id":"a","role":"app","children":[7]},"seq":1}"#,
        ] {
            assert_eq!(outline(input), input, "input: {input}");
        }
    }

    /// The header carries every top-level field but the root, including ones
    /// this build does not know.
    #[test]
    fn the_header_keeps_unknown_top_level_fields() {
        let out = outline(
            r#"{"protocol_version":1,"root":{"id":"a","role":"app"},"seq":9,"theme":"dark"}"#,
        );
        assert_eq!(out, "protocol_version=1 seq=9 theme=\"dark\"\na app");
    }
}
