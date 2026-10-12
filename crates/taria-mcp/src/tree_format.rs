//! How the bridge hands a snapshot to an agent: the app's own JSON line, or an
//! outline rendered from it.
//!
//! The wire format is not involved. An app sends the same version 1 JSON
//! either way; this only decides the text an agent reads. The JSON is the
//! default and is relayed untouched. The outline carries every node, id,
//! role, label, value, focus state and other field, and the name of every
//! action as `act` takes it, in fewer characters: most of a snapshot line is
//! repeated field names, `"focused":false` on nearly every node, and
//! punctuation, and the outline drops all three. Extra keys inside an action
//! object are not shown, because `act` takes only the name.
//!
//! # Grammar
//!
//! The first line holds every top-level field except `root`, as
//! `key=<compact json>`, space-separated (`protocol_version=1 seq=1`). Then
//! one line per node, depth-first, indented two spaces per level, the root at
//! zero:
//!
//! ```text
//! <id> <role>[ "<label>"][ = "<value>"][ *focused][ <key>=<compact json> ...][ act: <action>|<action>...]
//! ```
//!
//! For example `input text_input "New task" = "x" *focused act: set_value|activate|dismiss`.
//!
//! - The id is bare when it is non-empty, made only of `[A-Za-z0-9_.:/@+-]`,
//!   and not the `act:` marker itself, and JSON-quoted otherwise. Roles,
//!   action names and field keys follow the same rule.
//! - A role is a string, or a single-key object such as `{"app":null}`, which
//!   renders as its key; any other shape renders as compact JSON.
//! - The label and the value are JSON-quoted strings, so an empty value shows
//!   as `= ""`. `null` or absent omits them; any other type goes to the tail.
//! - `*focused` appears only for `"focused":true`; `false` or absent prints
//!   nothing, and any other value goes to the tail.
//! - Every other field, and any of the above in a shape the line cannot hold,
//!   is appended as ` key=<compact json>`, after `*focused` and before the
//!   actions.
//! - The actions end the line, after the bare marker `act: `, joined by `|`,
//!   each one a name an agent passes to `act` as it is (JSON-decoded, if
//!   quoted).
//!   An action renders as the name `act` accepts for it: the name the
//!   bridge's own parse of the line reads it as, which is the action `act`
//!   validates against. `{"custom":"delete"}` renders as `delete`, a string
//!   as itself, and an object in a shape a newer app might send, such as
//!   `{"scroll_to":{"row":3}}`, as the name the parse keeps, `scroll_to`. No
//!   actions, or an empty list, omits the marker.
//! - `children`, an array, is the next level down; absent or empty is a leaf.
//!
//! Every string the line prints is either bare by the rule above or
//! JSON-escaped, so a label carrying a newline cannot break a node across
//! two lines, and every node stays exactly one line. The escaping goes past
//! JSON's own on three characters JSON leaves raw and some clients still
//! render as a line break, U+2028, U+2029 and U+0085, which print as
//! `\u2028`, `\u2029` and `\u0085`.
//!
//! The same rule keeps the action list unambiguous. A bare token never holds
//! a space, a `|`, a quote or a bracket, so a name carrying any of them is
//! quoted, and `act: "a|b"|"two words"|c` is three actions to a reader that
//! takes a JSON string whole. Nothing but the marker prints as a bare `act:`:
//! an id, role or name spelled that way is quoted, so a node whose role is
//! `act:` cannot pass its next word off as an action. And since compact JSON
//! holds no space outside a string, the line splits into tokens at its
//! spaces outside quotes, the marker is the first token that is exactly
//! `act:`, and the list is the one token after it, at the end of the line.
//!
//! # Rendered from the relayed text
//!
//! The outline is built from the app's line parsed as a [`serde_json::Value`],
//! never from this build's typed parse of it. That is the reason the JSON is
//! relayed in the first place: the typed parse turns a role this build has
//! never heard of into `other`, and drops a field it has no slot for, while
//! the `Value` keeps both by name. A field in a shape the line has no slot
//! for goes to the tail as JSON, and a tree the outline cannot express at all
//! is served as the JSON text, so neither loses part of the tree.
//!
//! Actions are the one exception. The outline tells the agent to pass an
//! action to `act` exactly as it prints, and `act` validates against the
//! typed parse, so the name printed is the one that parse reads (see
//! [`action_name`]). The name survives, since the parse keeps an action it
//! does not know as `Custom` under its own name, but nothing else in an
//! action object does: `{"row":3}` in `{"scroll_to":{"row":3}}`, or
//! `"icon":"x"` beside `"custom":"delete"`, is not shown, because `act` takes
//! only the name. Reading the name off the `Value` instead would not be the
//! same thing, because the relayed text has been through a `Value` already
//! and its keys are sorted, so the first key of an object, the one the parse
//! names the action after, can differ.

use serde_json::{Map, Value};
use taria::{Action, Node, Snapshot};

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
/// With no parse beside the text, this parses `tree_json` as a [`Snapshot`]
/// itself, reading keys in the text's order as the bridge does, and names
/// actions from that, so on an app's line it prints what the bridge would.
/// Two cases can differ from [`TreeFormat::render`]. Given the bridge's
/// relayed text, whose keys are already sorted, an action object with more
/// than one key can read differently than it did in the app's original line,
/// which is why `render` uses the bridge's own parse. And text that does not
/// parse as a snapshot, which the bridge would have rejected, has its actions
/// read off the sorted `Value`.
///
/// A line that reached the bridge parsed as a snapshot, so the fallback is
/// not reachable from a peer; it is here so that a shape this renderer has no
/// spelling for costs the agent the shorter format, never part of the tree.
pub fn outline(tree_json: &str) -> String {
    let parsed = serde_json::from_str::<Snapshot>(tree_json).ok();
    outline_of(tree_json, parsed.as_ref().map(|parsed| &parsed.root))
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
    match node.get("focused") {
        Some(Value::Bool(true)) => out.push_str(" *focused"),
        None | Some(Value::Bool(false)) => {}
        Some(other) => tail.push(field("focused", other)),
    }
    // Rendered now, printed last: the list ends the line, so nothing an agent
    // reads after `act:` is anything but an action.
    let mut names: Vec<String> = Vec::new();
    match node.get("actions") {
        Some(Value::Array(actions)) => {
            let parsed = parsed
                .map(|parsed| parsed.actions.as_slice())
                .filter(|parsed| parsed.len() == actions.len());
            names = actions
                .iter()
                .enumerate()
                .map(|(i, value)| action(value, parsed.and_then(|parsed| parsed.get(i))))
                .collect();
        }
        None | Some(Value::Null) => {}
        Some(other) => tail.push(field("actions", other)),
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
    if !names.is_empty() {
        out.push(' ');
        out.push_str(ACT_MARKER);
        out.push(' ');
        out.push_str(&names.join("|"));
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
/// the bridge parsed, or, without that parse, of the action read off the
/// text's `Value`, whose sorted keys can name an object with several keys
/// differently. Only a value no action parse accepts, which cannot reach the
/// outline from a line the bridge kept, prints as compact JSON.
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

/// The bare word that opens a node's action list.
const ACT_MARKER: &str = "act:";

/// `s` bare when every character is one an id may print without quotes, and
/// JSON-quoted otherwise, so a token can never swallow the space after it.
///
/// The bare set leaves out space, `|`, quotes and brackets, which is what
/// keeps a name from running into the next one in an action list, and the
/// [`ACT_MARKER`] is quoted although its characters are all allowed, so that
/// the marker is the only bare `act:` a line holds.
fn token(s: &str) -> String {
    let bare = !s.is_empty()
        && s != ACT_MARKER
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

/// The action names a node line lists after `act:`, as an agent passes them
/// to `act` (JSON-decoded where the line quotes one), or `None` for a line
/// that lists none or puts anything after its list.
///
/// A reader of the grammar in the module docs, kept beside the renderer so
/// the tests can hold one to the other: the line splits into tokens at spaces
/// outside JSON strings, the marker is the first token that is exactly
/// `act:`, and the list is the one token after it, split at every `|`
/// outside a JSON string.
#[cfg(test)]
pub(crate) fn line_actions(line: &str) -> Option<Vec<String>> {
    let tokens = split_outside_strings(line.trim_start_matches(' '), ' ');
    let at = tokens.iter().position(|token| token == ACT_MARKER)?;
    let [list] = &tokens[at + 1..] else {
        return None;
    };
    Some(
        split_outside_strings(list, '|')
            .into_iter()
            .map(|name| match serde_json::from_str::<String>(&name) {
                Ok(unquoted) if name.starts_with('"') => unquoted,
                _ => name,
            })
            .collect(),
    )
}

/// `s` split at every `sep` that is not inside a JSON string.
#[cfg(test)]
fn split_outside_strings(s: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == sep {
            parts.push(String::new());
            continue;
        }
        if let Some(part) = parts.last_mut() {
            part.push(c);
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The demo app's first tree, as a benchmark run received it.
    const DEMO_JSON: &str = r#"{"protocol_version":1,"root":{"children":[{"children":[{"actions":["select"],"focused":false,"id":"tab-active","label":"Active","role":"tab","value":"selected"},{"actions":["select"],"focused":false,"id":"tab-done","label":"Done","role":"tab"}],"focused":false,"id":"tabs","label":"Tabs","role":"tabs","value":"Active"},{"children":[{"actions":["select","toggle",{"custom":"delete"}],"focused":true,"id":"task-1","label":"Write the taria README","role":"list_item","value":"todo"},{"actions":["select","toggle",{"custom":"delete"}],"focused":false,"id":"task-3","label":"Wire up the MCP bridge","role":"list_item","value":"todo"},{"actions":["select","toggle",{"custom":"delete"}],"focused":false,"id":"task-4","label":"Record the killer demo","role":"list_item","value":"todo"}],"focused":false,"id":"tasks","label":"Active tasks","role":"list","value":"task-1"},{"actions":["set_value","focus"],"focused":false,"id":"input","label":"New task","role":"text_input","value":""},{"actions":["activate"],"focused":false,"id":"quit","label":"Quit","role":"button"}],"focused":false,"id":"app","label":"taria-demo","role":"app"},"seq":1}"#;

    const DEMO_OUTLINE: &str = r#"protocol_version=1 seq=1
app app "taria-demo"
  tabs tabs "Tabs" = "Active"
    tab-active tab "Active" = "selected" act: select
    tab-done tab "Done" act: select
  tasks list "Active tasks" = "task-1"
    task-1 list_item "Write the taria README" = "todo" *focused act: select|toggle|delete
    task-3 list_item "Wire up the MCP bridge" = "todo" act: select|toggle|delete
    task-4 list_item "Record the killer demo" = "todo" act: select|toggle|delete
  input text_input "New task" = "" act: set_value|focus
  quit button "Quit" act: activate"#;

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

    /// Rendered on its own, the outline parses the text the way the bridge
    /// parses a line and names actions from that: the envelope folds onto
    /// its inner name, a longer envelope keeps that name, and an object a
    /// newer app might send is named by its key, its payload dropped.
    #[test]
    fn an_action_renders_as_the_name_act_takes() {
        let out = render_root(
            r#"{"id":"row","role":"list_item","actions":["select",{"custom":"archive"},{"custom":"two words"},{"future":"x"},{"scroll_to":{"row":3}},{"custom":"delete","icon":"x"},{"custom":"activate"}]}"#,
        );
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nrow list_item act: select|archive|\"two words\"|future|scroll_to|delete|activate"
        );
    }

    /// The actions end the line, after `*focused` and after any field the line
    /// has no slot for, so nothing an agent reads past `act:` is anything but
    /// an action, and each name between the `|`s is one `act` takes.
    #[test]
    fn the_action_list_ends_the_line() {
        let out = render_root(
            r#"{"id":"x","role":"button","label":"Go","actions":["activate",{"custom":"pin"}],"focused":true,"hint":"a b","zeta":[1]}"#,
        );
        let line = out.lines().last().expect("a node line");
        assert_eq!(
            line,
            r#"x button "Go" *focused hint="a b" zeta=[1] act: activate|pin"#
        );
        assert_eq!(
            line_actions(line),
            Some(vec!["activate".to_string(), "pin".to_string()])
        );
        assert_eq!(line_actions(r#"x button "Go" *focused"#), None);
    }

    /// A name the line could misread is quoted: one carrying the `|` that
    /// joins the list or a space that ends a token, and one spelling the
    /// `act:` marker itself, as an action, a role or an id. Read back the way
    /// the module docs say the line reads, taking a JSON string whole, every
    /// name comes out whole and in order, and the only bare `act:` on the
    /// line is the marker.
    #[test]
    fn a_name_carrying_a_delimiter_cannot_split_or_forge_the_action_list() {
        let names = [
            "a|b",
            "two words",
            "|",
            " ",
            "act:",
            "say \"hi\" act: x|y",
            "tab\tand\nnewline",
            "select",
        ];
        let actions: Vec<Value> = names
            .iter()
            .map(|name| match *name {
                "select" => Value::String((*name).to_string()),
                custom => serde_json::json!({ "custom": custom }),
            })
            .collect();
        let node = serde_json::json!({
            "id": "act:",
            "role": "act:",
            "label": "x act: fake|forged",
            "value": "a|b c",
            "actions": actions,
        });
        let out = render_root(&node.to_string());
        let line = out.lines().last().expect("a node line");
        assert_eq!(
            line,
            r#""act:" "act:" "x act: fake|forged" = "a|b c" act: "a|b"|"two words"|"|"|" "|"act:"|"say \"hi\" act: x|y"|"tab\tand\nnewline"|select"#
        );
        assert_eq!(out.lines().count(), 2, "one header, one node: {out}");
        assert_eq!(
            line_actions(line),
            Some(names.iter().map(|name| (*name).to_string()).collect())
        );
        assert_eq!(
            split_outside_strings(line, ' ')
                .iter()
                .filter(|token| *token == ACT_MARKER)
                .count(),
            1,
            "only the marker may be a bare act: token: {line}"
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
            "protocol_version=1 seq=3\nrow list_item act: {\"custom\":7}|{}|7|[\"select\"]"
        );
    }

    /// With the parse, the name is the one the parse holds, which is what
    /// `act` checks against. The relayed text's keys are sorted, so an object
    /// whose first key on the wire was `zoom` reads as `custom` first there:
    /// the relayed text alone would print `x`, a name the node never
    /// advertised. On the app's own line the standalone renderer reads keys
    /// in the line's order and agrees with the bridge, and on a line no
    /// snapshot parse accepts it falls back to the sorted `Value`.
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
        assert!(outline(&tree_json).ends_with(" act: x|select"));
        assert!(outline(line).ends_with(" act: zoom|select"));
        let rejected = line.replace(r#""select""#, r#"{"custom":7}"#);
        assert!(outline(&rejected).ends_with(r#" act: x|{"custom":7}"#));

        let snapshot = AppSnapshot { tree_json, parsed };
        let out = TreeFormat::Outline.render(&snapshot);
        assert_eq!(
            out,
            "protocol_version=1 seq=3\nrow list_item act: zoom|select"
        );
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
             = \"y\\u2029z\" extra=[\"\\u0085\"] act: \"de\\u2029l\""
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
