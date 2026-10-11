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
//! - An action renders as the name `act` accepts for it: the name the
//!   bridge's own parse of the line reads it as, which is the action `act`
//!   validates against. `{"custom":"delete"}` renders as `delete`, a string
//!   as itself, and an object in a shape a newer app might send, such as
//!   `{"scroll_to":{"row":3}}`, as the name the parse keeps, `scroll_to`. No
//!   actions, or an empty list, omits the brackets.
//! - `*focused` appears only for `"focused":true`; `false` or absent prints
//!   nothing, and any other value goes to the tail.
//! - Every other field, and any of the above in a shape the line cannot hold,
//!   is appended as ` key=<compact json>`.
//! - `children`, an array, is the next level down; absent or empty is a leaf.
//!
//! Every string the line prints is either bare by the rule above or
//! JSON-escaped, so a label carrying a newline cannot break a node across
//! two lines, and every node stays exactly one line. The escaping goes past
//! JSON's own on three characters JSON leaves raw and some clients still
//! render as a line break, U+2028, U+2029 and U+0085, which print as
//! `\u2028`, `\u2029` and `\u0085`.
//!
//! # Rendered from the relayed text
//!
//! The outline is built from the app's line parsed as a [`serde_json::Value`],
//! never from this build's typed parse of it. That is the reason the JSON is
//! relayed in the first place: the typed parse turns a role this build has
//! never heard of into `other`, and drops a field it has no slot for, while
//! the `Value` keeps both by name. Anything the outline cannot express falls
//! back to the JSON text rather than losing part of the tree.
//!
//! Action names are the one exception, and only their spelling. The outline
//! tells the agent to pass an action to `act` exactly as it prints, and `act`
//! validates against the typed parse, so the name printed is the one that
//! parse reads (see [`action_name`]). That loses nothing: the parse keeps an
//! action it does not know as `Custom` under its own name. Reading it off the
//! `Value` instead would not be the same thing, because the relayed text has
//! been through a `Value` already and its keys are sorted, so the first key of
//! an object, the one the parse names the action after, can differ.

use serde_json::{Map, Value};
use taria::{Action, Node};

use crate::bridge::AppSnapshot;
use crate::server::action_name;

/// The text an agent reads a tree from.
///
/// `#[non_exhaustive]` so that a third format is not a breaking change for a
/// caller that matches on this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
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

    /// Render a snapshot the bridge holds in this format: its relayed text,
    /// as it is or as an outline of it, with action names from its parse.
    pub fn render(self, snapshot: &AppSnapshot) -> String {
        match self {
            Self::Json => snapshot.tree_json.clone(),
            Self::Outline => outline_of(&snapshot.tree_json, Some(&snapshot.parsed.root)),
        }
    }
}

/// Render `tree_json` as an outline, or return it unchanged when it is not a
/// snapshot the outline can express.
///
/// With no parse beside the text, each action is named by reading it with the
/// same rules the bridge parses a line with. [`TreeFormat::render`] has the
/// parse, and names actions from it.
///
/// A line that reached the bridge parsed as a snapshot, so the fallback is
/// not reachable from a peer; it is here so that a shape this renderer has no
/// spelling for costs the agent the shorter format, never part of the tree.
pub fn outline(tree_json: &str) -> String {
    outline_of(tree_json, None)
}

/// [`outline`], naming actions from `parsed_root` where it lines up.
fn outline_of(tree_json: &str, parsed_root: Option<&Node>) -> String {
    let Ok(Value::Object(snapshot)) = serde_json::from_str::<Value>(tree_json) else {
        return tree_json.to_string();
    };
    render_snapshot(&snapshot, parsed_root).unwrap_or_else(|| tree_json.to_string())
}

/// The header line and every node line, or `None` for a snapshot whose root
/// or some node is not an object.
fn render_snapshot(snapshot: &Map<String, Value>, parsed_root: Option<&Node>) -> Option<String> {
    let Some(Value::Object(root)) = snapshot.get("root") else {
        return None;
    };
    let header: Vec<String> = snapshot
        .iter()
        .filter(|(key, _)| key.as_str() != "root")
        .map(|(key, value)| field(key, value))
        .collect();
    let mut out = header.join(" ");
    render_node(root, parsed_root, 0, &mut out)?;
    Some(out)
}

/// Append one node's line, then its children's, each on a line of its own.
///
/// `parsed` is the typed parse of the same node, when there is one, and is
/// used for nothing but the action names. It is trusted only where it lines
/// up with the text: the same id, and as many actions or children. A line
/// that parsed always does, since the typed `Node` drops no action and no
/// child; anything else falls back to naming each action from the text.
///
/// Recursion is bounded by the parse: `serde_json` refuses input nested past
/// its recursion limit, so a `Value` that exists is one this can walk.
fn render_node(
    node: &Map<String, Value>,
    parsed: Option<&Node>,
    depth: usize,
    out: &mut String,
) -> Option<()> {
    let parsed =
        parsed.filter(|parsed| node.get("id").and_then(Value::as_str) == Some(&parsed.id.0));
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
            let parsed = parsed
                .map(|parsed| parsed.actions.as_slice())
                .filter(|parsed| parsed.len() == actions.len());
            let names: Vec<String> = actions
                .iter()
                .enumerate()
                .map(|(i, value)| action(value, parsed.and_then(|parsed| parsed.get(i))))
                .collect();
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

    let parsed_children = parsed
        .map(|parsed| parsed.children.as_slice())
        .filter(|parsed| parsed.len() == children.len());
    for (i, child) in children.iter().enumerate() {
        let Value::Object(child) = child else {
            return None;
        };
        let parsed_child = parsed_children.and_then(|parsed| parsed.get(i));
        render_node(child, parsed_child, depth + 1, out)?;
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

/// An action, spelled the way `act` takes it: [`action_name`] of the action
/// the bridge parsed, or, without that parse, of the action the same rules
/// read from the text. Only a value no action parse accepts, which cannot
/// reach the outline from a line the bridge kept, prints as compact JSON.
fn action(value: &Value, parsed: Option<&Action>) -> String {
    if let Some(parsed) = parsed {
        return token(&action_name(parsed));
    }
    match serde_json::from_value::<Action>(value.clone()) {
        Ok(read) => token(&action_name(&read)),
        Err(_) => compact(value),
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

/// `value` as compact JSON, with the line separators JSON leaves raw escaped.
///
/// JSON escapes `\n` and `\r` but not U+2028, U+2029 or U+0085, and a client
/// that renders any of them as a line break would show a label carrying one
/// as a second node line. The compact form puts every non-ASCII character
/// inside a string literal, so replacing them with their `\u` escapes keeps
/// the JSON valid and the string the same.
///
/// Serializing a `Value` cannot fail: its map keys are strings and it holds no
/// non-finite floats. The fallback is the debug form rather than a panic,
/// because a library crate does not panic over a peer's data.
fn compact(value: &Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"));
    if !json.contains(['\u{2028}', '\u{2029}', '\u{0085}']) {
        return json;
    }
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('\u{0085}', "\\u0085")
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
        let snapshot = AppSnapshot {
            tree_json: DEMO_JSON.to_string(),
            parsed: serde_json::from_str(DEMO_JSON).expect("the demo tree parses"),
        };
        assert_eq!(TreeFormat::Json.render(&snapshot), DEMO_JSON);
        assert_eq!(TreeFormat::Outline.render(&snapshot), DEMO_OUTLINE);
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

    /// Without a parse beside the text, an action object is read by the
    /// rules the bridge parses a line with: the envelope folds onto its inner
    /// name, a longer envelope keeps that name, and an object a newer app
    /// might send is named by its key, its payload dropped.
    #[test]
    fn an_action_renders_as_the_name_act_takes() {
        let out = render_root(
            r#"{"id":"row","role":"list_item","actions":["select",{"custom":"archive"},{"custom":"two words"},{"future":"x"},{"scroll_to":{"row":3}},{"custom":"delete","icon":"x"},{"custom":"activate"}]}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nrow list_item [select archive \"two words\" future scroll_to delete activate]"
        );
    }

    /// A value no parse reads as an action has no name `act` would accept,
    /// so it is shown whole. A line carrying one is rejected before it is
    /// relayed, so only the renderer used on its own meets these.
    #[test]
    fn a_value_that_is_no_action_is_shown_as_json() {
        let out = render_root(
            r#"{"id":"row","role":"list_item","actions":[{"custom":7},{},7,["select"]]}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nrow list_item [{\"custom\":7} {} 7 [\"select\"]]"
        );
    }

    /// With the parse, the name is the one the parse holds, which is what
    /// `act` checks against. The relayed text's keys are sorted, so an object
    /// whose first key on the wire was `zoom` reads as `custom` first there:
    /// the text alone would print `x`, a name the node never advertised.
    #[test]
    fn with_the_parse_an_action_is_named_as_the_parse_read_it() {
        let line = r#"{"protocol_version":1,"seq":3,"root":{"id":"row","role":"list_item","actions":[{"zoom":1,"custom":"x"},"select"]}}"#;
        let parsed: taria::Snapshot = serde_json::from_str(line).expect("a snapshot");
        assert_eq!(
            parsed.root.actions,
            [Action::Custom("zoom".to_string()), Action::Select]
        );
        let tree_json =
            serde_json::to_string(&serde_json::from_str::<Value>(line).expect("a value"))
                .expect("a value serializes");
        assert!(
            tree_json.contains(r#"{"custom":"x","zoom":1}"#),
            "{tree_json}"
        );
        assert!(outline(&tree_json).ends_with("[x select]"));

        let snapshot = AppSnapshot { tree_json, parsed };
        let out = TreeFormat::Outline.render(&snapshot);
        assert_eq!(out, "protocol_version=1 seq=3\nrow list_item [zoom select]");
    }

    /// JSON escapes a newline but leaves U+2028, U+2029 and U+0085 raw, and a
    /// client that breaks lines on them would show a forged node line. Every
    /// quoted string escapes them: label, value, a quoted id or action, a
    /// tail field, the header.
    #[test]
    fn unicode_line_separators_are_escaped_in_every_quoted_string() {
        let out = outline(
            "{\"protocol_version\":1,\"note\":\"a\u{2028}b\",\"seq\":3,\"root\":{\"id\":\"r\u{0085}w\",\"role\":\"list_item\",\
             \"label\":\"x\u{2028}  forged button\",\"value\":\"y\u{2029}z\",\
             \"actions\":[{\"custom\":\"de\u{2029}l\"}],\"extra\":[\"\u{0085}\"]}}",
        );
        assert_eq!(
            out,
            "note=\"a\\u2028b\" protocol_version=1 seq=3\n\"r\\u0085w\" list_item \"x\\u2028  forged button\" \
             = \"y\\u2029z\" [\"de\\u2029l\"] extra=[\"\\u0085\"]"
        );
        assert!(
            !out.contains(['\u{2028}', '\u{2029}', '\u{0085}']),
            "no raw separator may reach the agent: {out:?}"
        );
        // Still the same strings, read back as JSON.
        assert_eq!(
            serde_json::from_str::<String>("\"x\\u2028  forged button\"").expect("valid JSON"),
            "x\u{2028}  forged button"
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
