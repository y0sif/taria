//! The MCP tool surface: `read_tree`, `act`, `key`, and `type_text`.
//!
//! A server built with [`TariaMcpServer::new`] or
//! [`TariaMcpServer::with_tree_format`] serves whatever bridge it is handed,
//! which [`bridge::spawn`] connects at once and keeps
//! connected. [`TariaMcpServer::on_demand`], which the `taria-mcp` binary
//! uses, spawns its own bridge that connects on the first tool call and lets
//! go of the app after an idle window, and every tool call then counts as
//! activity and gives a bridge it woke a bounded moment to reach the app.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use taria::key::KeyPress;
use taria::wire::{InputId, InputStatus, MAX_LINE_BYTES};
use taria::{Action, AgentInput, MAX_NODE_DEPTH, Node, NodeId, PROTOCOL_VERSION};
use tokio::sync::{broadcast, watch};

use crate::bridge::{
    self, AppSnapshot, BridgeHandle, BridgeState, Call, Demand, LinkState, OVERSIZED_LINE,
};
use crate::tree_format::TreeFormat;

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

/// How long an open connection may say nothing before the bridge reports it
/// as a peer that is not reading rather than as a handshake still in flight.
///
/// A taria adapter writes its `Hello` as soon as it accepts a bridge, and its
/// listener polls for a new connection every 50 ms, so a connection still
/// silent this long after `connect` returned is not a slow app: it is a
/// connection nobody has accepted, which is what serving one bridge at a time
/// does to the second one, until the first lets go (on its own idle window,
/// for a bridge started on demand) or its session ends. Ten times the
/// adapter's poll, so a loaded machine's first handshake is not diagnosed as
/// a socket somebody else is holding.
///
/// A bridge started on demand connects on the first tool call, and that call
/// waits up to just past this grace for the connection to say something (see
/// [`Call::settle`]), so a held app is diagnosed on the first call rather
/// than the second.
pub(crate) const SILENT_LINK_GRACE: Duration = Duration::from_millis(500);

/// Most presses one `key` call may send.
///
/// The point of `repeat` is to spend one call on "move down five rows", not to
/// hand an agent a way to fill the app's input queue from a single call; the
/// app drops what overflows it. Visible to the bridge because the ack channel
/// has to hold the acks a maximum burst draws.
pub(crate) const MAX_KEY_REPEAT: u32 = 64;

/// Longest string one `key` call may carry, in characters.
///
/// The grammar peels modifier prefixes without a limit, so `"ctrl+"` repeated
/// enough times parses as the key it ends in and serializes to a line past the
/// 1 MiB both peers treat as a broken connection: the app's reader finds no
/// newline within its cap, drops the connection and discards every input still
/// queued on it. One legal call cut the app off from its bridge, which is the
/// hazard `act` and `type_text` already bound their payloads against.
/// `ctrl+alt+shift+backspace`, the longest press this grammar spells, is 24
/// characters, so this leaves room for longer aliases and for named keys the
/// grammar has yet to learn while staying far short of a payload that costs the
/// connection. It is also what makes the peeling loop's cost a constant.
const MAX_KEY_CHARS: usize = 64;

/// Longest string one `type_text` call may carry, in characters.
///
/// A bounded payload keeps one call from costing the app unbounded work. The
/// protocol does not say how an app consumes typed text, but an app that
/// lowers it into key events, the way the reference lowering in
/// `taria-ratatui` does, spends one event per character on it, so the string
/// an agent sends is the size of the burst the app has to absorb.
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
///
/// Short on purpose. The instructions and the tool list are sent again with
/// every model call, so each sentence here is paid for on every step of a
/// session, and the tool descriptions already carry each tool's own rules.
/// What stays is the order of work and the one rule worth saying twice:
/// `type_text`, not `key`, for text. Some clients drop instructions
/// altogether, which is why `key`'s description says it too.
///
/// A preference rather than a "never": a version 1 app is free to route no
/// typed text at all and answer every `type_text` with `Ignored`, and in such
/// an app `key` is the only way into its fields. The reason stays, because it
/// is what an agent weighs that against: through `key`, every character meets
/// the app's key bindings.
const INSTRUCTIONS: &str = "Bridge to a live terminal (TUI) app. Call read_tree first, then \
prefer act with an action a node advertises; key is the fallback. Type text with type_text, not \
key: key's characters hit the app's key bindings.";

/// One passage of agent-facing text that describes the JSON tree, and what
/// `--tree-format outline` says in its place.
///
/// The outline variants are made by replacing exactly this passage in the
/// JSON text, never written out whole, so the two modes cannot drift apart
/// anywhere but where the format is described. The tests check every
/// passage is still found in its JSON text: one that is not would leave the
/// outline mode describing JSON.
struct FormatPassage {
    /// The passage as the JSON text spells it.
    json: &'static str,
    /// What the outline mode says instead.
    outline: &'static str,
}

impl FormatPassage {
    /// `text` with this passage swapped for its outline wording.
    fn to_outline(&self, text: &str) -> String {
        text.replace(self.json, self.outline)
    }
}

/// The passage of [`INSTRUCTIONS`] that the outline mode extends with a
/// pointer to its line format and to where a line lists its actions.
const INSTRUCTIONS_PASSAGE: FormatPassage = FormatPassage {
    json: "first,",
    outline: "first (an outline, one node per line, actions after act:),",
};

/// The passage of the `read_tree` description that names the format, and in
/// the outline mode spells out the line and what to do with its action list.
const READ_TREE_PASSAGE: FormatPassage = FormatPassage {
    json: "as JSON.",
    outline: "as an outline, one node per line, indented by depth: id role \"label\" = \
              \"value\" *focused act: a|b, parts only when set. Each name after act: is one \
              action to pass to act.",
};

/// The passage of the `act` description about the custom-action envelope,
/// which the outline never shows: it prints the inner name, and quotes a name
/// or an id only when the line could not hold it bare.
///
/// A quoted token is a JSON string literal, escapes and all (`"a\"b"` is the
/// name `a"b`), so "decoded" rather than "minus the quotes": stripping the
/// quotes alone hands `act` a backslash the node never advertised. Named as
/// "node and action" rather than "both", which read as the button and field
/// the sentence before it mentions.
const ACT_PASSAGE: FormatPassage = FormatPassage {
    json: "An action shown as {\"custom\":\"delete\"} is passed as \"delete\".",
    outline: "Pass node and action as shown, a quoted one JSON-decoded.",
};

/// Parameters for the `act` tool.
///
/// The fields carry no schema description, because the tool's description
/// already says where each comes from and the schema states the bound: a
/// description per field was the same sentence sent a second time on every
/// model call. `#[schemars(description = "")]` keeps these doc comments out
/// of the schema, since schemars leaves out an empty description.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ActParams {
    /// Id of the target node, as the tree from read_tree spells it.
    #[schemars(description = "")]
    pub node: String,
    /// Action name advertised by that node, as the tree spells it, or the
    /// inner name of an action the JSON tree spells as `{"custom":"delete"}`.
    #[schemars(description = "")]
    pub action: String,
    /// Value for actions that take one (e.g. the text for "set_value"), up to
    /// 4096 characters.
    #[schemars(length(max = 4096), description = "")]
    pub value: Option<String>,
}

/// Parameters for the `key` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// Key to press, e.g. "q", "enter", "tab", "ctrl+c", up to 64 characters.
    ///
    /// Undescribed in the schema, like every field of [`ActParams`]: the
    /// tool's description already gives these examples.
    #[schemars(length(min = 1, max = 64), description = "")]
    pub key: String,
    /// How many times to send this key, 1 to 64. Defaults to 1.
    ///
    /// The schema states the bounds; the default is the one thing it cannot.
    #[schemars(range(min = 1, max = 64), description = "Defaults to 1.")]
    pub repeat: Option<u32>,
}

/// Parameters for the `type_text` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TypeTextParams {
    /// Literal text to type, up to 4096 characters.
    ///
    /// Undescribed in the schema, like every field of [`ActParams`].
    #[schemars(length(min = 1, max = 4096), description = "")]
    pub text: String,
}

/// MCP server handler bridging tool calls to the socket-manager task.
#[derive(Clone)]
pub struct TariaMcpServer {
    bridge: BridgeHandle,
    tool_router: ToolRouter<Self>,
    /// How every tree this server returns is spelled for the agent.
    tree_format: TreeFormat,
    /// How tool calls reach a bridge that connects on demand, which only
    /// [`on_demand`](Self::on_demand) builds. `None` for a bridge handed in
    /// already spawned: that one connects on its own schedule, and calls are
    /// served exactly as they were before on-demand bridges existed.
    demand: Option<Demand>,
}

impl TariaMcpServer {
    /// Build the handler on top of a spawned bridge, serving the tree as the
    /// app's own JSON.
    pub fn new(handle: BridgeHandle) -> Self {
        Self::with_tree_format(handle, TreeFormat::Json)
    }

    /// Build the handler and the bridge it serves, a bridge that connects
    /// only while tool calls want the app. This is what the `taria-mcp`
    /// binary runs.
    ///
    /// No connection is opened until the first tool call: starting the
    /// server, MCP `initialize` and `tools/list` connect to nothing. A call
    /// that finds the bridge idle (or between reconnect attempts) wakes it
    /// and waits a moment, well under a second, for the app's tree, so the
    /// first call still answers with the live tree. Once no tool call has run
    /// for `idle_release`, the bridge lets go of the app: it closes its
    /// connection, or leaves the listen backlog if it was waiting behind
    /// another bridge, and stops reconnecting until the next call. A taria
    /// adapter serves one bridge at a time, so letting go is what hands the
    /// app to another session. It does not let go while an input it sent is
    /// still waiting for the app's ack, for up to 30 s past the window,
    /// because a connection that ends discards the inputs the app has not
    /// dequeued yet. [`Duration::ZERO`] never lets go once connected.
    ///
    /// Spawns the bridge task, so it has to be called from inside a tokio
    /// runtime, like [`bridge::spawn`].
    pub fn on_demand(socket: PathBuf, tree_format: TreeFormat, idle_release: Duration) -> Self {
        let (handle, demand) = bridge::spawn_on_demand(socket, idle_release);
        Self {
            demand: Some(demand),
            ..Self::with_tree_format(handle, tree_format)
        }
    }

    /// Build the handler on top of a spawned bridge, serving the tree in
    /// `tree_format`.
    ///
    /// The tool descriptions come from the `#[tool]` attributes, which
    /// describe the JSON. For the outline the router's entries are adjusted
    /// here, once, so `tools/list` describes the format the tools return.
    pub fn with_tree_format(handle: BridgeHandle, tree_format: TreeFormat) -> Self {
        let mut tool_router = Self::tool_router();
        if tree_format == TreeFormat::Outline {
            describe_outline(&mut tool_router);
        }
        Self {
            bridge: handle,
            tool_router,
            tree_format,
            demand: None,
        }
    }

    /// Count this tool call into the bridge's activity window, and give a
    /// bridge that connects on demand a bounded moment to reach the app
    /// before the call reads anything from it.
    ///
    /// Called once per tool call, before its first look at the tree, and the
    /// result held until the call returns: the call is activity for as long
    /// as it runs, and its end starts the idle window. The input tools take
    /// their baseline tree after this, so the tree a wake-up delivers is
    /// where they start from rather than a change they report. `None`, at
    /// once, for a bridge that was handed in already spawned.
    async fn attend(&self) -> Option<Call> {
        let demand = self.demand.as_ref()?;
        let call = demand.begin();
        call.settle(&self.bridge).await;
        Some(call)
    }

    /// The instructions surfaced on initialize, in this server's format.
    fn instructions(&self) -> String {
        match self.tree_format {
            TreeFormat::Json => INSTRUCTIONS.to_string(),
            TreeFormat::Outline => INSTRUCTIONS_PASSAGE.to_outline(INSTRUCTIONS),
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
        report(seen, fallback, ids.len(), self.tree_format)
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

    /// The bridge's own view of the socket, for the calls that find no tree.
    ///
    /// Read here rather than inside [`available_snapshot`] so the state and
    /// the facts explaining it are taken by the same caller: `act`, `key` and
    /// `type_text` take the state through their own receiver, to mark it seen.
    fn link_report(&self) -> LinkReport {
        LinkReport {
            rejected: self.bridge.rejected_rx.borrow().clone(),
            link: *self.bridge.link_rx.borrow(),
            peer_protocol: *self.bridge.protocol_rx.borrow(),
        }
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
    #[tool(description = "Read the TUI app's current semantic tree as JSON. Call this first.")]
    pub async fn read_tree(&self) -> Result<CallToolResult, McpError> {
        let _call = self.attend().await;
        let snapshot =
            available_snapshot(self.bridge.state_rx.borrow().clone(), self.link_report())?;
        tree_result(&snapshot, self.tree_format)
    }

    /// The `act` tool: validate and forward an advertised action.
    ///
    /// "Returns the tree if it changed", like `key` and `type_text`, because
    /// three of the answers `report` gives in place of an error carry a note
    /// and no tree: an input the app took that changed nothing, one it neither
    /// acknowledged nor answered with a change, and one it took on its way
    /// out. "Returns the updated tree" promised a tree every time.
    #[tool(
        description = "Invoke an action a node advertises; node and action come from \
                       read_tree, value is for actions like set_value. activate presses a button \
                       or submits a field. An action shown as {\"custom\":\"delete\"} is passed \
                       as \"delete\". Returns the tree if it changed."
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
        let _call = self.attend().await;
        let mut rx = self.bridge.state_rx.clone();
        let snapshot = available_snapshot(rx.borrow_and_update().clone(), self.link_report())?;

        let Some(target) = find_node(&snapshot.parsed.root, &node) else {
            let mut ids = Vec::new();
            collect_node_ids(&snapshot.parsed.root, &mut ids);
            return Err(McpError::invalid_params(
                format!(
                    "unknown node id `{node}`; valid node ids: {}",
                    id_list(&ids)
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
    ///
    /// The examples are bare, not quoted: every quote is two characters once
    /// the description is JSON, and the list reads the same without them. The
    /// text rule is a preference, for the reason on [`INSTRUCTIONS`].
    #[tool(
        description = "Send one raw key press (q, enter, tab, ctrl+c, f5) to wherever focus \
                       is; repeat sends it up to 64 times. A fallback where no advertised action \
                       fits. Type text with type_text, not key: key's characters hit the app's \
                       key bindings. Returns the tree if it changed."
    )]
    pub async fn key(
        &self,
        Parameters(KeyParams { key, repeat }): Parameters<KeyParams>,
    ) -> Result<CallToolResult, McpError> {
        if key.is_empty() {
            return Err(McpError::invalid_params("key must be non-empty", None));
        }
        // Bounded before the parse, not only before the send, for the reason on
        // [`MAX_KEY_CHARS`]: the string that costs the connection is one the
        // parser accepts, and peeling a million modifiers to learn that is
        // itself the wrong amount of work to do for one key.
        let len = key.chars().count();
        if len > MAX_KEY_CHARS {
            return Err(McpError::invalid_params(
                format!(
                    "key is {len} characters, over the {MAX_KEY_CHARS} character limit for one \
                     key call; a key is one press, so send it without the extra text"
                ),
                None,
            ));
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
        let _call = self.attend().await;
        let mut rx = self.bridge.state_rx.clone();
        // Like `act`, refuse while no app is connected: a key queued now
        // would only be delivered to (and confuse) the *next* app instance.
        let pre = available_snapshot(rx.borrow_and_update().clone(), self.link_report())?;
        self.send_and_report(rx, pre, AgentInput::key(key), repeat)
            .await
    }

    /// The `type_text` tool: a whole string in one call.
    ///
    /// A newline "may" act as Enter and submit: an adapter that feeds typing in
    /// as key events lowers it to Enter by convention, an app that reads the
    /// characters itself decides what it means, and Enter submits only where
    /// the focused widget submits on it, not in a multi-line editor. What
    /// holds everywhere is the advice: text meant for after a submit belongs
    /// in a second call, once the tree shows where typing goes next.
    #[tool(
        description = "Type literal text into the app, never through its key bindings: it goes \
                       where the app takes typing, usually the focused field. A newline may act \
                       as Enter and submit; send text after it in a second call. Returns the tree \
                       if it changed."
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
        let _call = self.attend().await;
        let mut rx = self.bridge.state_rx.clone();
        let pre = available_snapshot(rx.borrow_and_update().clone(), self.link_report())?;
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
            .with_instructions(self.instructions())
    }
}

/// Rewrite the tool descriptions that describe the JSON tree so they describe
/// the outline instead, leaving every other word as the attributes wrote it.
///
/// Only `read_tree` and `act` mention the format: `key` and `type_text` say
/// they return "the tree if it changed", which is true of either, and no
/// parameter carries a description to swap. A route shaped other than
/// expected is left alone rather than failed on; the tests pin that every
/// passage is found.
fn describe_outline(router: &mut ToolRouter<TariaMcpServer>) {
    if let Some(route) = router.map.get_mut("read_tree") {
        swap_description(&mut route.attr.description, &READ_TREE_PASSAGE);
    }
    if let Some(route) = router.map.get_mut("act") {
        swap_description(&mut route.attr.description, &ACT_PASSAGE);
    }
}

/// Swap `passage` for its outline wording inside one tool description.
fn swap_description(
    description: &mut Option<std::borrow::Cow<'static, str>>,
    passage: &FormatPassage,
) {
    if let Some(text) = description.as_mut() {
        *text = passage.to_outline(text).into();
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

/// What the bridge knows about its own end of the socket, for the calls that
/// find no snapshot to work from.
///
/// Grouped rather than passed one by one because no field decides the answer
/// alone: a rejected line, an open connection, and a completed handshake are
/// three facts about the same silence, and which of them is the story depends
/// on the others. Read nowhere else. Once a tree is being served, none of this
/// is what an agent needs to hear about. A disconnect reads only `rejected`,
/// and only for [`OVERSIZED_LINE`]: see [`available_snapshot`].
struct LinkReport {
    /// Why the most recent line from an app was thrown away, if one was.
    rejected: Option<String>,
    /// Whether a connection is open, and since when.
    link: LinkState,
    /// The `protocol_version` the peer declared, if it has said anything at
    /// all. `Some` is the handshake, so this doubles as "the peer greeted us".
    peer_protocol: Option<u32>,
}

/// Extract the live snapshot from a [`BridgeState`], or the error explaining
/// why none is available. The failure modes call for different next steps, so
/// they get distinct messages: an app that connected and then went away (it
/// exited or crashed; waiting for it to come back is enough), the same state
/// reached because the bridge cut the app off over a line too long to read
/// (waiting fixes nothing), the same state again with this bridge reconnected
/// and left unanswered because another bridge holds the app (the one that won
/// the reconnect race when the app restarted, or the one that took it while
/// this bridge had let go), and the ways [`BridgeState::Never`] is reached,
/// which [`no_snapshot_reason`] tells apart.
fn available_snapshot(state: BridgeState, report: LinkReport) -> Result<AppSnapshot, McpError> {
    match state {
        BridgeState::Connected(snapshot) => Ok(snapshot),
        BridgeState::Never => Err(McpError::internal_error(no_snapshot_reason(report), None)),
        BridgeState::Disconnected {
            app_label,
            last_seq,
        } => {
            // The opening is the same either way: callers and scripts match
            // on it to tell a departed app from one that never came.
            let gone = format!(
                "app '{}' disconnected (last snapshot seq {last_seq})",
                app_label.as_deref().unwrap_or("unknown")
            );
            let message = if is_oversized(report.rejected.as_deref()) {
                format!(
                    "{gone}; its latest snapshot was over 1 MiB ({MAX_LINE_BYTES} bytes), this \
                     bridge's line limit, so the bridge closed the connection. {OVERSIZED_REPEATS}"
                )
            } else if let Some(open_for) = held_for(&report) {
                // "It may have exited" was the answer here, about an app that
                // was running and serving another bridge.
                format!(
                    "{gone}, and the app is now held by another bridge. {}",
                    held_by_another_bridge(open_for)
                )
            } else {
                format!(
                    "{gone}; it may have exited. The bridge reconnects automatically; retry once \
                     the app is back."
                )
            };
            Err(McpError::internal_error(message, None))
        }
    }
}

/// What follows from a snapshot over [`MAX_LINE_BYTES`], said the same way by
/// every answer that reports one.
///
/// The loop is the point. The app resends its latest snapshot to every bridge
/// that connects, so each reconnect meets the same line and is closed over it
/// again. "It may have exited; retry once the app is back" had an agent
/// waiting on an app that was running all along, for a tree no retry could
/// fetch.
const OVERSIZED_REPEATS: &str = "The app resends its latest snapshot on every reconnect, so this \
repeats until it publishes a smaller tree, most likely by bounding a value it does not bound \
today (a draft, a log, a transcript). No input tool can reach the app until then, and nothing you \
send can fix it.";

/// True when `rejected` is the bridge's reason for a line over
/// [`MAX_LINE_BYTES`], as opposed to a line that arrived whole and failed to
/// parse, or no rejection at all.
fn is_oversized(rejected: Option<&str>) -> bool {
    rejected == Some(OVERSIZED_LINE)
}

/// Why no app has published a tree yet, in the terms of whichever fact is the
/// story.
///
/// [`BridgeState::Never`] is five states wearing one name, and they call for
/// five different next steps. Only the last of them is worth checking a socket
/// path over, and it used to be the answer given to all of them. Sending an
/// adopter to re-check a path the bridge is connected to is the first wrong
/// turn a retrofit takes, and the bridge knew better every time:
///
/// - A line ran past [`MAX_LINE_BYTES`] and the bridge closed the connection
///   over it. The app found the socket, and its tree is too big to carry.
/// - A line arrived and could not be read. The app found the socket, so the
///   line is what is wrong.
/// - The peer completed the handshake and has published nothing. The app is
///   connected and is not publishing nodes.
/// - A connection is open and nobody has said a word on it. An adapter greets
///   a bridge the moment it accepts one, and serves one bridge at a time, so
///   this is what the second bridge gets: a connection the kernel completed
///   into the listen backlog, which the app does not accept until the first
///   bridge leaves, or lets go of the app after its idle window.
/// - Nothing is connected. Here, and only here, the path and whether the app
///   is running are the questions.
fn no_snapshot_reason(report: LinkReport) -> String {
    // An oversized line outranks everything, the rejected-line answer below
    // included: that one's depth and version hints are about a line that
    // arrived whole and failed to parse, and this one never arrived whole. It
    // outranks the link too, because every reconnect greets the bridge and
    // then sends the same line, so an open connection or a fresh handshake is
    // the lead-up to the next overflow rather than news. A link that is down
    // with the reason still held is between two of those reconnects: the
    // bridge drops the reason as soon as a connect finds nothing listening,
    // and this answer then gives way to the plain ones below.
    if is_oversized(report.rejected.as_deref()) {
        return format!(
            "no snapshot from the app yet, but not because nothing is there: an app reached \
             this socket, and its snapshot was over 1 MiB ({MAX_LINE_BYTES} bytes), this \
             bridge's line limit, so the bridge closed the connection. The socket path is right, \
             so the tree is what is wrong. {OVERSIZED_REPEATS}"
        );
    }
    // A rejected line outranks the rest: it is the most specific thing that
    // happened, and it happened on whatever connection the others describe.
    if let Some(err) = report.rejected {
        return format!(
            "no snapshot from the app yet, but not because nothing is there: an app reached \
             this socket and sent lines, and this bridge could not read the last one ({err}). \
             The socket path is right, so the line is what is wrong. A snapshot more than \
             {MAX_NODE_DEPTH} levels deep exceeds what a JSON parser will recurse into and \
             fails exactly like this; so does an app built against a taria whose snapshot \
             shape differs from this bridge's. Fix the tree the app publishes, or match its \
             taria dependency to this bridge's; the bridge keeps reading and needs no restart.",
        );
    }
    let Some(open_for) = report.link.open_for() else {
        return "no snapshot from the app yet, and this bridge holds no connection to the \
                socket: is the taria-enabled app running, and is the socket path correct? The \
                bridge reconnects automatically; retry once the app is up."
            .to_string();
    };
    if let Some(version) = report.peer_protocol {
        return format!(
            "no snapshot from the app yet, and the socket is not what is wrong: this bridge is \
             connected to it and the app completed the taria handshake (protocol version \
             {version}), so what is missing is the tree. An app that binds the layer and never \
             publishes nodes looks exactly like this; publish once per draw, from \
             `publish(nodes)` or a `FrameRecorder`. Nothing needs restarting: the first \
             snapshot the app publishes is served."
        );
    }
    if open_for < SILENT_LINK_GRACE {
        return format!(
            "no snapshot from the app yet: this bridge connected to the socket {}ms ago and no \
             handshake has arrived yet. The path is right, so this is worth one retry; a \
             connection that is still silent on the next call is diagnosed rather than waited \
             on.",
            open_for.as_millis()
        );
    }
    format!(
        "no snapshot from the app yet: the app is held by another bridge. {}",
        held_by_another_bridge(open_for)
    )
}

/// How long this bridge's open connection has gone without a handshake, once
/// that is past [`SILENT_LINK_GRACE`]: a connection the app has not accepted,
/// because another bridge holds it. `None` for a link that is down, one still
/// inside the grace, and one the peer has greeted, which is no longer waiting
/// to be accepted.
fn held_for(report: &LinkReport) -> Option<Duration> {
    if report.peer_protocol.is_some() {
        return None;
    }
    report
        .link
        .open_for()
        .filter(|open_for| *open_for >= SILENT_LINK_GRACE)
}

/// What follows "held by another bridge", said the same way by both answers
/// that report it: why the silence means that, what frees the app, and when.
///
/// The way out it names is a retry, not a hunt for the other process. A
/// taria-mcp from 0.3.2 on lets go of the app once its session has gone quiet,
/// and this bridge is accepted as soon as it does. Before 0.3.2 the holder was
/// most often a bridge left running by an idle session, and the answer sent
/// agents to find and stop it, which is the step letting go removes. The 60 s
/// is the default of `--idle-release`, which is all this bridge can know of
/// the holder's.
fn held_by_another_bridge(open_for: Duration) -> String {
    format!(
        "The socket path is right: this bridge has been connected for {:.1}s with no handshake, \
         which is what a second bridge gets, because a taria adapter serves one bridge at a time \
         and greets each one only when it accepts it. A taria-mcp lets go of the app after 60 s \
         with no tool calls (its default), and this bridge takes over as soon as it does, with \
         no restart; retry in a minute. If the app stays held, another session is using it, or \
         the holder is a taria-mcp older than 0.3.2, which holds the app until its session ends. \
         Failing that, the peer at this path wedged before its handshake, or is not a taria \
         adapter at all.",
        open_for.as_secs_f64()
    )
}

/// Render a snapshot as the standard tool result: the app's own JSON, or the
/// outline of it.
fn tree_result(snapshot: &AppSnapshot, format: TreeFormat) -> Result<CallToolResult, McpError> {
    Ok(text_result(snapshot_text(snapshot, format)))
}

/// One text block, the shape every result of this server takes.
fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// The text agents read the tree from: the app's own snapshot, relayed as
/// JSON or rendered as an outline from that same JSON. Every tree an agent
/// receives, from `read_tree` and inside every input tool's answer, comes
/// through here.
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
///
/// The outline is rendered from that relayed text too, for the same reason,
/// and takes only its action names from the parse, because the parse is what
/// `act` checks a name against (see [`crate::tree_format`]).
fn snapshot_text(snapshot: &AppSnapshot, format: TreeFormat) -> String {
    format.render(snapshot)
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
    ///
    /// It also tells two departures apart: an app that exited, and one the
    /// bridge closed the connection on because its next tree was over
    /// [`MAX_LINE_BYTES`] (see [`OVERSIZED_LINE`]).
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
fn report(
    seen: Observed,
    fallback: AppSnapshot,
    sent: usize,
    format: TreeFormat,
) -> Result<CallToolResult, McpError> {
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
    //
    // Which of the two answers it gets is decided by the ack, not by the
    // departure. An acknowledged input has a known fate: the app took delivery
    // of it and then left, which is exactly what invoking a quit action looks
    // like from here, and what the app did next is a fact about the app rather
    // than a failure of this call. Raising it made every caller that branches
    // on the error flag read a working quit as a failed one, and left agents
    // passing each other the workaround "treat this one as success". An input
    // that was never acknowledged is the opposite case and stays an error: its
    // fate is unknown, which is what this crate raises errors for, next to a
    // call that was invalid.
    if let Some(state) = seen.gone
        && seen.status != Some(InputStatus::Dropped)
    {
        // Both answers name the departure the way `read_tree` does, so an
        // agent meets one vocabulary for a missing app, not two.
        //
        // Unless the bridge did the leaving. A tree over the line limit ends
        // the connection from this side, and "may have exited, possibly
        // because of this input" then sends the agent to wait for an app
        // that never went anywhere and will send the same tree on every
        // reconnect. The openings stay, so a caller matching on them still
        // reads the same verdict.
        let oversized = is_oversized(seen.rejected.as_deref());
        if seen.status.is_some() {
            let text = if oversized {
                format!(
                    "The app acknowledged this input and then disconnected: {}, because the \
                     snapshot it sent next was over 1 MiB ({MAX_LINE_BYTES} bytes), this bridge's \
                     line limit. The bridge closed the connection, not the app, so there is no \
                     tree to return. {OVERSIZED_REPEATS}",
                    gone_app(&state)
                )
            } else {
                format!(
                    "The app acknowledged this input and then disconnected: {} and may have \
                     exited, possibly because of this input. An input that ends the app reads \
                     exactly like this, and there is no tree to return because the app is gone. \
                     If you did not mean to end it, it exited for its own reasons. The bridge \
                     reconnects automatically; call read_tree once the app is back.",
                    gone_app(&state)
                )
            };
            return Ok(text_result(text));
        }
        let message = if oversized {
            format!(
                "the app disconnected before acknowledging this input: {}, because a snapshot it \
                 sent was over 1 MiB ({MAX_LINE_BYTES} bytes), this bridge's line limit. The \
                 bridge closed the connection, not the app, and whether the input was applied \
                 before it did is not known. {OVERSIZED_REPEATS}",
                gone_app(&state)
            )
        } else {
            format!(
                "the app disconnected before acknowledging this input: {} and may have exited, \
                 possibly because of this input. Whether the input was applied before it went is \
                 not known, which is what separates this from an input the app acknowledged on \
                 its way out. The bridge reconnects automatically; call read_tree once the app \
                 is back, then retry if the effect is missing.",
                gone_app(&state)
            )
        };
        return Err(McpError::internal_error(message, None));
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
                 an act a modal dialog blocks or naming a node it no longer knows, a set_value \
                 carrying no value, or text sent while nothing is accepting typing). Re-plan \
                 from the current tree below.\n{}",
                snapshot_text(&snapshot, format)
            )))
        }
        (Some(InputStatus::Delivered), Some(snapshot)) => tree_result(&snapshot, format),
        (Some(InputStatus::Delivered), None) => Ok(text_result(format!(
            "The app received this input, and its tree did not change within {}ms. Call \
             read_tree if you expect a delayed effect.",
            UPDATE_WAIT.as_millis()
        ))),
        // No ack at all is how an adapter that predates acks behaves. Its
        // changed tree is the only answer it can give, and answer enough.
        (None, Some(snapshot)) => tree_result(&snapshot, format),
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
                snapshot_text(&snapshot, format)
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

/// How many node ids the unknown-node error names before it truncates.
///
/// Enough to be the whole tree for a normal screen, and small enough that a
/// typo cannot cost an agent its context: uncapped, a 2001-node tree put
/// nearly 21 KB of ids into a single error, and one wide table would put in
/// far more.
const MAX_LISTED_NODE_IDS: usize = 50;

/// Render `ids` for an error message, capped at [`MAX_LISTED_NODE_IDS`].
///
/// A truncated list says how many it left out, because a list that merely
/// stops reads as the complete set of ids, and an agent that believes that
/// concludes the id it wants does not exist rather than reading the tree
/// again.
fn id_list(ids: &[String]) -> String {
    if ids.len() <= MAX_LISTED_NODE_IDS {
        return ids.join(", ");
    }
    format!(
        "{}, and {} more not listed of {} in the tree; read_tree returns them all",
        ids[..MAX_LISTED_NODE_IDS].join(", "),
        ids.len() - MAX_LISTED_NODE_IDS,
        ids.len()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

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

    /// A [`LinkReport`] for a bridge that has heard nothing at all, which
    /// every case below varies one fact of.
    fn silent_link() -> LinkReport {
        LinkReport {
            rejected: None,
            link: LinkState::Down,
            peer_protocol: None,
        }
    }

    /// A link that has been open for `ms`, however long ago that puts the
    /// connect. Built by hand rather than by waiting: the grace is half a
    /// second, and a test that sleeps through it buys nothing.
    fn open_for(ms: u64) -> LinkState {
        LinkState::Open {
            since: Instant::now()
                .checked_sub(Duration::from_millis(ms))
                .expect("the test clock is not that young"),
        }
    }

    /// The finding this whole channel exists for. A bridge whose `connect`
    /// succeeded, against a peer that never speaks, is what the adapter's
    /// one-bridge-at-a-time design does to the second bridge. Answering it
    /// with "is the socket path correct?" sent the first agent to meet this
    /// project through nine shell calls of socket forensics.
    ///
    /// The way out it names is a retry: since 0.3.2 a taria-mcp lets go of
    /// the app once its session goes quiet, and the phrases pinned here are
    /// the ones the end-to-end scripts match on.
    #[test]
    fn a_connection_nobody_accepted_is_named_instead_of_blamed_on_the_path() {
        let reason = no_snapshot_reason(LinkReport {
            link: open_for(4_000),
            ..silent_link()
        });
        assert!(
            reason.starts_with("no snapshot from the app yet: the app is held by another bridge. "),
            "every no-tree answer keeps the same opening fact, then the cause: {reason}"
        );
        assert!(
            reason.contains("The socket path is right") && !reason.contains("socket path correct"),
            "a connected bridge must not send anyone to check the path: {reason}"
        );
        assert!(
            reason.contains("connected for 4.0s with no handshake"),
            "the age of the connection is the evidence: {reason}"
        );
        assert!(
            reason.contains("serves one bridge at a time")
                && reason.contains("lets go of the app after 60 s")
                && reason.contains("retry in a minute")
                && reason.contains("older than 0.3.2"),
            "the message must name the cause and what to do about it: {reason}"
        );
        assert!(
            !reason.contains("may have exited") && !reason.contains('\u{2014}'),
            "the app is running, and the text is plain: {reason}"
        );
    }

    /// The restart race: two bridges reconnect to a restarted app, one wins,
    /// and the other is left in the backlog with the `Disconnected` record of
    /// the app it served before. "It may have exited" was its answer, about
    /// an app that was up and serving the winner. The same answer follows a
    /// release, when another bridge took the app meanwhile.
    #[test]
    fn a_disconnected_bridge_left_unaccepted_says_the_app_is_held() {
        let gone = || BridgeState::Disconnected {
            app_label: Some("demo".to_string()),
            last_seq: 7,
        };
        let err = available_snapshot(
            gone(),
            LinkReport {
                link: open_for(4_000),
                ..silent_link()
            },
        )
        .expect_err("a held app serves this bridge no tree");
        assert!(
            err.message.starts_with(
                "app 'demo' disconnected (last snapshot seq 7), and the app is now held by \
                 another bridge. The socket path is right: this bridge has been connected for \
                 4.0s"
            ),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("lets go of the app after 60 s")
                && !err.message.contains("may have exited"),
            "{}",
            err.message
        );
        // Both held answers explain it in the same words.
        let never = no_snapshot_reason(LinkReport {
            link: open_for(4_000),
            ..silent_link()
        });
        let opening = "no snapshot from the app yet: the app is held by another bridge. ";
        assert!(never.starts_with(opening), "{never}");
        assert!(
            err.message.ends_with(&never[opening.len()..]),
            "{}",
            err.message
        );

        // A link that is down, one just opened, and one the app greeted are
        // not held, and keep the departure answer byte for byte.
        let plain = "app 'demo' disconnected (last snapshot seq 7); it may have exited. The \
                     bridge reconnects automatically; retry once the app is back.";
        for (name, link, peer_protocol) in [
            ("down", LinkState::Down, None),
            ("just opened", open_for(10), None),
            ("greeted", open_for(4_000), Some(PROTOCOL_VERSION)),
        ] {
            let err = available_snapshot(
                gone(),
                LinkReport {
                    link,
                    peer_protocol,
                    ..silent_link()
                },
            )
            .expect_err("a disconnected app has no tree");
            assert_eq!(err.message, plain, "{name}");
        }

        // And a tree over the limit still outranks it: the bridge closed the
        // connection over the size, and the app resends it on every
        // reconnect.
        let err = available_snapshot(
            gone(),
            LinkReport {
                rejected: Some(OVERSIZED_LINE.to_string()),
                link: open_for(4_000),
                peer_protocol: None,
            },
        )
        .expect_err("a disconnected app has no tree");
        assert!(
            err.message.contains(OVER_THE_LIMIT) && !err.message.contains("held by another"),
            "{}",
            err.message
        );
    }

    /// Below the grace the same silence is a handshake still arriving, and
    /// the honest answer is "retry". The path is not in question here either:
    /// the bridge is connected to it.
    #[test]
    fn a_connection_that_just_opened_is_worth_a_retry_not_a_diagnosis() {
        let reason = no_snapshot_reason(LinkReport {
            link: open_for(10),
            ..silent_link()
        });
        assert!(
            reason.contains("worth one retry"),
            "a fresh connection is not yet evidence of anything: {reason}"
        );
        assert!(
            !reason.contains("socket path correct"),
            "a connected bridge must not send anyone to check the path: {reason}"
        );
    }

    /// A peer that greeted this bridge and published nothing is a third
    /// thing again: the app is there, and it is not publishing nodes.
    #[test]
    fn an_app_that_greeted_and_published_nothing_is_reported_as_itself() {
        let reason = no_snapshot_reason(LinkReport {
            link: open_for(4_000),
            peer_protocol: Some(PROTOCOL_VERSION),
            ..silent_link()
        });
        assert!(
            reason.contains("completed the taria handshake")
                && reason.contains("never publishes nodes"),
            "the app is connected; what is missing is the tree: {reason}"
        );
        assert!(
            !reason.contains("held by another bridge"),
            "an app that greeted this bridge is not a socket somebody else holds: {reason}"
        );
    }

    /// And with nothing connected, the old questions are the right ones.
    #[test]
    fn with_no_connection_the_path_and_the_app_are_the_questions() {
        let reason = no_snapshot_reason(silent_link());
        assert!(
            reason.contains("holds no connection to the socket")
                && reason.contains("is the socket path correct"),
            "nothing connected is the one case the path is worth checking: {reason}"
        );

        // A rejected line still outranks all of it: something reached the
        // socket, which is the most specific thing that happened.
        let reason = no_snapshot_reason(LinkReport {
            rejected: Some("expected value at line 1".to_string()),
            link: open_for(4_000),
            peer_protocol: None,
        });
        assert!(
            reason.contains("could not read the last one (expected value at line 1)"),
            "a rejected line is the story wherever there is one: {reason}"
        );
    }

    /// The words every answer about a line over the limit carries.
    const OVER_THE_LIMIT: &str = "over 1 MiB (1048576 bytes)";

    /// A line over the limit outranks every other answer, the rejected-line
    /// one included, whose depth and version hints are about a line that
    /// arrived whole. An app whose tree outgrew the limit greets every
    /// reconnect and then overflows again, so each moment the link describes
    /// (down between attempts, just opened, greeted) is one an agent polls
    /// into, and each used to answer it with something else: the socket path,
    /// "worth one retry", an app that never publishes nodes.
    ///
    /// Down means between attempts only. The bridge drops the reason when a
    /// connect finds nothing listening, so a link that stays down because the
    /// app has gone arrives here without it and gets the plain answers the
    /// tests above pin (the bridge's
    /// `a_connect_that_finds_nothing_listening_forgets_the_oversized_reason`
    /// covers the drop).
    #[test]
    fn an_oversized_line_outranks_every_link_state() {
        let links = [
            ("down between attempts", LinkState::Down, None),
            ("just opened", open_for(10), None),
            ("open and silent", open_for(4_000), None),
            ("greeted", open_for(10), Some(PROTOCOL_VERSION)),
        ];
        for (name, link, peer_protocol) in links {
            let reason = no_snapshot_reason(LinkReport {
                rejected: Some(OVERSIZED_LINE.to_string()),
                link,
                peer_protocol,
            });
            assert!(
                reason.starts_with("no snapshot from the app yet")
                    && reason.contains(OVER_THE_LIMIT)
                    && reason.contains("The socket path is right"),
                "{name}: the size is the story, and the path is not in question: {reason}"
            );
            for wrong in [
                "is the socket path correct",
                "worth one retry",
                "never publishes nodes",
                "held by another bridge",
                "levels deep",
            ] {
                assert!(
                    !reason.contains(wrong),
                    "{name}: `{wrong}` is another state's answer: {reason}"
                );
            }
        }
    }

    /// The departure an oversized line causes keeps the opening every
    /// disconnect has, which callers match on, and replaces "it may have
    /// exited" with the cause: the app is running, and waiting fixes nothing.
    #[test]
    fn a_disconnect_over_an_oversized_line_names_the_size_not_an_exit() {
        let gone = || BridgeState::Disconnected {
            app_label: Some("demo".to_string()),
            last_seq: 7,
        };
        let err = available_snapshot(
            gone(),
            LinkReport {
                rejected: Some(OVERSIZED_LINE.to_string()),
                ..silent_link()
            },
        )
        .expect_err("a disconnected app has no tree");
        assert!(
            err.message
                .starts_with("app 'demo' disconnected (last snapshot seq 7); ")
                && err.message.contains(OVER_THE_LIMIT),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("on every reconnect")
                && err.message.contains("nothing you send can fix it"),
            "the agent has to hear that this repeats and that it cannot help: {}",
            err.message
        );
        assert!(
            !err.message.contains("may have exited"),
            "the app did not exit; the bridge closed the connection: {}",
            err.message
        );

        // Any other rejected line is not this story, and leaves the
        // disconnect answer as it was.
        let err = available_snapshot(
            gone(),
            LinkReport {
                rejected: Some("expected value at line 1".to_string()),
                ..silent_link()
            },
        )
        .expect_err("a disconnected app has no tree");
        assert_eq!(
            err.message,
            "app 'demo' disconnected (last snapshot seq 7); it may have exited. The bridge \
             reconnects automatically; retry once the app is back."
        );
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

        let err = report(seen, pre, 4, TreeFormat::Json)
            .expect_err("an incomplete observation is not a success");
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
        let err = report(seen, snapshot("after"), 5, TreeFormat::Json)
            .expect_err("lost acks are not a success");
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
        let err = report(seen, snapshot("after"), 1, TreeFormat::Json)
            .expect_err("lost acks are not a success");
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

        let err =
            report(seen, pre, 2, TreeFormat::Json).expect_err("a dropped burst is not a success");
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

        let err =
            report(seen, pre, 2, TreeFormat::Json).expect_err("a partial burst is not a success");
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
    ///
    /// It is a result and not an error, because the input's fate is known:
    /// the app took delivery of it and said so, then left. Raised, the most
    /// ordinary way to reach this state, an agent invoking an advertised
    /// `quit`, was reported as a failed call to every caller that reads the
    /// error flag rather than the prose.
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

        let result = report(seen, pre, 1, TreeFormat::Json)
            .expect("an acknowledged input is not a failed call, even here");
        let text = result_text(&result);
        assert!(
            text.contains("acknowledged this input and then disconnected")
                && text.contains("app 'demo' (last snapshot seq 7) is gone"),
            "the report must name the app and its departure: {text}"
        );
        assert!(
            !text.contains("did not change"),
            "an app that exited must not be reported as one that ignored the input: {text}"
        );
    }

    /// The other side of the same departure: no ack at all. Nobody can say
    /// whether this input was applied before the app went, and an unknown
    /// fate is what this crate raises errors for, beside a call that was
    /// invalid. The two answers have to stay distinguishable by more than
    /// their prose, because the flag is what a caller branches on.
    #[test]
    fn an_input_the_app_never_acknowledged_stays_an_error_when_it_goes() {
        let seen = Observed {
            status: None,
            gone: Some(BridgeState::Disconnected {
                app_label: Some("demo".to_string()),
                last_seq: 7,
            }),
            ..Observed::default()
        };
        let err = report(seen, snapshot("after"), 1, TreeFormat::Json)
            .expect_err("an input with no acknowledgement has no known fate");
        assert!(
            err.message
                .contains("disconnected before acknowledging this input")
                && err
                    .message
                    .contains("app 'demo' (last snapshot seq 7) is gone"),
            "the report must name the app and its departure: {}",
            err.message
        );
    }

    /// The input that pushed the tree over the limit: acknowledged, and then
    /// the bridge closed the connection over the snapshot that followed.
    /// "May have exited, possibly because of this input" is wrong twice over,
    /// since the app is running and resends that tree on every reconnect. The
    /// fate is still known, so the answer is still a result, and the opening
    /// is the one every acknowledged departure has.
    #[test]
    fn an_acknowledged_input_followed_by_an_oversized_tree_names_the_size() {
        let gone = || {
            Some(BridgeState::Disconnected {
                app_label: Some("demo".to_string()),
                last_seq: 7,
            })
        };
        let seen = Observed {
            status: Some(InputStatus::Delivered),
            gone: gone(),
            rejected: Some(OVERSIZED_LINE.to_string()),
            ..Observed::default()
        };
        let result = report(seen, snapshot("after"), 1, TreeFormat::Json)
            .expect("an acknowledged input is not a failed call, even here");
        let text = result_text(&result);
        assert!(
            text.starts_with("The app acknowledged this input and then disconnected: ")
                && text.contains("app 'demo' (last snapshot seq 7) is gone")
                && text.contains(OVER_THE_LIMIT)
                && text.contains("The bridge closed the connection, not the app"),
            "{text}"
        );
        assert!(
            !text.contains("may have exited"),
            "the app did not exit: {text}"
        );

        // Without an acknowledgement the fate is unknown and it stays an
        // error, naming the same cause.
        let seen = Observed {
            status: None,
            gone: gone(),
            rejected: Some(OVERSIZED_LINE.to_string()),
            ..Observed::default()
        };
        let err = report(seen, snapshot("after"), 1, TreeFormat::Json)
            .expect_err("an input with no acknowledgement has no known fate");
        assert!(
            err.message
                .starts_with("the app disconnected before acknowledging this input: ")
                && err.message.contains(OVER_THE_LIMIT)
                && !err.message.contains("may have exited"),
            "{}",
            err.message
        );

        // A line rejected for any other reason says nothing about why the app
        // went, and the plain departure answer stands.
        let seen = Observed {
            status: Some(InputStatus::Delivered),
            gone: gone(),
            rejected: Some("expected value at line 1".to_string()),
            ..Observed::default()
        };
        let result = report(seen, snapshot("after"), 1, TreeFormat::Json).expect("a result");
        let text = result_text(&result);
        assert!(
            text.contains("and may have exited, possibly because of this input")
                && !text.contains(OVER_THE_LIMIT),
            "{text}"
        );
    }

    /// The text of a result, for the answers that are not errors.
    fn result_text(result: &CallToolResult) -> &str {
        match result.content.first() {
            Some(ContentBlock::Text(text)) => &text.text,
            other => panic!("expected text content, got {other:?}"),
        }
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
        TariaMcpServer::new(detached_handle())
    }

    /// A bridge handle with nothing on the other end.
    fn detached_handle() -> BridgeHandle {
        let (_state_tx, state_rx) = watch::channel(BridgeState::Never);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
        let (ack_tx, _) = broadcast::channel(1);
        let (_protocol_tx, protocol_rx) = watch::channel(None);
        let (_rejected_tx, rejected_rx) = watch::channel(None);
        let (_link_tx, link_rx) = watch::channel(LinkState::Down);
        BridgeHandle {
            state_rx,
            input_tx,
            ack_tx,
            protocol_rx,
            rejected_rx,
            link_rx,
        }
    }

    /// The JSON mode's agent-facing text, copied here rather than read from
    /// the constants, so that the JSON mode is held to it byte for byte and a
    /// change to what an agent reads is made here too, on purpose. All of it
    /// is resent with every model call, which is why it is this short.
    const PINNED_INSTRUCTIONS: &str = "Bridge to a live terminal (TUI) app. Call read_tree first, \
        then prefer act with an action a node advertises; key is the fallback. Type text with \
        type_text, not key: key's characters hit the app's key bindings.";

    const PINNED_READ_TREE: &str =
        "Read the TUI app's current semantic tree as JSON. Call this first.";

    const PINNED_ACT: &str = "Invoke an action a node advertises; node and action come from \
        read_tree, value is for actions like set_value. activate presses a button or submits a \
        field. An action shown as {\"custom\":\"delete\"} is passed as \"delete\". Returns the \
        tree if it changed.";

    const PINNED_KEY: &str = "Send one raw key press (q, enter, tab, ctrl+c, f5) to wherever \
        focus is; repeat sends it up to 64 times. A fallback where no advertised action fits. \
        Type text with type_text, not key: key's characters hit the app's key bindings. Returns \
        the tree if it changed.";

    const PINNED_TYPE_TEXT: &str = "Type literal text into the app, never through its key \
        bindings: it goes where the app takes typing, usually the focused field. A newline may \
        act as Enter and submit; send text after it in a second call. Returns the tree if it \
        changed.";

    /// The outline mode's three swapped texts, pinned the same way, so that a
    /// change to a format passage is made here on purpose too.
    const PINNED_OUTLINE_INSTRUCTIONS: &str = "Bridge to a live terminal (TUI) app. Call \
        read_tree first (an outline, one node per line, actions after act:), then prefer act \
        with an action a node advertises; key is the fallback. Type text with type_text, not \
        key: key's characters hit the app's key bindings.";

    const PINNED_OUTLINE_READ_TREE: &str = "Read the TUI app's current semantic tree as an \
        outline, one node per line, indented by depth: id role \"label\" = \"value\" *focused \
        act: a|b, parts only when set. Each name after act: is one action to pass to act. Call \
        this first.";

    const PINNED_OUTLINE_ACT: &str = "Invoke an action a node advertises; node and action come \
        from read_tree, value is for actions like set_value. activate presses a button or \
        submits a field. Pass node and action as shown, a quoted one JSON-decoded. Returns the \
        tree if it changed.";

    /// A tool's input schema as `tools/list` serves it.
    fn input_schema(server: &TariaMcpServer, name: &str) -> serde_json::Value {
        let tool = server
            .get_tool(name)
            .unwrap_or_else(|| panic!("{name} is listed"));
        serde_json::Value::Object((*tool.input_schema).clone())
    }

    /// A tool's description as `tools/list` serves it.
    fn description(server: &TariaMcpServer, name: &str) -> String {
        server
            .get_tool(name)
            .and_then(|tool| tool.description)
            .unwrap_or_else(|| panic!("{name} is listed with a description"))
            .into_owned()
    }

    /// The default mode serves the pinned text: the instructions, every tool
    /// description, and every schema, whose parameters carry no description
    /// but `repeat`'s default and keep every bound.
    #[test]
    fn json_mode_serves_the_pinned_text() {
        let server = TariaMcpServer::new(detached_handle());
        assert_eq!(server.tree_format, TreeFormat::Json);
        assert_eq!(
            server.get_info().instructions.as_deref(),
            Some(PINNED_INSTRUCTIONS)
        );
        assert_eq!(description(&server, "read_tree"), PINNED_READ_TREE);
        assert_eq!(description(&server, "act"), PINNED_ACT);
        assert_eq!(description(&server, "key"), PINNED_KEY);
        assert_eq!(description(&server, "type_text"), PINNED_TYPE_TEXT);
        let draft = "https://json-schema.org/draft/2020-12/schema";
        assert_eq!(
            input_schema(&server, "act"),
            serde_json::json!({
                "$schema": draft,
                "properties": {
                    "action": {"type": "string"},
                    "node": {"type": "string"},
                    "value": {"maxLength": 4096, "type": ["string", "null"]},
                },
                "required": ["node", "action"],
                "type": "object",
            })
        );
        assert_eq!(
            input_schema(&server, "key"),
            serde_json::json!({
                "$schema": draft,
                "properties": {
                    "key": {"maxLength": 64, "minLength": 1, "type": "string"},
                    "repeat": {
                        "description": "Defaults to 1.",
                        "format": "uint32",
                        "maximum": 64,
                        "minimum": 1,
                        "type": ["integer", "null"],
                    },
                },
                "required": ["key"],
                "type": "object",
            })
        );
        assert_eq!(
            input_schema(&server, "type_text"),
            serde_json::json!({
                "$schema": draft,
                "properties": {
                    "text": {"maxLength": 4096, "minLength": 1, "type": "string"},
                },
                "required": ["text"],
                "type": "object",
            })
        );
        assert_eq!(
            input_schema(&server, "read_tree"),
            serde_json::json!({"properties": {}, "type": "object"})
        );
        // Every other byte of the listing too: names, schemas, annotations.
        assert_eq!(
            server.tool_router.list_all(),
            TariaMcpServer::tool_router().list_all(),
            "the JSON mode must list the tools exactly as the attributes wrote them"
        );
        let explicit = TariaMcpServer::with_tree_format(detached_handle(), TreeFormat::Json);
        assert_eq!(
            explicit.tool_router.list_all(),
            server.tool_router.list_all()
        );
        assert_eq!(
            explicit.get_info().instructions,
            server.get_info().instructions
        );

        let tree = snapshot("pinned");
        assert_eq!(snapshot_text(&tree, TreeFormat::Json), tree.tree_json);
    }

    /// The outline mode lists the same four tools and changes only the
    /// passages that describe the format, which are swapped back here to
    /// prove nothing else moved.
    #[test]
    fn outline_mode_describes_the_outline_and_nothing_else_changes() {
        let json = TariaMcpServer::new(detached_handle());
        let outline = TariaMcpServer::with_tree_format(detached_handle(), TreeFormat::Outline);

        let names = |server: &TariaMcpServer| {
            server
                .tool_router
                .list_all()
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&outline), ["act", "key", "read_tree", "type_text"]);
        assert_eq!(names(&outline), names(&json));

        let pairs = [
            (
                json.get_info().instructions.unwrap_or_default(),
                outline.get_info().instructions.unwrap_or_default(),
                &INSTRUCTIONS_PASSAGE,
            ),
            (
                description(&json, "read_tree"),
                description(&outline, "read_tree"),
                &READ_TREE_PASSAGE,
            ),
            (
                description(&json, "act"),
                description(&outline, "act"),
                &ACT_PASSAGE,
            ),
        ];
        for (json_text, outline_text, passage) in pairs {
            assert_eq!(
                json_text.matches(passage.json).count(),
                1,
                "the JSON text must carry the passage the outline replaces exactly once, or \
                 the outline mode would describe JSON or swap the wrong words: {json_text}"
            );
            assert!(outline_text.contains(passage.outline), "{outline_text}");
            // "JSON" alone may appear: a quoted outline token is a JSON
            // string, and act says to decode it as one. What may not is a
            // description of the tree as JSON.
            assert!(
                !outline_text.contains("{\"custom\"")
                    && !outline_text.contains("as JSON")
                    && !outline_text.contains("as it appears"),
                "the outline mode must not describe the JSON: {outline_text}"
            );
            assert_eq!(
                outline_text.replace(passage.outline, passage.json),
                json_text,
                "only the format passage may differ between the modes"
            );
        }
        assert_eq!(
            outline.get_info().instructions.as_deref(),
            Some(PINNED_OUTLINE_INSTRUCTIONS)
        );
        assert_eq!(description(&outline, "read_tree"), PINNED_OUTLINE_READ_TREE);
        assert_eq!(description(&outline, "act"), PINNED_OUTLINE_ACT);
        // The markers the benchmark reads a session's format from, and the
        // phrase the end-to-end gate checks the instructions for.
        assert!(description(&json, "read_tree").contains("tree as JSON"));
        assert!(description(&outline, "read_tree").contains("tree as an outline"));
        assert!(
            outline
                .get_info()
                .instructions
                .is_some_and(|text| text.contains("one node per line"))
        );

        // The legend is the renderer's own line for a node with every part
        // set, so the two cannot drift apart.
        let legend = crate::tree_format::outline(
            r#"{"protocol_version":1,"seq":1,"root":{"id":"id","role":"role","label":"label","value":"value","focused":true,"actions":["a","b"]}}"#,
        );
        let legend = legend.lines().last().expect("a node line");
        assert_eq!(legend, "id role \"label\" = \"value\" *focused act: a|b");
        assert!(
            description(&outline, "read_tree").contains(legend),
            "{}",
            description(&outline, "read_tree")
        );

        // `key` and `type_text` describe no format, so they do not move at
        // all, and no schema does either: no parameter is described in terms
        // of the format.
        for name in ["act", "key", "read_tree", "type_text"] {
            assert_eq!(
                input_schema(&outline, name),
                input_schema(&json, name),
                "{name}"
            );
        }
        assert_eq!(description(&outline, "key"), PINNED_KEY);
        assert_eq!(description(&outline, "type_text"), PINNED_TYPE_TEXT);
    }

    /// The action names in an outline line, in order, JSON-decoded where
    /// quoted, the way the outline tells an agent to pass them.
    fn outline_action_names(line: &str) -> Vec<String> {
        crate::tree_format::line_actions(line).expect("the line ends in an action list")
    }

    /// The outline tells the agent to pass an action exactly as the line
    /// spells it, so every name it prints has to get through `act`'s
    /// validation and reach the app as the action the node advertised. The
    /// shapes here are the ones a plain envelope rule got wrong: a longer
    /// envelope, an object a newer app might send, an envelope naming a
    /// built-in, an object whose first key on the wire is not the first in the
    /// relayed, key-sorted text, and names that need quoting, one of them
    /// carrying the `|` that joins the list and one a quote, which prints
    /// escaped: stripping the quotes alone would pass `a\"b` for `a"b`.
    #[tokio::test]
    async fn every_action_name_the_outline_prints_is_one_act_accepts() {
        let line = concat!(
            r#"{"type":"snapshot","protocol_version":1,"seq":4,"root":{"id":"app","role":"app","#,
            r#""children":[{"id":"row","role":"list_item","actions":["select","#,
            r#"{"custom":"delete","icon":"x"},{"scroll_to":{"row":3}},{"custom":"activate"},"#,
            r#"{"custom":"two words"},"frobnicate",{"zoom":1,"custom":"x"},{"custom":"a|b"},"#,
            r#"{"custom":"a\"b"}]}]}}"#
        );
        let Ok(taria::wire::AppToBridge::Snapshot(parsed)) = serde_json::from_str(line) else {
            panic!("the line is a snapshot");
        };
        let tree = AppSnapshot::from_line(line, parsed);
        let outline = snapshot_text(&tree, TreeFormat::Outline);
        assert_eq!(
            outline,
            "protocol_version=1 seq=4\napp app\n  row list_item \
             act: select|delete|scroll_to|activate|\"two words\"|frobnicate|zoom|\"a|b\"|\"a\\\"b\""
        );
        let names = outline_action_names(outline.lines().last().expect("a node line"));
        let row = find_node(&tree.parsed.root, "row").expect("the row parsed");
        assert_eq!(names.len(), row.actions.len(), "{names:?}");

        // A bridge whose app acks every input as dropped, which ends each
        // call at once; what matters is which inputs got as far as the app.
        let (_state_tx, state_rx) = watch::channel(BridgeState::Connected(tree.clone()));
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(4);
        let (ack_tx, _) = broadcast::channel(4);
        let (_protocol_tx, protocol_rx) = watch::channel(Some(PROTOCOL_VERSION));
        let (_rejected_tx, rejected_rx) = watch::channel(None);
        let (_link_tx, link_rx) = watch::channel(LinkState::Down);
        let app_acks = ack_tx.clone();
        let server = TariaMcpServer::with_tree_format(
            BridgeHandle {
                state_rx,
                input_tx,
                ack_tx,
                protocol_rx,
                rejected_rx,
                link_rx,
            },
            TreeFormat::Outline,
        );
        let app = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some((id, input)) = input_rx.recv().await {
                received.push(input);
                let _ = app_acks.send((id, InputStatus::Dropped));
            }
            received
        });
        for name in &names {
            let result = server
                .act(Parameters(ActParams {
                    node: "row".to_string(),
                    action: name.clone(),
                    value: None,
                }))
                .await;
            if let Err(err) = result {
                assert!(
                    err.message.contains("dropped"),
                    "`{name}` is printed in the outline, so act must send it: {}",
                    err.message
                );
            }
        }
        drop(server);
        let received = app.await.expect("the fake app");
        let sent: Vec<AgentInput> = row
            .actions
            .iter()
            .map(|action| AgentInput::act(NodeId("row".to_string()), action.clone(), None))
            .collect();
        assert_eq!(received, sent, "names: {names:?}");
    }

    /// Every tree an input tool hands back goes out in the server's format,
    /// the note-and-tree answers included.
    #[test]
    fn input_results_carry_the_tree_in_the_servers_format() {
        let pre = snapshot("before");
        let after = snapshot("after");
        let outline = "protocol_version=1 seq=1\napp app \"after\"";
        assert_eq!(snapshot_text(&after, TreeFormat::Outline), outline);

        let changed = Observed {
            status: Some(InputStatus::Delivered),
            changed: Some(after.clone()),
            ..Observed::default()
        };
        let result = report(changed, pre.clone(), 1, TreeFormat::Outline).expect("a tree");
        assert_eq!(result_text(&result), outline);

        let ignored = Observed {
            status: Some(InputStatus::Ignored),
            ..Observed::default()
        };
        let result = report(ignored, after.clone(), 1, TreeFormat::Outline).expect("a tree");
        let text = result_text(&result);
        assert!(
            text.starts_with("The app received this input and deliberately did nothing")
                && text.ends_with(&format!(".\n{outline}")),
            "{text}"
        );

        // And the JSON mode still answers with the app's own line.
        let ignored = Observed {
            status: Some(InputStatus::Ignored),
            ..Observed::default()
        };
        let result = report(ignored, after.clone(), 1, TreeFormat::Json).expect("a tree");
        assert!(
            result_text(&result).ends_with(&format!(".\n{}", after.tree_json)),
            "{}",
            result_text(&result)
        );
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

    /// The grammar peels modifier prefixes without a limit, so a key string of
    /// repeated `ctrl+` parses fine and serializes to a line past the 1 MiB
    /// cap both peers break the connection over. Bounding the string is what
    /// keeps one legal call from cutting the app off from its bridge, and it
    /// has to happen before the parse, which is the part that would spend a
    /// million iterations on the same string.
    #[tokio::test]
    async fn key_refuses_a_string_over_the_limit() {
        let server = detached_server();

        let oversized = format!("{}a", "ctrl+".repeat(MAX_KEY_CHARS));
        let len = oversized.chars().count();
        let err = server
            .key(Parameters(KeyParams {
                key: oversized,
                repeat: None,
            }))
            .await
            .expect_err("a key string this long is not a key");
        assert!(
            err.message.contains(&MAX_KEY_CHARS.to_string())
                && err.message.contains(&len.to_string()),
            "the refusal must name the limit and what was sent: {}",
            err.message
        );

        // At the limit the length is not what stops the call: a key that long
        // still has to parse, and this one does not.
        let err = server
            .key(Parameters(KeyParams {
                key: "x".repeat(MAX_KEY_CHARS),
                repeat: None,
            }))
            .await
            .expect_err("64 `x` characters are not a key");
        assert!(
            err.message.contains("unrecognized key"),
            "the limit is a character count, not a second grammar: {}",
            err.message
        );
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

    /// The id list an unknown node id draws is bounded, and says so when it
    /// is: uncapped it was 21 KB of an agent's context for a 2001-node tree,
    /// and a list that just stops reads as the complete set of ids.
    #[test]
    fn the_id_list_is_capped_and_names_what_it_left_out() {
        let ids: Vec<String> = (0..MAX_LISTED_NODE_IDS)
            .map(|i| format!("node-{i}"))
            .collect();
        let listed = id_list(&ids);
        assert!(listed.ends_with("node-49"), "listed: {listed}");
        assert!(
            !listed.contains("not listed"),
            "a complete list must not claim to be partial: {listed}"
        );

        let ids: Vec<String> = (0..2001).map(|i| format!("node-{i}")).collect();
        let listed = id_list(&ids);
        assert!(listed.starts_with("node-0, node-1, "), "listed: {listed}");
        assert!(!listed.contains("node-50"), "listed: {listed}");
        assert!(
            listed.contains("1951 more not listed of 2001 in the tree"),
            "the truncation must name how many it left out: {listed}"
        );
        assert!(
            listed.len() < 1024,
            "a capped list is what keeps the error small, but it is {} bytes",
            listed.len()
        );
    }
}
