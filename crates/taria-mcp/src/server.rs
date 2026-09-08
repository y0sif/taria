//! The MCP tool surface: `read_tree`, `act`, `key`, and `type_text`.

use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use taria::key::KeyPress;
use taria::wire::{InputId, InputStatus};
use taria::{Action, AgentInput, MAX_NODE_DEPTH, Node, NodeId, PROTOCOL_VERSION};
use tokio::sync::{broadcast, watch};

use crate::bridge::{AppSnapshot, BridgeHandle, BridgeState};

/// How long an input-sending tool waits for the app to answer, counting both
/// the app's ack and any snapshot it publishes in response.
const UPDATE_WAIT: Duration = Duration::from_millis(500);

/// How long one input waits for room in the bridge's queue to the app.
///
/// The queue only backs up while the app is not draining its socket, so an
/// unbounded send parks the whole tool call until the app comes back, with
/// nothing said to the agent meanwhile. Bounding it turns an indefinite hang
/// into an error naming the cause.
const QUEUE_WAIT: Duration = Duration::from_millis(500);

/// Most presses one `key` call may send.
///
/// The point of `repeat` is to spend one call on "move down five rows", not to
/// hand an agent a way to fill the app's input queue from a single call; the
/// app drops what overflows it. Visible to the bridge because the ack channel
/// has to hold the acks a maximum burst draws.
pub(crate) const MAX_KEY_REPEAT: u32 = 64;

/// Longest string one `type_text` call may carry, in characters.
///
/// A bounded payload keeps one call from monopolizing the app's input queue:
/// the adapter lowers the text into one key event per character.
const MAX_TEXT_CHARS: usize = 4096;

/// Longest `value` one `act` call may carry, in characters.
///
/// The same number as [`MAX_TEXT_CHARS`], because it answers the same
/// question, how much text one call may push into the app, and an agent that
/// learns one limit should not then meet a second. Generous for the text
/// fields `set_value` addresses, and short enough that the value cannot be
/// used as a weapon: it travels as one ndjson line, and both peers treat a
/// line over 1 MiB as a broken connection, so an unbounded value was a way to
/// cut an app off from its bridge. Bounded here rather than only in apps
/// because every app would otherwise have to defend itself.
const MAX_VALUE_CHARS: usize = MAX_TEXT_CHARS;

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
    /// Value for actions that take one (e.g. the text for "set_value"), up to
    /// 4096 characters.
    #[schemars(length(max = 4096))]
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

    /// Send `input` to the app `repeat` times and report what became of the
    /// burst.
    ///
    /// Every copy carries its own [`InputId`], but they are the same input, so
    /// the last id is the one the tree answer hangs on: the app works through
    /// them in order, and its answer to the last covers the ones before it.
    /// Every id in the burst is still watched, because a drop partway through
    /// is invisible in the last input's ack and would otherwise be reported as
    /// plain success.
    ///
    /// A send that fails partway is reported the same way, and for the same
    /// reason: the copies already handed over may have taken effect, so the
    /// call is neither the failure the send error describes on its own nor a
    /// success.
    ///
    /// `repeat` of 0 still sends once. The count is validated by the caller,
    /// and sending nothing while reporting on an input that was never sent
    /// would be the worse failure.
    async fn send_and_report(
        &self,
        rx: watch::Receiver<BridgeState>,
        pre: AppSnapshot,
        input: AgentInput,
        repeat: u32,
    ) -> Result<CallToolResult, McpError> {
        self.check_protocol()?;
        // Subscribe before anything is sent: an ack published before this
        // point is one the receiver would never see.
        let acks = self.bridge.subscribe_acks();
        // Same, for lines the bridge could not read. Marked seen now so that
        // only a rejection from this call's own window is reported against
        // this input.
        let mut rejected = self.bridge.rejected_rx.clone();
        rejected.borrow_and_update();
        let wanted = repeat.max(1) as usize;
        let mut ids = Vec::with_capacity(wanted);
        // Carried past `observe` rather than returned on the spot: failing out
        // of the loop would tell the agent "this input was not sent" while the
        // copies before it are already on the wire, and invite a retry of a
        // burst that partly landed.
        let mut cut_short = false;
        for _ in 0..wanted {
            match self.send_input(input.clone()).await {
                Ok(id) => ids.push(id),
                Err(err) => {
                    if ids.is_empty() {
                        // Nothing reached the app, so the send error is the
                        // whole truth and already says so.
                        return Err(err);
                    }
                    tracing::warn!(
                        sent = ids.len(),
                        wanted,
                        err = %err.message,
                        "input burst cut short; part of it is already on the wire"
                    );
                    cut_short = true;
                    break;
                }
            }
        }
        let mut seen = observe(acks, rx, &pre, &ids).await;
        if rejected.has_changed().unwrap_or(false) {
            seen.rejected = rejected.borrow_and_update().clone();
        }
        if cut_short {
            return Err(partial_send_error(&seen, ids.len(), wanted));
        }
        // The answer to an ignored input is a tree, and the freshest one the
        // bridge holds beats the one the input was aimed at: the frame that
        // caused the ignore can land after the ack that reports it.
        let fallback = match self.bridge.state_rx.borrow().clone() {
            BridgeState::Connected(snapshot) => snapshot,
            BridgeState::Never | BridgeState::Disconnected { .. } => pre,
        };
        report(seen, fallback, ids.len())
    }

    /// Refuse to send input to a peer on another protocol version.
    ///
    /// Such a peer cannot parse the `Input` messages this bridge writes, so
    /// every input it receives is skipped on its side. Warning and sending
    /// anyway leaves the agent reading "it may not have reacted yet" for a
    /// session that can never react.
    fn check_protocol(&self) -> Result<(), McpError> {
        let Some(app_version) = *self.bridge.protocol_rx.borrow() else {
            return Ok(());
        };
        if app_version == PROTOCOL_VERSION {
            return Ok(());
        }
        Err(McpError::internal_error(
            format!(
                "the app speaks taria protocol version {app_version}, this bridge speaks \
                 {PROTOCOL_VERSION}: the app cannot parse input from this bridge, so nothing was \
                 sent and no input tool will work against it. Match the app's taria dependency to \
                 the bridge's version. read_tree keeps working for as long as the app's snapshots \
                 still parse, which a mismatch does not guarantee."
            ),
            None,
        ))
    }

    /// Hand one input to the socket task, returning the id it was sent under.
    async fn send_input(&self, input: AgentInput) -> Result<InputId, McpError> {
        let id = self.bridge.next_input_id();
        let send = self.bridge.input_tx.send((id, input));
        match tokio::time::timeout(QUEUE_WAIT, send).await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(_)) => Err(McpError::internal_error(
                "failed to forward input: the bridge connection task is gone",
                None,
            )),
            Err(_) => Err(McpError::internal_error(
                format!(
                    "the app is not accepting input: the bridge's queue to it stayed full for \
                     {}ms, so this input was not sent. The app is stopped or not reading its \
                     socket; retry once it is responsive.",
                    QUEUE_WAIT.as_millis()
                ),
                None,
            )),
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
        let snapshot = available_snapshot(
            self.bridge.state_rx.borrow().clone(),
            self.bridge.rejected_rx.borrow().clone(),
        )?;
        tree_result(&snapshot)
    }

    /// The `act` tool: validate and forward an advertised action.
    #[tool(
        description = "Invoke an advertised action on a node of the app's semantic tree. `node` \
                       is a node id and `action` an action name, both taken from read_tree; pass \
                       `value` for actions that need one (e.g. set_value), up to 4096 \
                       characters. Returns the updated tree once the app reacts."
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
        // Bounded before anything is sent, for the reason on
        // [`MAX_VALUE_CHARS`]: an oversized value is a line neither peer will
        // read, so it would break the connection instead of setting a value.
        if let Some(len) = value.as_ref().map(|value| value.chars().count())
            && len > MAX_VALUE_CHARS
        {
            return Err(McpError::invalid_params(
                format!(
                    "value is {len} characters, over the {MAX_VALUE_CHARS} character limit for \
                     one act call; set a shorter value"
                ),
                None,
            ));
        }
        let mut rx = self.bridge.state_rx.clone();
        let snapshot = available_snapshot(
            rx.borrow_and_update().clone(),
            self.bridge.rejected_rx.borrow().clone(),
        )?;

        let Some(target) = find_node(&snapshot.parsed.root, &node) else {
            let mut ids = Vec::new();
            collect_node_ids(&snapshot.parsed.root, &mut ids);
            return Err(McpError::invalid_params(
                format!(
                    "unknown node id `{node}`; valid node ids: {}",
                    ids.join(", ")
                ),
                None,
            ));
        };
        let Some(parsed) = resolve_action(&action, target) else {
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
        };

        let input = AgentInput::act(NodeId(node), parsed, value);
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
        let pre = available_snapshot(
            rx.borrow_and_update().clone(),
            self.bridge.rejected_rx.borrow().clone(),
        )?;
        self.send_and_report(rx, pre, AgentInput::key(key), repeat)
            .await
    }

    /// The `type_text` tool: a whole string in one call.
    #[tool(
        description = "Type literal text into the app, as if every character were pressed in \
                       turn. One call instead of one key call per character: a 100-character \
                       string costs 1 call, not 100. This is the tool for filling a text input, \
                       and for any app that scores individual keystrokes. The text goes wherever \
                       the app currently sends typing: read_tree names the focused node, so move \
                       focus to the target first if it is not already there. Up to 4096 \
                       characters. Returns the updated tree once the app reacts."
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
        let pre = available_snapshot(
            rx.borrow_and_update().clone(),
            self.bridge.rejected_rx.borrow().clone(),
        )?;
        self.send_and_report(rx, pre, AgentInput::text(text), 1)
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

/// Pick the [`Action`] to send for the agent-supplied name `action`, or
/// `None` when `target` advertises neither form of it.
///
/// A name that maps to a built-in variant normally sends that variant, but a
/// node is free to advertise [`Action::Custom`] under a built-in's name, and
/// the tree spells both exactly the same way (see [`action_name`]). Without
/// the fallback such a node is unreachable through the only name it ever
/// showed, and says so in an error that contradicts itself: "does not
/// advertise action `activate`; advertised actions: activate".
fn resolve_action(action: &str, target: &Node) -> Option<Action> {
    let parsed = parse_action(action);
    if target.actions.contains(&parsed) {
        return Some(parsed);
    }
    let custom = Action::Custom(action.to_string());
    if target.actions.contains(&custom) {
        return Some(custom);
    }
    // An action taria promoted to a built-in after `parse_action` was written:
    // the tree spells it as that built-in, `parse_action` still reads the name
    // as `Custom`, and neither lookup above can match. Matching on the name
    // `action_name` prints for it closes that gap, so an action a node
    // advertises stays invocable through the only name it ever showed.
    target
        .actions
        .iter()
        .find(|advertised| action_name(advertised) == action)
        .cloned()
}

/// The string an agent would pass to invoke `action`; inverse of
/// [`parse_action`] for every advertised action.
///
/// An action taria promoted to a built-in after this bridge was written has no
/// arm below, and a placeholder there would print a name no agent could
/// invoke. The derived `Serialize` still holds that action's real wire name, so
/// the fallback reads it back from there; only a future variant carrying a
/// payload (as [`Action::Custom`] does) fails that and falls through to the
/// debug form, which at least names the variant.
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
        other => match serde_json::to_value(other) {
            Ok(serde_json::Value::String(name)) => name,
            _ => format!("{other:?}"),
        },
    }
}

/// Extract the live snapshot from a [`BridgeState`], or the error explaining
/// why none is available. The failure modes call for different next steps, so
/// they get distinct messages: an app that never connected (wrong socket
/// path? not started?), an app that connected and then went away (it exited
/// or crashed; waiting for it to come back is enough), and an app that
/// reached the socket and sent lines this bridge threw away.
///
/// `rejected` is [`BridgeHandle::rejected_rx`](crate::bridge::BridgeHandle),
/// and it is what separates the first case from the third. Both leave the
/// state at [`BridgeState::Never`], but only one of them is worth checking a
/// socket path over: an app whose lines were rejected demonstrably found the
/// socket. Sending an adopter to re-check the path there is the first wrong
/// turn a retrofit takes, and the bridge knew the real reason all along.
fn available_snapshot(
    state: BridgeState,
    rejected: Option<String>,
) -> Result<AppSnapshot, McpError> {
    match state {
        BridgeState::Connected(snapshot) => Ok(snapshot),
        BridgeState::Never => Err(McpError::internal_error(
            match rejected {
                Some(err) => format!(
                    "no snapshot from the app yet, but not because nothing is there: an app \
                     reached this socket and sent lines, and this bridge could not read the \
                     last one ({err}). The socket path is right, so the line is what is wrong. \
                     A snapshot more than {MAX_NODE_DEPTH} levels deep exceeds what a JSON \
                     parser will recurse into and fails exactly like this; so does an app built \
                     against a taria whose snapshot shape differs from this bridge's. Fix the \
                     tree the app publishes, or match its taria dependency to this bridge's; \
                     the bridge keeps reading and needs no restart.",
                ),
                None => "no snapshot from the app yet - is the taria-enabled app running, and \
                     is the socket path correct? The bridge reconnects automatically; retry \
                     once the app is up."
                    .to_string(),
            },
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

/// Render a snapshot as the standard tool result: the app's own JSON.
fn tree_result(snapshot: &AppSnapshot) -> Result<CallToolResult, McpError> {
    Ok(text_result(snapshot_json(snapshot)))
}

/// One text block, the shape every result of this server takes.
fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// The JSON agents read the tree from: the app's own snapshot, relayed.
///
/// Not `serde_json::to_string(&snapshot.parsed)`. Round-tripping through this
/// build's types is what turned an app's `"role":"sparkline"` into
/// `"role":"other"` on the way to the agent, and would do the same to every
/// field a newer app adds. Two things follow from relaying instead. The
/// vocabulary is no longer this bridge's, so a role, an action or a field it
/// has never heard of reaches the agent by name. And there is nothing left to
/// fail, so the result no longer carries a serialization error no peer could
/// have provoked.
///
/// What the agent gets is the tree, not the message carrying it: the
/// `"type":"snapshot"` key that frames the line on the wire is dropped where
/// the snapshot is stored (see [`AppSnapshot::from_line`]), because this tool
/// promises the app's tree and that key was never part of one.
fn snapshot_json(snapshot: &AppSnapshot) -> String {
    snapshot.tree_json.clone()
}

/// What the app said about one burst of inputs inside [`UPDATE_WAIT`].
#[derive(Debug, Default)]
struct Observed {
    /// Newest ack naming the last input of the burst, if the app sent one.
    status: Option<InputStatus>,
    /// How many inputs of the burst the app's newest ack for each calls
    /// [`Dropped`](InputStatus::Dropped).
    ///
    /// Per input rather than per ack, so it can never exceed the burst.
    /// [`InputStatus`] lets an app ack one input more than once with the last
    /// ack winning, so counting acks let a conforming app report more drops
    /// than there were inputs, and the `sent - dropped` below then panicked
    /// the bridge mid-call: the tool never answered and the agent waited on
    /// it forever.
    dropped: usize,
    /// How many acks were published while this call was in flight and never
    /// read, because the observer fell behind the broadcast channel.
    ///
    /// Any of them could have been a [`Dropped`](InputStatus::Dropped) for one
    /// of this burst's ids, so a nonzero count makes [`dropped`](Self::dropped)
    /// a floor rather than a count, and the burst's fate only partly known.
    lost: u64,
    /// Newest snapshot differing from the one the input was aimed at.
    changed: Option<AppSnapshot>,
    /// The state the bridge was left in when the app went away and had not
    /// come back by the end of the window.
    ///
    /// An input can be the reason the app is gone (a quit key, a crash it
    /// triggered), and every other verdict here describes an app that is
    /// still there, so this one has to outrank them.
    gone: Option<BridgeState>,
    /// Why a line the app sent inside the window was thrown away, if one was.
    ///
    /// Not filled in by [`observe`], which watches acks and trees; a rejected
    /// line produced neither, which is the point. An ack carrying a status
    /// this build cannot name fails its whole line, id and all, so the app
    /// answering an input is indistinguishable here from the app saying
    /// nothing. This is the one trace that is left of the difference.
    rejected: Option<String>,
}

/// Watch the app's acks and snapshots for up to [`UPDATE_WAIT`], and report
/// what arrived about the inputs sent as `ids`.
///
/// Both signals are needed, and neither is sufficient: an ack says the app saw
/// the input but not what it did, a new tree says something happened but not
/// that this input caused it. Waiting on both, and on nothing else, is what
/// separates "applied", "ignored", "dropped" and "no reaction".
///
/// The last id drives the verdict, and the rest are watched only for drops:
/// a burst whose middle presses overflowed the app's queue reads as success
/// in the last press's ack alone, which is the misreport [`Observed::dropped`]
/// exists to catch.
///
/// The window is not cut short by a snapshot that arrives without an ack: an
/// app that acks could still be about to report this very input dropped, and
/// answering "here is your new tree" to a dropped input is the misreport this
/// whole path exists to prevent. An app that never acks pays the full window
/// and gets its tree at the end of it. An `Ignored` ack does not cut it short
/// either: the adapter flushes queued acks ahead of the pending snapshot in
/// every writer pass, so the frame that caused the ignore arrives *after* the
/// ack that reports it, and returning at the ack hands back a tree that
/// predates the state the agent is told to re-plan from.
///
/// "Differs" is a full comparison against `pre` rather than a `seq > pre.seq`
/// check: after an app restart the fresh instance's `seq` starts over at 1, so
/// a lower (or equal) `seq` with different content is still a change. Watching
/// the state pass through a non-`Connected` value (the bridge's disconnect
/// marker) also counts as a change, because the next snapshot then comes from
/// a fresh app instance.
///
/// A departure the window ends on is kept in [`Observed::gone`] instead: the
/// app that received this input is not there any more, and no ack or tree
/// answers that. An app that comes back inside the window clears it, because
/// its tree is a real answer and the better one.
async fn observe(
    mut acks: broadcast::Receiver<(InputId, InputStatus)>,
    mut rx: watch::Receiver<BridgeState>,
    pre: &AppSnapshot,
    ids: &[InputId],
) -> Observed {
    let mut seen = Observed::default();
    let mut reconnected = false;
    let mut acks_open = true;
    let mut state_open = true;
    let last = ids.last().copied();
    // The newest status per input of the burst, which is what "the last ack
    // wins" means for the drop tally: a re-ack replaces an input's fate
    // instead of adding to a running count.
    let mut fates: Vec<Option<InputStatus>> = vec![None; ids.len()];
    let deadline = tokio::time::sleep(UPDATE_WAIT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            // Biased so the branches run in wire order: the app writes an ack
            // before the snapshot that follows it, and a random pick would let
            // the snapshot land first, making a `Delivered` still awaiting its
            // `Ignored` refinement look like the final word. The deadline goes
            // first of all, so a stream of acks cannot starve the timeout.
            biased;
            () = &mut deadline => return seen,
            ack = acks.recv(), if acks_open => match ack {
                Ok((acked, status)) => {
                    // An ack for another call's input says nothing about this
                    // burst, and needs no bookkeeping beyond being skipped.
                    let Some(pos) = ids.iter().position(|id| *id == acked) else {
                        continue;
                    };
                    // Record this input's newest fate and recount, rather than
                    // adding to a tally: the wire format permits a second ack
                    // for the same input, so a running count is a count of
                    // acks, not of inputs, and can outgrow the burst.
                    fates[pos] = Some(status);
                    seen.dropped = fates
                        .iter()
                        .filter(|fate| **fate == Some(InputStatus::Dropped))
                        .count();
                    // Every id counts towards the drop tally, but only the
                    // last one decides when there is nothing left to wait for.
                    if Some(acked) != last {
                        continue;
                    }
                    seen.status = Some(status);
                    // A dropped input carries no tree and nothing later can
                    // undo the drop: the verdict is final.
                    if status == InputStatus::Dropped {
                        return seen;
                    }
                    // Delivered can still be refined to Ignored, and an ignored
                    // input still owes a tree, so keep listening unless the
                    // tree already answered. A status added to the protocol
                    // after this bridge was written waits here too, rather than
                    // ending the window: it is an answer this build cannot
                    // read, and the tree is the only part of it still legible.
                    // A test on `Dropped` rather than a match with a wildcard,
                    // so an unreadable status cannot fall into the arm that
                    // declares a verdict final.
                    if seen.changed.is_some() {
                        return seen;
                    }
                }
                // Lagging drops the oldest acks, and the oldest are this
                // burst's earliest presses: the very ones whose `Dropped`
                // answers only the tally would ever show. Counted rather
                // than shrugged off, so the report can say the observation
                // is incomplete instead of under-counting the drops.
                Err(broadcast::error::RecvError::Lagged(skipped)) => seen.lost += skipped,
                Err(broadcast::error::RecvError::Closed) => acks_open = false,
            },
            changed = rx.changed(), if state_open => {
                if changed.is_err() {
                    state_open = false;
                    continue;
                }
                match rx.borrow_and_update().clone() {
                    state @ (BridgeState::Never | BridgeState::Disconnected { .. }) => {
                        reconnected = true;
                        seen.gone = Some(state);
                    }
                    BridgeState::Connected(snapshot) => {
                        seen.gone = None;
                        // Compared on the parse, not on the line the agent
                        // reads: two lines spelling the same tree differently
                        // are not a change the agent needs to hear about.
                        if reconnected || snapshot.parsed != pre.parsed {
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
/// `fallback` is the freshest tree the bridge holds, used only when the app
/// ignored the input and published nothing newer. `sent` is how many inputs
/// the burst carried, which the two partial answers need: a drop somewhere in
/// the burst, and an observation with acks missing from it.
fn report(seen: Observed, fallback: AppSnapshot, sent: usize) -> Result<CallToolResult, McpError> {
    // Acks this call never read could have been its own `Dropped` answers, so
    // none of the verdicts below can be stood behind: the drop tally is a
    // floor, and a clean tree would claim an intact burst nobody watched.
    if seen.lost > 0 {
        let known = if seen.dropped > 0 {
            format!(
                "at least {} of them {} dropped because the app's input queue was full, and the \
                 rest may or may not have been applied",
                seen.dropped,
                was_were(seen.dropped)
            )
        } else {
            "they may or may not have been applied".to_string()
        };
        return Err(McpError::internal_error(
            format!(
                "the bridge lost {} of the app's acknowledgements while this call was in flight, \
                 so what became of the {} it sent cannot be reported in full: {known}. Call \
                 read_tree to see what actually landed, and send fewer inputs per call so the \
                 answers can be read as fast as they arrive.",
                seen.lost,
                inputs(sent)
            ),
            None,
        ));
    }
    // A drop anywhere in a burst is a partial failure of the whole call, even
    // when the last press landed and the tree changed: reporting the tree
    // would tell the agent the burst arrived intact.
    if seen.dropped > 0 && sent > 1 {
        // Saturating even though the tally is now per input and so cannot
        // exceed `sent`: this subtraction is fed by what a peer said, and a
        // report is no place to panic. Overflowing it took the whole tool
        // call down with it, leaving the agent waiting on a reply that could
        // never come.
        let landed = sent.saturating_sub(seen.dropped);
        return Err(McpError::internal_error(
            format!(
                "the app dropped {} of the {sent} inputs this call sent because its input queue \
                 was full, so at most {landed} landed and the effect is partial. Send fewer \
                 inputs, or wait for each call to return before sending the next; slower input \
                 gets through.",
                seen.dropped
            ),
            None,
        ));
    }
    // The app this call was talking to is gone, and none of the verdicts
    // below can say so: a tree would describe a UI that no longer exists, and
    // the two no-answer notes would put it down to an app that has not
    // reacted yet. A drop still outranks this, here and in the burst report
    // above: "the app never applied this input" is a fact about the input
    // that the app's exit does not change, and it would contradict the
    // "possibly because of this input" below.
    if let Some(state) = seen.gone
        && seen.status != Some(InputStatus::Dropped)
    {
        // Said the way `read_tree` says it about the same condition, so an
        // agent meets one vocabulary for a missing app, not two.
        let lead = if seen.status.is_some() {
            "the app received this input and then disconnected"
        } else {
            "the app disconnected before acknowledging this input"
        };
        return Err(McpError::internal_error(
            format!(
                "{lead}: {} and may have exited, possibly because of this input. The bridge \
                 reconnects automatically; retry once the app is back.",
                gone_app(&state)
            ),
            None,
        ));
    }
    match (seen.status, seen.changed) {
        (Some(InputStatus::Dropped), _) => Err(McpError::internal_error(
            "the app dropped this input because its input queue was full, so nothing was \
             applied. Send fewer inputs, or wait for each call to return before sending the \
             next; slower input gets through.",
            None,
        )),
        (Some(InputStatus::Ignored), changed) => {
            let snapshot = changed.unwrap_or(fallback);
            Ok(text_result(format!(
                "The app received this input and deliberately did nothing with it (for example \
                 an action a modal dialog blocks, or a node it no longer knows). Re-plan from \
                 the current tree below.\n{}",
                snapshot_json(&snapshot)
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
        // Silence, with the one thing that can turn out not to be silence
        // at all: a line the app sent in this window that the bridge threw
        // away. An ack whose status this build cannot name is exactly that,
        // and it takes the id down with it, so the answer to this input can
        // be sitting in `rejected` with nothing left to match it to.
        (None, None) => Ok(text_result(match seen.rejected {
            Some(err) => format!(
                "The app neither acknowledged this input nor changed its tree within {}ms, but \
                 it did send a line this bridge could not read ({err}). That line may have been \
                 this input's acknowledgement: an acknowledgement whose status this bridge does \
                 not know fails as a whole and takes the input id with it, so it arrives here \
                 as silence. If it was, matching the app's taria dependency to this bridge's \
                 makes it readable; call read_tree for the current tree either way.",
                UPDATE_WAIT.as_millis()
            ),
            None => format!(
                "The app neither acknowledged this input nor changed its tree within {}ms. It \
                 may not report acknowledgements, or it may not have reacted yet; call \
                 read_tree to re-check.",
                UPDATE_WAIT.as_millis()
            ),
        })),
        // A status this build can decode but has no case for above.
        //
        // Required, because [`InputStatus`] is `#[non_exhaustive]`, and not
        // reachable from a newer *app*: a status name this build does not
        // know fails the whole `Ack` line, so the ack never arrives and the
        // input reads as unacknowledged in the arms above, which is where the
        // `rejected` note exists to explain it. What reaches here is the
        // other direction, this bridge rebuilt against a taria that added a
        // status while this match was left alone. That makes the arm a guard
        // on the bridge's own upgrade rather than on the app's, and the fix
        // it names has to be the bridge.
        //
        // None of the four verdicts can be claimed for such a status: being
        // unable to read an answer is not a licence to guess `Delivered`.
        // Said plainly, with the freshest tree, because the agent can still
        // re-plan from a tree and cannot re-plan from an error.
        (Some(_), changed) => {
            let snapshot = changed.unwrap_or(fallback);
            Ok(text_result(format!(
                "The app acknowledged this input with a status this bridge has no report for, \
                 so whether it was applied cannot be stated. This bridge is built against a \
                 taria newer than its own handling of acknowledgements; upgrading the bridge \
                 restores the full report. The current tree follows.\n{}",
                snapshot_json(&snapshot)
            )))
        }
    }
}

/// The error for a burst the bridge could not finish sending.
///
/// The `sent` inputs that did go out are on the wire and may already have been
/// applied, so neither of the two answers this call could otherwise give is
/// true: "this input was not sent" (what the send error says on its own) hides
/// the part that landed, and a tree claims a burst that arrived intact. The
/// agent needs both counts to know what is left to retry.
fn partial_send_error(seen: &Observed, sent: usize, wanted: usize) -> McpError {
    // Saturating for the same reason as `report`: the counts that reach here
    // are sound, and an error path that can panic is worse than one that
    // under-reports.
    let unsent = wanted.saturating_sub(sent);
    let dropped = if seen.dropped > 0 {
        format!(
            " Of the {sent} sent, the app dropped {} because its input queue was full.",
            seen.dropped
        )
    } else {
        String::new()
    };
    McpError::internal_error(
        format!(
            "the app stopped accepting input partway through this call: {sent} of the {wanted} \
             inputs {} sent and may already have taken effect, and the remaining {unsent} {} not \
             sent, so the effect is partial.{dropped} Call read_tree to see what landed, then \
             retry the rest once the app is responsive.",
            was_were(sent),
            was_were(unsent)
        ),
        None,
    )
}

/// "1 input" or "4 inputs", so a single-input call is not reported with a
/// count and a noun that disagree.
fn inputs(count: usize) -> String {
    if count == 1 {
        format!("{count} input")
    } else {
        format!("{count} inputs")
    }
}

/// The verb agreeing with a subject of `count`, for the same reason.
fn was_were(count: usize) -> &'static str {
    if count == 1 { "was" } else { "were" }
}

/// Name the app the bridge no longer has, in the words
/// [`available_snapshot`] uses for the same state.
fn gone_app(state: &BridgeState) -> String {
    match state {
        BridgeState::Disconnected {
            app_label,
            last_seq,
        } => format!(
            "app '{}' (last snapshot seq {last_seq}) is gone",
            app_label.as_deref().unwrap_or("unknown")
        ),
        // The bridge only ever demotes a connected app to `Disconnected`, and
        // an input tool refuses to send before the first connection, so
        // neither of these can be the state a call was left in. Described
        // without the details rather than asserted away, because a report is
        // no place to panic.
        BridgeState::Never | BridgeState::Connected(_) => "the app is gone".to_string(),
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
    use taria::{Role, Snapshot};

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

    /// The counterexample the inverse above does not cover: a node advertising
    /// `Custom("activate")` spells its action exactly like the built-in, so
    /// the built-in name has to reach it.
    #[test]
    fn a_custom_action_named_like_a_builtin_is_still_reachable() {
        let custom = Action::Custom("activate".to_string());
        let node = Node::new("x", Role::Button).action(custom.clone());
        assert_eq!(resolve_action("activate", &node), Some(custom));

        // The built-in still wins wherever a node advertises it.
        let builtin = Node::new("y", Role::Button).action(Action::Activate);
        assert_eq!(resolve_action("activate", &builtin), Some(Action::Activate));

        // Neither form advertised is still a rejection.
        let neither = Node::new("z", Role::Button).action(Action::Toggle);
        assert_eq!(resolve_action("activate", &neither), None);

        // And a custom name nothing advertises stays a rejection too.
        assert_eq!(resolve_action("archive", &neither), None);
    }

    /// A published snapshot in both forms, built from a line the way an app
    /// would send it: the tests below compare parses, and the results they
    /// read back are what the relay makes of that line.
    fn snapshot(marker: &str) -> AppSnapshot {
        let parsed = Snapshot::new(1, Node::new("app", Role::App).label(marker));
        let line = serde_json::to_string(&taria::wire::AppToBridge::Snapshot(parsed.clone()))
            .expect("a snapshot serializes");
        AppSnapshot::from_line(&line, parsed)
    }

    /// Acks published faster than the observer reads them are dropped oldest
    /// first, and the oldest are this burst's earliest presses. Skipping them
    /// silently is what turns "3 of your 5 presses were dropped" into a clean
    /// success, so they have to be counted and reported.
    #[tokio::test]
    async fn acks_lost_to_lag_are_counted_and_refuse_to_read_as_success() {
        let (ack_tx, acks) = broadcast::channel(2);
        let (_state_tx, rx) = watch::channel(BridgeState::Never);
        let pre = snapshot("before");
        // Four acks into a two-slot channel, all published before the observer
        // reads: the two oldest are gone by its first recv.
        for id in 1..=4 {
            let _ = ack_tx.send((id, InputStatus::Delivered));
        }

        let seen = observe(acks, rx, &pre, &[1, 2, 3, 4]).await;
        assert_eq!(seen.lost, 2, "skipped acks must be counted: {seen:?}");

        let err = report(seen, pre, 4).expect_err("an incomplete observation is not a success");
        assert!(
            err.message.contains("lost 2 of the app's acknowledgements"),
            "error should say how many answers went missing: {}",
            err.message
        );
        assert!(
            err.message.contains("the 4 inputs it sent"),
            "error should say how much of the call is in question: {}",
            err.message
        );
        assert!(
            err.message.contains("read_tree"),
            "error should say how to find out what landed: {}",
            err.message
        );
    }

    /// With acks lost, the drops that were seen are a floor and must not be
    /// reported as the count.
    #[test]
    fn a_drop_seen_beside_a_lost_ack_is_reported_as_a_floor() {
        let seen = Observed {
            status: Some(InputStatus::Delivered),
            dropped: 2,
            lost: 1,
            changed: None,
            gone: None,
            rejected: None,
        };
        let err = report(seen, snapshot("after"), 5).expect_err("lost acks are not a success");
        assert!(
            err.message.contains("at least 2 of them were dropped"),
            "a drop count that cannot be complete must not read as exact: {}",
            err.message
        );
    }

    /// Counts and the words around them have to agree: "the 1 inputs it sent"
    /// reads as a bug in the bridge, in a message whose whole job is to be
    /// trusted about what became of a call.
    #[test]
    fn a_one_input_call_is_reported_in_the_singular() {
        let seen = Observed {
            status: Some(InputStatus::Delivered),
            dropped: 1,
            lost: 1,
            changed: None,
            gone: None,
            rejected: None,
        };
        let err = report(seen, snapshot("after"), 1).expect_err("lost acks are not a success");
        assert!(
            err.message.contains("the 1 input it sent"),
            "a single input is one input: {}",
            err.message
        );
        assert!(
            err.message.contains("at least 1 of them was dropped"),
            "a single drop is not a plural: {}",
            err.message
        );

        // The same agreement in the other message that counts a burst.
        let cut_short = partial_send_error(&Observed::default(), 1, 2);
        assert!(
            cut_short.message.contains("1 of the 2 inputs was sent"),
            "one input sent is singular: {}",
            cut_short.message
        );
        assert!(
            cut_short.message.contains("the remaining 1 was not sent"),
            "one input unsent is singular: {}",
            cut_short.message
        );
    }

    /// [`InputStatus`] lets an app ack one input more than once with the last
    /// ack winning, so a repeated `Dropped` is a conforming app's message.
    /// Counting acks instead of inputs let the tally outgrow the burst, and
    /// the `sent - dropped` in [`report`] then panicked the task serving the
    /// call: the process survived, the tool never answered, and the agent
    /// waited on that call forever.
    #[tokio::test]
    async fn a_duplicated_drop_ack_is_counted_once() {
        let (ack_tx, acks) = broadcast::channel(8);
        let pre = snapshot("before");
        let (_state_tx, rx) = watch::channel(BridgeState::Connected(pre.clone()));
        // A two-press burst, three acks: the first input is dropped and said
        // so twice.
        let _ = ack_tx.send((1, InputStatus::Dropped));
        let _ = ack_tx.send((1, InputStatus::Dropped));
        let _ = ack_tx.send((2, InputStatus::Dropped));

        let seen = observe(acks, rx, &pre, &[1, 2]).await;
        assert_eq!(
            seen.dropped, 2,
            "two inputs were dropped however many acks said so: {seen:?}"
        );

        let err = report(seen, pre, 2).expect_err("a dropped burst is not a success");
        assert!(
            err.message.contains("dropped 2 of the 2 inputs"),
            "the burst report must not claim more drops than inputs: {}",
            err.message
        );
        assert!(
            err.message.contains("at most 0 landed"),
            "the landed count must stay a real count: {}",
            err.message
        );
    }

    /// The other half of "the last ack wins": an input acked `Dropped` and
    /// then `Delivered` is delivered, and must leave the drop tally.
    #[tokio::test]
    async fn a_drop_ack_the_app_takes_back_stops_counting() {
        let (ack_tx, acks) = broadcast::channel(8);
        let pre = snapshot("before");
        let (_state_tx, rx) = watch::channel(BridgeState::Connected(pre.clone()));
        let _ = ack_tx.send((1, InputStatus::Dropped));
        let _ = ack_tx.send((1, InputStatus::Delivered));
        // The last input's `Dropped` ends the window, so this stays fast.
        let _ = ack_tx.send((2, InputStatus::Dropped));

        let seen = observe(acks, rx, &pre, &[1, 2]).await;
        assert_eq!(
            seen.dropped, 1,
            "only the input whose newest ack is a drop counts: {seen:?}"
        );

        let err = report(seen, pre, 2).expect_err("a partial burst is not a success");
        assert!(
            err.message.contains("dropped 1 of the 2 inputs")
                && err.message.contains("at most 1 landed"),
            "the report must follow the newest ack: {}",
            err.message
        );
    }

    /// An input the app does not survive must not read as "nothing happened".
    /// The app is gone, `read_tree` would say so on the next call, and the
    /// tool that sent the input is the first place the agent can hear it.
    #[tokio::test]
    async fn an_app_that_stays_gone_is_reported_gone_rather_than_unreactive() {
        let (ack_tx, acks) = broadcast::channel(4);
        let pre = snapshot("before");
        let (state_tx, rx) = watch::channel(BridgeState::Connected(pre.clone()));
        let _ = ack_tx.send((1, InputStatus::Delivered));
        state_tx.send_replace(BridgeState::Disconnected {
            app_label: Some("demo".to_string()),
            last_seq: 7,
        });

        let seen = observe(acks, rx, &pre, &[1]).await;
        assert!(
            seen.gone.is_some(),
            "a departure must be recorded: {seen:?}"
        );

        let err = report(seen, pre, 1).expect_err("an app that went away is not a success");
        assert!(
            err.message
                .contains("received this input and then disconnected")
                && err
                    .message
                    .contains("app 'demo' (last snapshot seq 7) is gone"),
            "the report must name the app and its departure: {}",
            err.message
        );
        assert!(
            !err.message.contains("did not change"),
            "an app that exited must not be reported as one that ignored the input: {}",
            err.message
        );
    }

    /// The other half of the same window: an app that comes back is a real
    /// answer, and the fresh instance's tree is what the agent needs.
    #[tokio::test]
    async fn an_app_that_comes_back_inside_the_window_answers_with_its_tree() {
        let (ack_tx, acks) = broadcast::channel(4);
        let pre = snapshot("before");
        let (state_tx, rx) = watch::channel(BridgeState::Connected(pre.clone()));
        let _ = ack_tx.send((1, InputStatus::Delivered));
        state_tx.send_replace(BridgeState::Disconnected {
            app_label: Some("demo".to_string()),
            last_seq: 7,
        });
        // The restarted app draws the same tree it drew before, so the gap
        // itself is the only evidence a new instance answered.
        let after = pre.clone();
        tokio::spawn(async move {
            // Long enough for the observer to read the disconnect first, and
            // far short of the window it has to answer in.
            tokio::time::sleep(Duration::from_millis(50)).await;
            state_tx.send_replace(BridgeState::Connected(after));
        });

        let seen = observe(acks, rx, &pre, &[1]).await;
        assert!(
            seen.gone.is_none(),
            "an app that came back is not gone: {seen:?}"
        );
        assert_eq!(
            seen.changed.as_ref(),
            Some(&pre),
            "the fresh instance's tree is the answer: {seen:?}"
        );
    }

    /// The counts an agent needs after a burst that stopped partway: what went
    /// out (and may have taken effect) and what never did.
    #[test]
    fn a_partial_send_names_both_halves_of_the_burst() {
        let err = partial_send_error(&Observed::default(), 2, 5);
        assert!(
            err.message.contains("2 of the 5 inputs were sent"),
            "error should say how much was sent: {}",
            err.message
        );
        assert!(
            err.message.contains("remaining 3 were not sent"),
            "error should say how much was not: {}",
            err.message
        );
        assert!(
            !err.message.contains("this input was not sent"),
            "a partial send must not read like nothing was sent: {}",
            err.message
        );

        // Drops seen among the sent part are named too, since they shrink the
        // part that may have landed.
        let dropped = Observed {
            dropped: 1,
            ..Observed::default()
        };
        assert!(
            partial_send_error(&dropped, 2, 5)
                .message
                .contains("the app dropped 1"),
            "drops in the sent part belong in the report"
        );
    }

    /// A server with no app behind it, which is all the value bound needs:
    /// an oversized value is refused before the bridge is asked for anything.
    fn detached_server() -> TariaMcpServer {
        let (_state_tx, state_rx) = watch::channel(BridgeState::Never);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
        let (ack_tx, _) = broadcast::channel(1);
        let (_protocol_tx, protocol_rx) = watch::channel(None);
        let (_rejected_tx, rejected_rx) = watch::channel(None);
        TariaMcpServer::new(BridgeHandle {
            state_rx,
            input_tx,
            ack_tx,
            protocol_rx,
            rejected_rx,
        })
    }

    fn act_params(value: String) -> Parameters<ActParams> {
        Parameters(ActParams {
            node: "input".to_string(),
            action: "set_value".to_string(),
            value: Some(value),
        })
    }

    /// `act`'s value was the one payload the tool layer left unbounded, while
    /// `key` bounds its repeat and `type_text` its text. It travels as one
    /// ndjson line, and both peers treat a line over 1 MiB as a broken
    /// connection, so an unbounded value was a way to cut an app off from its
    /// bridge with a single legal call.
    #[tokio::test]
    async fn act_refuses_a_value_over_the_limit() {
        let server = detached_server();

        let err = server
            .act(act_params("x".repeat(MAX_VALUE_CHARS + 1)))
            .await
            .expect_err("an oversized value is not a request the bridge can carry");
        assert!(
            err.message.contains(&MAX_VALUE_CHARS.to_string())
                && err.message.contains(&(MAX_VALUE_CHARS + 1).to_string()),
            "the refusal must name the limit and what was sent: {}",
            err.message
        );

        // At the limit the value is not what stops the call: with no app
        // connected it gets as far as asking for a tree, and fails there.
        for value in ["x".repeat(MAX_VALUE_CHARS), "😀".repeat(MAX_VALUE_CHARS)] {
            let err = server
                .act(act_params(value))
                .await
                .expect_err("no app is connected");
            assert!(
                err.message.contains("no snapshot from the app yet"),
                "the limit is a character count, not a byte count: {}",
                err.message
            );
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
