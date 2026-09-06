//! The MCP tool surface: `read_tree`, `act`, and `key`.

use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use taria::{Action, AgentInput, Node, NodeId, Snapshot};
use tokio::sync::{mpsc, watch};

use crate::bridge::BridgeHandle;

/// How long `act`/`key` wait for the app to publish a newer snapshot before
/// reporting that the tree did not change.
const UPDATE_WAIT: Duration = Duration::from_millis(500);

/// Instructions surfaced to the agent on MCP initialize.
const INSTRUCTIONS: &str = "Bridge to a live terminal (TUI) application. Call read_tree first: \
it returns the app's current semantic tree, including every node id and the actions each node \
advertises. Prefer act with a node id and one of that node's advertised actions (pass value for \
set_value); node ids and action names come from the tree, never guess them. key sends a raw key \
press and is only a fallback for parts of the UI without semantic coverage. act and key return \
the updated tree when the app reacts; call read_tree again whenever you need a fresh view.";

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
}

/// MCP server handler bridging tool calls to the socket-manager task.
#[derive(Clone)]
pub struct TariaMcpServer {
    snapshot_rx: watch::Receiver<Option<Snapshot>>,
    input_tx: mpsc::Sender<AgentInput>,
    tool_router: ToolRouter<Self>,
}

impl TariaMcpServer {
    /// Build the handler on top of a spawned bridge.
    pub fn new(handle: BridgeHandle) -> Self {
        Self {
            snapshot_rx: handle.snapshot_rx,
            input_tx: handle.input_tx,
            tool_router: Self::tool_router(),
        }
    }

    /// Forward `input` to the app, then wait up to [`UPDATE_WAIT`] for a
    /// snapshot newer than `pre_seq` and render the tool result.
    async fn send_and_report(
        &self,
        rx: watch::Receiver<Option<Snapshot>>,
        pre_seq: Option<u64>,
        input: AgentInput,
    ) -> Result<CallToolResult, McpError> {
        self.input_tx.send(input).await.map_err(|_| {
            McpError::internal_error(
                "failed to forward input: the bridge connection task is gone",
                None,
            )
        })?;
        match wait_for_change(rx, pre_seq).await {
            Some(snapshot) => tree_result(&snapshot),
            None => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Input sent, but the tree did not change within {}ms. The app may not have \
                 reacted (yet); call read_tree to re-check.",
                UPDATE_WAIT.as_millis()
            ))])),
        }
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
        let snapshot = self
            .snapshot_rx
            .borrow()
            .clone()
            .ok_or_else(not_connected_error)?;
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
        let mut rx = self.snapshot_rx.clone();
        let snapshot = rx
            .borrow_and_update()
            .clone()
            .ok_or_else(not_connected_error)?;

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

        let pre_seq = snapshot.seq;
        let input = AgentInput::Act {
            node: NodeId(node),
            action: parsed,
            value,
        };
        self.send_and_report(rx, Some(pre_seq), input).await
    }

    /// The `key` tool: raw key fallback.
    #[tool(
        description = "Send a raw key press to the app, e.g. \"q\", \"enter\", \"tab\", \
                       \"ctrl+c\". Fallback for parts of the UI without semantic coverage; \
                       prefer act with an advertised action. Returns the updated tree once the \
                       app reacts."
    )]
    pub async fn key(
        &self,
        Parameters(KeyParams { key }): Parameters<KeyParams>,
    ) -> Result<CallToolResult, McpError> {
        if key.is_empty() {
            return Err(McpError::invalid_params("key must be non-empty", None));
        }
        let mut rx = self.snapshot_rx.clone();
        let pre_seq = rx.borrow_and_update().as_ref().map(|s| s.seq);
        self.send_and_report(rx, pre_seq, AgentInput::Key { key })
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

/// Error for tool calls made while no snapshot is available.
fn not_connected_error() -> McpError {
    McpError::internal_error(
        "no snapshot from the app yet - is the taria-enabled app running, and is the socket \
         path correct? The bridge reconnects automatically; retry once the app is up.",
        None,
    )
}

/// Render a snapshot as the standard tool result: compact JSON text.
fn tree_result(snapshot: &Snapshot) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string(snapshot).map_err(|err| {
        McpError::internal_error(format!("failed to serialize snapshot: {err}"), None)
    })?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

/// Wait up to [`UPDATE_WAIT`] for a snapshot with `seq` beyond `pre_seq`
/// (any snapshot at all when `pre_seq` is `None`).
async fn wait_for_change(
    mut rx: watch::Receiver<Option<Snapshot>>,
    pre_seq: Option<u64>,
) -> Option<Snapshot> {
    tokio::time::timeout(UPDATE_WAIT, async move {
        loop {
            if rx.changed().await.is_err() {
                return None;
            }
            let newer = rx
                .borrow_and_update()
                .as_ref()
                .filter(|s| pre_seq.is_none_or(|pre| s.seq > pre))
                .cloned();
            if newer.is_some() {
                return newer;
            }
        }
    })
    .await
    .ok()
    .flatten()
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
