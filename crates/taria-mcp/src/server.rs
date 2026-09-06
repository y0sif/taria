//! The MCP tool surface: `read_tree`, `act`, `key`, and `type_text`.

use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use taria::key::KeyPress;
use taria::wire::{InputId, InputStatus};
use taria::{Action, AgentInput, Node, NodeId, Snapshot};
use tokio::sync::{broadcast, watch};

use crate::bridge::{BridgeHandle, BridgeState};

/// How long an input-sending tool waits for the app to answer, counting both
/// the app's ack and any snapshot it publishes in response.
const UPDATE_WAIT: Duration = Duration::from_millis(500);

/// Most presses one `key` call may send.
///
/// The point of `repeat` is to spend one call on "move down five rows", not to
/// hand an agent a way to fill the app's input queue from a single call; the
/// app drops what overflows it.
const MAX_KEY_REPEAT: u32 = 64;

/// Longest string one `type_text` call may carry, in characters.
///
/// A bounded payload keeps one call from monopolizing the app's input queue:
/// the adapter lowers the text into one key event per character.
const MAX_TEXT_CHARS: usize = 4096;

/// Instructions surfaced to the agent on MCP initialize.
const INSTRUCTIONS: &str = "Bridge to a live terminal (TUI) application. Call read_tree first: \
it returns the app's current semantic tree, including every node id and the actions each node \
advertises. Prefer act with a node id and one of that node's advertised actions (pass value for \
set_value); node ids and action names come from the tree, never guess them. type_text types a \
literal string in one call instead of one call per character; use it for anything you would \
otherwise spell out with key. key sends a single raw key press and is a fallback for parts of \
the UI without semantic coverage; its repeat parameter sends the same key up to 64 times, so \
\"move down five rows\" is one call. act, key and type_text return the updated tree when the app \
reacts; call read_tree again whenever you need a fresh view.";

/// Parameters for the `act` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ActParams {
    /// Id of the target node, exactly as it appears in the tree from read_tree.
    pub node: String,
    /// Action name advertised by that node (e.g. "activate", "toggle",
    /// "set_value", or an app-specific custom action name).
    pub action: String,
    /// Value for actions that take one (e.g. the text for "set_value").
    pub value: Option<String>,
}

/// Parameters for the `key` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// Key to press, e.g. "q", "enter", "tab", "ctrl+c".
    pub key: String,
    /// How many times to send this key, 1 to 64. Defaults to 1.
    #[schemars(range(min = 1, max = 64))]
    pub repeat: Option<u32>,
}

/// Parameters for the `type_text` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TypeTextParams {
    /// Literal text to type, up to 4096 characters.
    #[schemars(length(min = 1, max = 4096))]
    pub text: String,
}

/// MCP server handler bridging tool calls to the socket-manager task.
#[derive(Clone)]
pub struct TariaMcpServer {
    bridge: BridgeHandle,
    tool_router: ToolRouter<Self>,
}

impl TariaMcpServer {
    /// Build the handler on top of a spawned bridge.
    pub fn new(handle: BridgeHandle) -> Self {
        Self {
            bridge: handle,
            tool_router: Self::tool_router(),
        }
    }

    /// Send `input` to the app `repeat` times and report on the last one.
    ///
    /// Every copy carries its own [`InputId`], but they are the same input, so
    /// the last id is the one worth waiting on: the app works through them in
    /// order, and its answer to the last covers the ones before it.
    ///
    /// `repeat` of 0 still sends once. The count is validated by the caller,
    /// and sending nothing while reporting on an input that was never sent
    /// would be the worse failure.
    async fn send_and_report(
        &self,
        rx: watch::Receiver<BridgeState>,
        pre: Snapshot,
        input: AgentInput,
        repeat: u32,
    ) -> Result<CallToolResult, McpError> {
        // Subscribe before anything is sent: an ack published before this
        // point is one the receiver would never see.
        let acks = self.bridge.subscribe_acks();
        for _ in 1..repeat {
            self.send_input(input.clone()).await?;
        }
        let id = self.send_input(input).await?;
        report(observe(acks, rx, &pre, id).await, pre)
    }

    /// Hand one input to the socket task, returning the id it was sent under.
    async fn send_input(&self, input: AgentInput) -> Result<InputId, McpError> {
        let id = self.bridge.next_input_id();
        self.bridge.input_tx.send((id, input)).await.map_err(|_| {
            McpError::internal_error(
                "failed to forward input: the bridge connection task is gone",
                None,
            )
        })?;
        Ok(id)
    }
}

#[tool_router]
impl TariaMcpServer {
    /// The `read_tree` tool: latest snapshot as compact JSON.
    #[tool(
        description = "Read the connected TUI app's current semantic tree as JSON: every node \
                       with its id, role, label, value, focus state, and advertised actions. \
                       Call this first; act and key depend on ids and actions from this tree."
    )]
    pub async fn read_tree(&self) -> Result<CallToolResult, McpError> {
        let snapshot = available_snapshot(self.bridge.state_rx.borrow().clone())?;
        tree_result(&snapshot)
    }

    /// The `act` tool: validate and forward an advertised action.
    #[tool(
        description = "Invoke an advertised action on a node of the app's semantic tree. `node` \
                       is a node id and `action` an action name, both taken from read_tree; pass \
                       `value` for actions that need one (e.g. set_value). Returns the updated \
                       tree once the app reacts."
    )]
    pub async fn act(
        &self,
        Parameters(ActParams {
            node,
            action,
            value,
        }): Parameters<ActParams>,
    ) -> Result<CallToolResult, McpError> {
        if action.is_empty() {
            return Err(McpError::invalid_params(
                "action must be a non-empty action name",
                None,
            ));
        }
        let mut rx = self.bridge.state_rx.clone();
        let snapshot = available_snapshot(rx.borrow_and_update().clone())?;

        let Some(target) = find_node(&snapshot.root, &node) else {
            let mut ids = Vec::new();
            collect_node_ids(&snapshot.root, &mut ids);
            return Err(McpError::invalid_params(
                format!(
                    "unknown node id `{node}`; valid node ids: {}",
                    ids.join(", ")
                ),
                None,
            ));
        };
        let parsed = parse_action(&action);
        if !target.actions.contains(&parsed) {
            let advertised = if target.actions.is_empty() {
                "none".to_string()
            } else {
                target
                    .actions
                    .iter()
                    .map(action_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(McpError::invalid_params(
                format!(
                    "node `{node}` does not advertise action `{action}`; \
                     advertised actions: {advertised}"
                ),
                None,
            ));
        }

        let input = AgentInput::Act {
            node: NodeId(node),
            action: parsed,
            value,
        };
        self.send_and_report(rx, snapshot, input, 1).await
    }

    /// The `key` tool: raw key fallback, with an optional repeat count.
    #[tool(
        description = "Send a raw key press to the app, e.g. \"q\", \"enter\", \"tab\", \
                       \"ctrl+c\", \"f5\". Pass `repeat` (1-64) to send the same key that many \
                       times, so moving down five rows is one call. For literal text use \
                       type_text instead, which types a whole string in one call. Fallback for \
                       parts of the UI without semantic coverage; prefer act with an advertised \
                       action. A key that does not match the accepted grammar is rejected, with \
                       the grammar in the error. Returns the updated tree once the app reacts."
    )]
    pub async fn key(
        &self,
        Parameters(KeyParams { key, repeat }): Parameters<KeyParams>,
    ) -> Result<CallToolResult, McpError> {
        if key.is_empty() {
            return Err(McpError::invalid_params("key must be non-empty", None));
        }
        // Parse before sending: the app lowers keys with this same parser, so
        // a key it would refuse is refused here, where the agent sees why.
        // Forwarded, it would reach the app's parser, be dropped there, and
        // leave the agent waiting on an effect that can never come.
        if let Err(err) = key.parse::<KeyPress>() {
            return Err(McpError::invalid_params(err.to_string(), None));
        }
        let repeat = repeat.unwrap_or(1);
        if repeat == 0 || repeat > MAX_KEY_REPEAT {
            return Err(McpError::invalid_params(
                format!("repeat must be between 1 and {MAX_KEY_REPEAT}, got {repeat}"),
                None,
            ));
        }
        let mut rx = self.bridge.state_rx.clone();
        // Like `act`, refuse while no app is connected: a key queued now
        // would only be delivered to (and confuse) the *next* app instance.
        let pre = available_snapshot(rx.borrow_and_update().clone())?;
        self.send_and_report(rx, pre, AgentInput::Key { key }, repeat)
            .await
    }

    /// The `type_text` tool: a whole string in one call.
    #[tool(
        description = "Type literal text into the app, as if every character were pressed in \
                       turn. One call instead of one key call per character: a 100-character \
                       string costs 1 call, not 100. This is the tool for filling a text input, \
                       and for any app that scores individual keystrokes. The text goes wherever \
                       the app currently sends typing, so put the target in focus first (act \
                       with focus, or key). Up to 4096 characters. Returns the updated tree once \
                       the app reacts."
    )]
    pub async fn type_text(
        &self,
        Parameters(TypeTextParams { text }): Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, McpError> {
        if text.is_empty() {
            return Err(McpError::invalid_params("text must be non-empty", None));
        }
        let len = text.chars().count();
        if len > MAX_TEXT_CHARS {
            return Err(McpError::invalid_params(
                format!(
                    "text is {len} characters, over the {MAX_TEXT_CHARS} character limit for one \
                     type_text call; send it in smaller pieces"
                ),
                None,
            ));
        }
        let mut rx = self.bridge.state_rx.clone();
        let pre = available_snapshot(rx.borrow_and_update().clone())?;
        self.send_and_report(rx, pre, AgentInput::Text { text }, 1)
            .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TariaMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Parse an agent-supplied action string. Known snake_case names map to the
/// built-in variants; anything else becomes an app-specific [`Action::Custom`].
pub fn parse_action(s: &str) -> Action {
    match s {
        "activate" => Action::Activate,
        "focus" => Action::Focus,
        "select" => Action::Select,
        "toggle" => Action::Toggle,
        "scroll" => Action::Scroll,
        "set_value" => Action::SetValue,
        "dismiss" => Action::Dismiss,
        other => Action::Custom(other.to_string()),
    }
}

/// The string an agent would pass to invoke `action`; inverse of
/// [`parse_action`] for every advertised action.
pub fn action_name(action: &Action) -> String {
    match action {
        Action::Activate => "activate".to_string(),
        Action::Focus => "focus".to_string(),
        Action::Select => "select".to_string(),
        Action::Toggle => "toggle".to_string(),
        Action::Scroll => "scroll".to_string(),
        Action::SetValue => "set_value".to_string(),
        Action::Dismiss => "dismiss".to_string(),
        Action::Custom(name) => name.clone(),
    }
}

/// Extract the live snapshot from a [`BridgeState`], or the error explaining
/// why none is available. The two failure modes call for different next
/// steps, so they get distinct messages: an app that never connected (wrong
/// socket path? not started?) versus an app that connected and then went
/// away (it exited or crashed; waiting for it to come back is enough).
fn available_snapshot(state: BridgeState) -> Result<Snapshot, McpError> {
    match state {
        BridgeState::Connected(snapshot) => Ok(snapshot),
        BridgeState::Never => Err(McpError::internal_error(
            "no snapshot from the app yet - is the taria-enabled app running, and is the socket \
             path correct? The bridge reconnects automatically; retry once the app is up.",
            None,
        )),
        BridgeState::Disconnected {
            app_label,
            last_seq,
        } => Err(McpError::internal_error(
            format!(
                "app '{}' disconnected (last snapshot seq {last_seq}); it may have exited. The \
                 bridge reconnects automatically; retry once the app is back.",
                app_label.as_deref().unwrap_or("unknown")
            ),
            None,
        )),
    }
}

/// Render a snapshot as the standard tool result: compact JSON text.
fn tree_result(snapshot: &Snapshot) -> Result<CallToolResult, McpError> {
    Ok(text_result(snapshot_json(snapshot)?))
}

/// One text block, the shape every result of this server takes.
fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Serialize a snapshot as the compact JSON agents read the tree from.
fn snapshot_json(snapshot: &Snapshot) -> Result<String, McpError> {
    serde_json::to_string(snapshot).map_err(|err| {
        McpError::internal_error(format!("failed to serialize snapshot: {err}"), None)
    })
}

/// What the app said about one input inside [`UPDATE_WAIT`].
#[derive(Debug, Default)]
struct Observed {
    /// Newest ack naming that input, if the app sent one at all.
    status: Option<InputStatus>,
    /// Newest snapshot differing from the one the input was aimed at.
    changed: Option<Snapshot>,
}

/// Watch the app's acks and snapshots for up to [`UPDATE_WAIT`], and report
/// what arrived about the input sent as `id`.
///
/// Both signals are needed, and neither is sufficient: an ack says the app saw
/// the input but not what it did, a new tree says something happened but not
/// that this input caused it. Waiting on both, and on nothing else, is what
/// separates "applied", "ignored", "dropped" and "no reaction".
///
/// The window is not cut short by a snapshot that arrives without an ack: an
/// app that acks could still be about to report this very input dropped, and
/// answering "here is your new tree" to a dropped input is the misreport this
/// whole path exists to prevent. An app that never acks pays the full window
/// and gets its tree at the end of it.
///
/// "Differs" is a full comparison against `pre` rather than a `seq > pre.seq`
/// check: after an app restart the fresh instance's `seq` starts over at 1, so
/// a lower (or equal) `seq` with different content is still a change. Watching
/// the state pass through a non-`Connected` value (the bridge's disconnect
/// marker) also counts as a change, because the next snapshot then comes from
/// a fresh app instance.
async fn observe(
    mut acks: broadcast::Receiver<(InputId, InputStatus)>,
    mut rx: watch::Receiver<BridgeState>,
    pre: &Snapshot,
    id: InputId,
) -> Observed {
    let mut seen = Observed::default();
    let mut reconnected = false;
    let mut acks_open = true;
    let mut state_open = true;
    let deadline = tokio::time::sleep(UPDATE_WAIT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return seen,
            ack = acks.recv(), if acks_open => match ack {
                Ok((acked, status)) => {
                    // An ack for another id says nothing about this input,
                    // and needs no bookkeeping beyond being skipped.
                    if acked != id {
                        continue;
                    }
                    seen.status = Some(status);
                    match status {
                        // Both are final verdicts: the app looked at the
                        // input and is done with it.
                        InputStatus::Dropped | InputStatus::Ignored => return seen,
                        // Delivered can still be refined to Ignored, so keep
                        // listening unless the tree already answered.
                        InputStatus::Delivered => if seen.changed.is_some() {
                            return seen;
                        },
                    }
                }
                // Lagging drops the oldest acks, which are the ones for
                // inputs already answered; the newest keep arriving.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => acks_open = false,
            },
            changed = rx.changed(), if state_open => {
                if changed.is_err() {
                    state_open = false;
                    continue;
                }
                match rx.borrow_and_update().clone() {
                    BridgeState::Never | BridgeState::Disconnected { .. } => reconnected = true,
                    BridgeState::Connected(snapshot) => {
                        if reconnected || snapshot != *pre {
                            seen.changed = Some(snapshot);
                            if seen.status.is_some() {
                                return seen;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Turn what was observed into the tool's answer.
///
/// `pre` is the tree the input was aimed at, used only when the app ignored
/// the input and published nothing newer.
fn report(seen: Observed, pre: Snapshot) -> Result<CallToolResult, McpError> {
    match (seen.status, seen.changed) {
        (Some(InputStatus::Dropped), _) => Err(McpError::internal_error(
            "the app dropped this input because its input queue was full, so nothing was \
             applied. Send fewer inputs, or wait for each call to return before sending the \
             next; slower input gets through.",
            None,
        )),
        (Some(InputStatus::Ignored), changed) => {
            let snapshot = changed.unwrap_or(pre);
            Ok(text_result(format!(
                "The app received this input and deliberately did nothing with it (for example \
                 an action a modal dialog blocks, or a node it no longer knows). Re-plan from \
                 the current tree below.\n{}",
                snapshot_json(&snapshot)?
            )))
        }
        (Some(InputStatus::Delivered), Some(snapshot)) => tree_result(&snapshot),
        (Some(InputStatus::Delivered), None) => Ok(text_result(format!(
            "The app received this input, and its tree did not change within {}ms. Call \
             read_tree if you expect a delayed effect.",
            UPDATE_WAIT.as_millis()
        ))),
        // No ack at all is how an adapter that predates acks behaves. Its
        // changed tree is the only answer it can give, and answer enough.
        (None, Some(snapshot)) => tree_result(&snapshot),
        (None, None) => Ok(text_result(format!(
            "The app neither acknowledged this input nor changed its tree within {}ms. It may \
             not report acknowledgements, or it may not have reacted yet; call read_tree to \
             re-check.",
            UPDATE_WAIT.as_millis()
        ))),
    }
}

/// Depth-first search for a node by id.
fn find_node<'a>(node: &'a Node, id: &str) -> Option<&'a Node> {
    if node.id.0 == id {
        return Some(node);
    }
    node.children.iter().find_map(|child| find_node(child, id))
}

/// Collect every node id in the tree, depth-first, for error messages.
fn collect_node_ids(node: &Node, out: &mut Vec<String>) {
    out.push(node.id.0.clone());
    for child in &node.children {
        collect_node_ids(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taria::Role;

    #[test]
    fn known_action_names_parse_to_builtin_variants() {
        let cases = [
            ("activate", Action::Activate),
            ("focus", Action::Focus),
            ("select", Action::Select),
            ("toggle", Action::Toggle),
            ("scroll", Action::Scroll),
            ("set_value", Action::SetValue),
            ("dismiss", Action::Dismiss),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_action(name), expected, "name: {name}");
        }
    }

    #[test]
    fn unknown_action_names_become_custom() {
        assert_eq!(
            parse_action("archive_task"),
            Action::Custom("archive_task".to_string())
        );
        // Case-sensitive: the wire format is snake_case.
        assert_eq!(
            parse_action("Activate"),
            Action::Custom("Activate".to_string())
        );
    }

    #[test]
    fn action_name_is_the_inverse_of_parse_action() {
        let actions = [
            Action::Activate,
            Action::Focus,
            Action::Select,
            Action::Toggle,
            Action::Scroll,
            Action::SetValue,
            Action::Dismiss,
            Action::Custom("archive".to_string()),
        ];
        for action in actions {
            assert_eq!(parse_action(&action_name(&action)), action);
        }
    }

    #[test]
    fn find_node_searches_depth_first_and_collects_all_ids() {
        let root = Node::new("app", Role::App).child(
            Node::new("list", Role::List)
                .child(Node::new("item-1", Role::ListItem))
                .child(Node::new("item-2", Role::ListItem)),
        );
        assert!(find_node(&root, "item-2").is_some());
        assert!(find_node(&root, "missing").is_none());

        let mut ids = Vec::new();
        collect_node_ids(&root, &mut ids);
        assert_eq!(ids, vec!["app", "list", "item-1", "item-2"]);
    }
}
