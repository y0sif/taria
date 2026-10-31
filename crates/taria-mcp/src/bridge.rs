//! Socket manager: owns the connection to the app's taria Unix socket.
//!
//! A single background task connects, retrying with capped backoff for as long
//! as it wants the app. [`spawn`] wants it from the start and for good: it
//! connects at once and holds the app until the session ends. The `taria-mcp`
//! binary starts the manager on demand instead (through
//! [`TariaMcpServerBuilder::on_demand`](crate::TariaMcpServerBuilder::on_demand)):
//! it opens no connection until the first tool call, and once no tool call has
//! run for its idle window it lets go of the app, closing its connection and
//! reconnecting only when the next call comes. Either way the task
//! reads `AppToBridge` ndjson lines into a [`watch`] channel holding the
//! latest [`BridgeState`], and writes queued [`AgentInput`]s out as
//! `BridgeToApp::Input` lines. A snapshot is stored as an [`AppSnapshot`],
//! which keeps the app's line (minus the message framing) alongside the parse
//! of it, so the tool layer passes on what the app wrote instead of
//! re-serializing what this build could decode. On disconnect the watch flips
//! to
//! [`BridgeState::Disconnected`] so tool calls fail fast (instead of acting
//! on a stale tree) with an error that says which app went away.
//!
//! Every outgoing input carries an [`InputId`] and the app answers it with an
//! `AppToBridge::Ack`. Acks are republished on a [`broadcast`] channel rather
//! than kept here, because only the tool call that sent an input knows which
//! id it is waiting for; the manager stays free of per-input bookkeeping.
//! Whether the app's hello said it stamps its snapshots is kept in one more
//! [`watch`] ([`BridgeHandle::stamps_rx`]), and ids are claimed and queued
//! under one lock ([`BridgeHandle::send`]), so a stamp can be compared with
//! them in the order they went out.
//!
//! A line the reader cannot parse is skipped, so that a buggy app cannot kill
//! the bridge, and the reason is kept in a third [`watch`] channel
//! ([`BridgeHandle::rejected_rx`]) rather than only logged. Skipping is
//! invisible from the tool surface: an app whose every line is rejected is
//! indistinguishable from an app that never started, and telling those apart
//! is the difference between an adopter fixing their tree and an adopter
//! re-checking a socket path that was right all along.
//!
//! A fourth channel ([`BridgeHandle::link_rx`]) reports the manager's own
//! `connect`, for the same reason one level down: a connect that keeps failing
//! and a connection nobody ever writes on both leave the state at
//! [`BridgeState::Never`], and only the first of them is about the socket
//! path. The adapter serves one bridge at a time, so a second bridge's connect
//! succeeds into the listen backlog and is not accepted until the first bridge
//! leaves, which is precisely the second case and the one a new adopter meets
//! first. (Once it is accepted, the reference adapter throws away whatever it
//! wrote while it waited, unread and unacked.) A bridge started on demand is
//! the first bridge leaving: a session that has gone quiet lets go of the app
//! after its idle window, and the bridge waiting behind it is accepted then.
//! Before 0.3.2 every bridge held the app for its whole session, so an idle
//! session locked out the one that wanted it.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus, MAX_LINE_BYTES};
use taria::{AgentInput, PROTOCOL_VERSION, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant as TokioInstant;

use crate::server::{MAX_KEY_REPEAT, SILENT_LINK_GRACE};
use crate::transport;

/// First reconnect delay after a failed connect attempt.
const RETRY_MIN: Duration = Duration::from_millis(250);
/// Backoff cap; the manager keeps retrying at this pace forever.
const RETRY_MAX: Duration = Duration::from_secs(2);
/// A connection that delivered no snapshot and died before living this long
/// is treated like a failed connect attempt: the backoff keeps growing
/// instead of resetting, so an app that accepts and immediately drops (e.g.
/// one whose snapshot always overflows [`MAX_LINE_BYTES`]) cannot pull the
/// manager into a full-CPU reconnect loop.
const HEALTHY_CONNECTION_MIN: Duration = Duration::from_secs(2);
/// Queued agent inputs awaiting the socket writer.
const INPUT_QUEUE: usize = 32;
/// Acks buffered per subscriber.
///
/// A waiter only needs the acks published while its own input is in flight,
/// but that is not one ack: an input draws up to two (a `Delivered` later
/// refined to `Ignored`), and one `key` call sends up to [`MAX_KEY_REPEAT`]
/// of them, so a single maximum burst can publish twice that many acks by
/// itself. Sized for four such bursts at once, because falling behind is not
/// a slow read that catches up: the broadcast channel drops the oldest acks,
/// and the oldest are exactly the `Dropped` answers a burst is watching for.
const ACK_QUEUE: usize = 8 * MAX_KEY_REPEAT as usize;

/// Longest a tool call waits for an on-demand bridge to reach the app before
/// it answers from whatever the bridge then knows, counted from the call's
/// start: 1.2 s.
///
/// The call that wakes an idle bridge would otherwise always find no tree,
/// since the connect it caused has not happened yet. A taria adapter accepts
/// within 50 ms and writes its `hello` and its latest snapshot the moment it
/// does, so the wait normally ends in milliseconds, on the tree. A connect
/// that fails, or a connection that ends, answers the call at once too.
///
/// What runs longer is a connection that stays silent, and that wait is
/// counted from when the connection opened, not from when the call began: it
/// ends once the connection is [`SILENT_LINK_GRACE`] old, the age at which
/// silence is diagnosed as the app being held by another bridge, so that
/// answer is the one the call gives. Counted from the call's start instead, a
/// connect that ran late (a loaded host, a reconnect that came late) left the
/// call answering about a connection too young to mean anything. This bound
/// keeps that wait finite: it covers the whole grace
/// for any connection opened within the call's first 700 ms, and a connection
/// opened later than that is answered as the young connection it is, with a
/// retry. A held app normally costs the call just over the grace, since the
/// connect into the backlog takes a millisecond.
const FIRST_CALL_WAIT: Duration = Duration::from_millis(1_200);

/// How long past the idle window an input still waiting for its ack keeps an
/// on-demand bridge holding the app.
///
/// Letting go while the app has not dequeued an input costs the input: a
/// taria adapter discards (and counts as `stale_inputs`) whatever is queued
/// from a connection once that connection ends. So the release waits for the
/// ack of every input this connection wrote. Not forever, because an app may
/// never ack at all (an adapter older than the ack contract never does), and
/// a bridge that holds the app for good is the bug the release exists to fix.
/// Thirty seconds is far past any ack a live app sends, and it is counted from
/// the end of the window, so from at least the idle window plus this after the
/// input was sent: every input is sent while a tool call is in flight.
pub(crate) const UNACKED_HOLD: Duration = Duration::from_secs(30);

/// Most unacked input ids one connection remembers.
///
/// Only whether any is outstanding matters, so the oldest is forgotten past
/// this many, which keeps the set bounded for an app that never acks. Ids
/// count up, so the smallest is the oldest. Far above anything a live app has
/// in flight at once: the bridge's own queue to it holds [`INPUT_QUEUE`].
const MAX_UNACKED: usize = 1024;

/// The reason [`BridgeHandle::rejected_rx`] holds for a line that ran past
/// [`MAX_LINE_BYTES`].
///
/// A fixed string rather than a measurement: the reader stops at the cap
/// instead of reading on to learn how long the line was, which is what the cap
/// is for. The tool layer matches on it, because this is the one rejection that
/// ends the connection, and the one an app repeats on every reconnect: it
/// resends its latest snapshot to every bridge that connects, so the same line
/// overflows again. The tests below hold its number to [`MAX_LINE_BYTES`].
pub(crate) const OVERSIZED_LINE: &str =
    "the line is over 1 MiB (1048576 bytes), this bridge's line limit";

/// One snapshot as the app published it, in both the forms the bridge needs.
///
/// They are kept together because neither can stand in for the other. The
/// **app's own line is authoritative for what the agent reads**: it is
/// forwarded as sent, so a role, an action or a field this build has never
/// heard of reaches the agent by name instead of being flattened into
/// whatever the typed struct could hold. Re-serializing
/// [`parsed`](Self::parsed) destroyed exactly that information: an app
/// publishing `"role":"sparkline"` had the agent read `"role":"other"`, and the
/// real name had been on the wire all along for the bridge to pass on. The two
/// things dropped on the way are the message's own framing key, which belongs
/// to the transport rather than to the tree, and the snapshot's stamp, which
/// belongs to the bridge; see [`from_line`](Self::from_line).
///
/// The **parsed form is authoritative for what the bridge decides**: `act`
/// validates node ids and advertised actions against it, and the tool layer
/// compares it to tell a tree that changed from one that did not. Neither of
/// those may read the bridge's copy of the line, which is untyped and, across
/// a version mismatch, not even shaped like this build's [`Snapshot`].
///
/// A line reaches here only after it parsed, so a malformed line is still
/// skipped rather than forwarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AppSnapshot {
    /// The app's snapshot as the agent reads it: its own line, minus the
    /// message framing and the stamp. What `read_tree` and every tool result
    /// hand back.
    /// Built by [`from_line`](Self::from_line), which is the only thing that
    /// removes anything.
    pub(crate) tree_json: String,
    /// The same message decoded into this build's types. For validation and
    /// change detection, never for output, except the action names in an
    /// outline, which have to be the names validation accepts.
    pub(crate) parsed: Snapshot,
}

impl AppSnapshot {
    /// Pair one app line with its parse, dropping the transport envelope and
    /// the stamp from the copy the agent reads.
    ///
    /// [`AppToBridge`] is internally tagged, so the line carries a
    /// `"type":"snapshot"` key beside the snapshot's own fields. That key
    /// frames the message on the wire and says nothing about the tree, while
    /// `read_tree` promises the app's tree, so handing it on reads as a field
    /// the app published. The stamp, `after_input`, goes too; see
    /// [`strip_envelope`].
    ///
    /// Removed through a `serde_json::Value`, which holds every key the line
    /// has, including the roles, actions and fields this build has never
    /// heard of. Only the two keys go, but the spelling of the rest becomes
    /// the serializer's again: keys sorted, whitespace gone, escapes and
    /// numbers rewritten (`1e3` as `1000.0`), and a key repeated in one object
    /// kept once, with its last value (`docs/protocol.md` section 16.6).
    pub(crate) fn from_line(line: &str, parsed: Snapshot) -> Self {
        Self {
            tree_json: strip_envelope(line),
            parsed,
        }
    }
}

/// `line` without its `"type"` key and its stamp.
///
/// The stamp (`after_input`) goes for the reason the framing key does: it
/// says when the tree was published relative to the bridge's own inputs,
/// which is for the bridge to reason about and not part of the tree. An
/// agent shown it would read an input id it never sent as a fact about the
/// app, and would see a tree that only had its stamp moved as one that
/// changed. A peer that stamps without saying so in its hello loses it too.
///
/// Both fallbacks return the line untouched, and neither is reachable from a
/// line that got here: it parsed as an `AppToBridge::Snapshot`, so it is a
/// JSON object, and a `Value` built from JSON serializes back. Passing the
/// envelope on is the wrong answer for both, but it is the one that keeps the
/// tree, which is what an agent came for.
fn strip_envelope(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
        object.remove("after_input");
    }
    serde_json::to_string(&value).unwrap_or_else(|_| line.to_string())
}

/// What the bridge currently knows about the app, as seen by the tool layer.
///
/// The distinction between [`Never`](Self::Never) and
/// [`Disconnected`](Self::Disconnected) exists purely for error messages:
/// "the app was never there" and "the app was there and died" call for very
/// different next steps from the agent.
///
/// [`Connected`](Self::Connected) holds a whole tree by value, which clippy
/// flags beside the small variants since a node gained its typed state. One
/// value lives in the watch channel and its clones are short-lived, so the
/// size costs nothing a `Box` would win back.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BridgeState {
    /// No app has delivered a snapshot since the bridge started.
    Never,
    /// An app is connected; its latest snapshot is available.
    Connected(AppSnapshot),
    /// A previously connected app went away after delivering snapshots.
    Disconnected {
        /// Label from the app's handshake, if one was received.
        app_label: Option<String>,
        /// `seq` of the last snapshot delivered before the app went away.
        last_seq: u64,
    },
}

/// What this bridge's own end of the socket is doing, as distinct from what
/// the peer has said on it.
///
/// [`BridgeState`] answers "has an app published a tree" and cannot answer
/// "did this bridge reach the socket at all". Both a `connect` that keeps
/// failing and a connection the app has not accepted (a second bridge's, until
/// the first one leaves or lets go after its idle window) leave the state at
/// [`BridgeState::Never`], and the two
/// call for opposite next steps: check the path, or leave the path alone
/// because the bridge is connected to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// No connection is open: the last connect attempt failed, or the last
    /// connection ended.
    Down,
    /// A connection is open, established at this instant.
    Open {
        /// When `connect` returned. The tool layer reads the age rather than
        /// the instant: a connection open for a moment is a handshake still
        /// arriving, and one open far longer is a socket nobody is reading.
        since: Instant,
    },
}

impl LinkState {
    /// How long the open connection has been up, or `None` when none is.
    pub(crate) fn open_for(self) -> Option<Duration> {
        match self {
            Self::Down => None,
            Self::Open { since } => Some(since.elapsed()),
        }
    }
}

/// Source of the [`InputId`]s the tool layer stamps on outgoing inputs.
///
/// Process-wide and monotonic, which is what the protocol asks for: an id must
/// not repeat for the lifetime of the bridge process, not merely for the
/// lifetime of one connection. Acks outlive the connection they were sent on,
/// so a scheme that restarts its ids per connection lets a dead app's in-flight
/// ack answer a live waiter holding the same id. Counting up across every
/// connection makes a leftover ack trivially non-matching, since no waiter is
/// ever looking for an id that low again. [`InputId`] is the normative
/// statement of the rule.
static NEXT_INPUT_ID: AtomicU64 = AtomicU64::new(1);

/// Handles the MCP tool layer uses to talk to the socket-manager task.
#[derive(Clone)]
pub(crate) struct BridgeHandle {
    /// Latest connection state; holds the current [`Snapshot`] while an app
    /// is connected.
    pub(crate) state_rx: watch::Receiver<BridgeState>,
    /// Queue of identified agent inputs to forward to the app.
    pub(crate) input_tx: mpsc::Sender<(InputId, AgentInput)>,
    /// Every ack the app sends, republished for whoever is waiting on one.
    pub(crate) ack_tx: broadcast::Sender<(InputId, InputStatus)>,
    /// The `protocol_version` the connected app declared in its handshake.
    ///
    /// `None` before the peer has said anything, and again once the app goes
    /// away, so a stale version is never read as the current peer's. A peer
    /// that sends a snapshot before any `Hello` is taken at the version that
    /// snapshot carries, and a `Hello` later on the same connection replaces
    /// it. Every taria adapter has greeted the bridge since its first
    /// release, so this is for hand-written peers; until 0.3.3 such a peer
    /// left the version `None`, which reads as "no mismatch known", and
    /// input went to a peer on another version.
    ///
    /// The tool layer needs this because a peer on another protocol version
    /// still delivers parseable snapshots while being unable to parse the
    /// inputs sent back to it, and reporting an input as sent to such a peer
    /// would be a lie.
    pub(crate) protocol_rx: watch::Receiver<Option<u32>>,
    /// Why the most recent line from an app was thrown away, if one was.
    ///
    /// Set for every line the reader could not turn into an [`AppToBridge`],
    /// and kept past the connection it arrived on, until the next connection
    /// opens, because the question it answers outlives the connection: an app
    /// that connects, publishes nothing this bridge can read and dies leaves
    /// the state at [`BridgeState::Never`], which on its own is
    /// indistinguishable from an app that never started. Reporting the reason
    /// is what stops `read_tree` sending an adopter to check a socket path the
    /// app plainly reached. A successful `connect` clears it, before the link
    /// reads as open, so a reason seen while a connection is open came from
    /// that connection: a restarted app is never blamed for a line its
    /// previous instance sent.
    ///
    /// Also set for a line over [`MAX_LINE_BYTES`], which ends the connection
    /// instead of being skipped. That reason is a fixed string that a connect
    /// does not clear, and it is retired on either of two events. The next
    /// snapshot that parses retires it, because the tree that overflowed has
    /// then been replaced by one that fits. A `connect` that finds nothing
    /// listening retires it too, because the app that sent the tree is gone,
    /// and "it resends that tree on every reconnect" is no longer true of
    /// anything. Short of either it is kept across reconnects, since a running
    /// app resends the same tree to every connection and it overflows every
    /// time.
    ///
    /// Read only where no snapshot is available. Once a snapshot has arrived,
    /// a later rejected line says nothing about the tree being served, and
    /// the tools do not mention it.
    pub(crate) rejected_rx: watch::Receiver<Option<String>>,
    /// Whether this bridge currently holds an open connection, and since when.
    ///
    /// The one fact that separates "nothing is there" from "this bridge is
    /// connected and the peer has not said a word", which are the same
    /// [`BridgeState::Never`] and want opposite answers. See [`LinkState`].
    ///
    /// Read only where no snapshot is available, like
    /// [`rejected_rx`](Self::rejected_rx): once a tree is being served, the
    /// connection carrying it is not what an agent needs to hear about.
    pub(crate) link_rx: watch::Receiver<LinkState>,
    /// Whether the connected app said in its hello that it stamps its
    /// snapshots (`after_input`, the newest input it dequeued).
    ///
    /// False until a hello says otherwise, and again once the connection
    /// ends, like [`protocol_rx`](Self::protocol_rx), so a peer is never
    /// judged by the previous one's hello. The tool layer reads it to decide
    /// how long an input waits for its tree and what the answer says: a peer
    /// that stamps can be told "did not change" the moment a tree stamped at
    /// the input arrives unchanged, and one that does not is answered as
    /// every peer was before stamps existed.
    pub(crate) stamps_rx: watch::Receiver<bool>,
    /// Held across claiming an input's id and queueing the input, so ids
    /// reach the socket in the order they were claimed, however tool calls
    /// overlap. See [`send`](Self::send).
    pub(crate) send_order: Arc<tokio::sync::Mutex<()>>,
}

/// Why [`BridgeHandle::send`] sent nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendError {
    /// The socket task is gone, so nothing will ever read the queue.
    Closed,
    /// The queue to the app stayed full for the whole wait.
    Full,
}

impl BridgeHandle {
    /// Claim the id for the next input to send.
    ///
    /// Takes `&self` so the id source stays an implementation detail of the
    /// handle: callers ask the bridge for an id rather than reaching for a
    /// counter of their own, and two tool calls racing still get different
    /// ids.
    pub(crate) fn next_input_id(&self) -> InputId {
        InputId::new(NEXT_INPUT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Claim the next id and queue `input` under it for the socket task,
    /// within `wait` in all, returning the id.
    ///
    /// The id is claimed and the input queued under one lock, so the ids go
    /// out on the socket in the order they were claimed. A stamped tree is
    /// credited to an input by comparing ids, and that comparison is only
    /// sound in the order the app received them: with the two steps apart,
    /// two calls racing could put id 8 on the wire before id 7, and a tree
    /// stamped 8 would be credited to 7 before the app had read it.
    ///
    /// The wait covers the lock too. Nothing holds the lock but a send
    /// waiting for room in the queue, so running out of time on either is
    /// the same full queue. A send that runs out of time claimed its id and
    /// queued nothing, which leaves a gap in the ids, and the protocol asks
    /// only that they never repeat.
    pub(crate) async fn send(
        &self,
        input: AgentInput,
        wait: Duration,
    ) -> Result<InputId, SendError> {
        let send = async {
            let _order = self.send_order.lock().await;
            let id = self.next_input_id();
            self.input_tx
                .send((id, input))
                .await
                .map(|()| id)
                .map_err(|_| SendError::Closed)
        };
        tokio::time::timeout(wait, send)
            .await
            .unwrap_or(Err(SendError::Full))
    }

    /// Start receiving acks from now on.
    ///
    /// A broadcast receiver only sees what is published after it subscribes,
    /// so a caller that waits on an input must subscribe *before* sending it:
    /// the app can ack before the sending task is scheduled again.
    pub(crate) fn subscribe_acks(&self) -> broadcast::Receiver<(InputId, InputStatus)> {
        self.ack_tx.subscribe()
    }
}

/// Spawn the socket-manager task for `socket_path` on the current runtime.
///
/// The manager connects at once and holds the app for as long as the returned
/// handle (or a clone of its `input_tx`) lives, reconnecting whenever the
/// connection ends. That is every bridge before 0.3.2, and what a library
/// caller gets from
/// [`TariaMcpServerBuilder::connected`](crate::TariaMcpServerBuilder::connected).
/// The `taria-mcp` binary connects on demand instead, through
/// [`TariaMcpServerBuilder::on_demand`](crate::TariaMcpServerBuilder::on_demand),
/// so that a session with no tool calls in flight does not keep every other
/// session away from the app.
pub(crate) fn spawn(socket_path: PathBuf) -> BridgeHandle {
    spawn_manager(socket_path, None)
}

/// Spawn a socket manager that connects only while tool calls want the app.
///
/// It opens no connection until the first [`Demand::begin`], and lets go of
/// the app once no call has run for `idle_release`: it closes its connection
/// (an accepted one, or one still waiting in the listen backlog, which leaves
/// the backlog with it) and makes no further connect attempt until the next
/// call. [`Duration::ZERO`] never lets go once connected. The handle is the
/// same as [`spawn`]'s; the [`Demand`] is how the tool layer reports its
/// calls.
pub(crate) fn spawn_on_demand(
    socket_path: PathBuf,
    idle_release: Duration,
) -> (BridgeHandle, Demand) {
    let (idle, demand) = idle_and_demand(idle_release);
    (spawn_manager(socket_path, Some(idle)), demand)
}

/// The two halves of an on-demand manager's activity window: the manager's,
/// and the tool layer's.
fn idle_and_demand(idle_release: Duration) -> (Idle, Demand) {
    let (activity_tx, activity_rx) = watch::channel(Activity::default());
    let activity = Arc::new(activity_tx);
    let idle = Idle {
        activity: Arc::clone(&activity),
        rx: activity_rx,
        release_after: idle_release,
        calls_answered: 0,
    };
    (idle, Demand { activity })
}

/// The channels both kinds of manager share, and the task serving them.
fn spawn_manager(socket_path: PathBuf, idle: Option<Idle>) -> BridgeHandle {
    let (state_tx, state_rx) = watch::channel(BridgeState::Never);
    let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
    let (ack_tx, _) = broadcast::channel(ACK_QUEUE);
    let (protocol_tx, protocol_rx) = watch::channel(None);
    let (rejected_tx, rejected_rx) = watch::channel(None);
    let (link_tx, link_rx) = watch::channel(LinkState::Down);
    let (stamps_tx, stamps_rx) = watch::channel(false);
    tokio::spawn(manager_loop(
        socket_path,
        state_tx,
        input_rx,
        ack_tx.clone(),
        protocol_tx,
        rejected_tx,
        link_tx,
        stamps_tx,
        idle,
    ));
    BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
        protocol_rx,
        rejected_rx,
        link_rx,
        stamps_rx,
        send_order: Arc::new(tokio::sync::Mutex::new(())),
    }
}

/// What the tool calls and an on-demand manager share.
///
/// Kept in one [`watch`] channel because its lock is what makes the release
/// safe: a call counts itself in, and the manager decides to let go, each
/// under that one lock, so either the manager sees the call and stays, or the
/// call sees the manager gone ([`awake`](Self::awake) false) and waits for it
/// to come back with a fresh connection. A call can never send input to a
/// connection the manager has already decided to close. The change
/// notification is what wakes each side for the other.
#[derive(Debug, Default)]
struct Activity {
    /// Tool calls running now.
    in_flight: usize,
    /// When the last tool call ended, if any has. The idle window runs from
    /// here.
    ///
    /// On tokio's clock, like every instant the idle window is reckoned in:
    /// the waits on it are tokio timers, and judging them by another clock
    /// is what lets a timer fire before the instant it was set for has come.
    /// Under a paused test clock that is a loop that wakes, finds the window
    /// still open by the wall clock, and sleeps until a deadline tokio has
    /// already passed.
    last_end: Option<TokioInstant>,
    /// Whether the manager wants the app: connecting, connected, or between
    /// attempts. False before the first call and after every release.
    ///
    /// Set true by the manager only once it has finished cleaning up after
    /// the last release, so a call that finds it true can trust the bridge's
    /// other channels to describe the current connection, not the one that
    /// was let go.
    awake: bool,
    /// Tool calls begun since the start. A manager between reconnect
    /// attempts watches it to cut its backoff short.
    calls: u64,
    /// Connect attempts that failed, and connections that ended, since the
    /// start: the events that answer a waiting call when no tree does.
    outcomes: u64,
    /// Whether the departure the bridge's [`BridgeState::Disconnected`]
    /// record names was this bridge letting go of the app after its idle
    /// window, rather than the app leaving.
    ///
    /// The tool layer reads it to tell an agent that the disconnect in front
    /// of a held answer was this bridge's own doing: agents read "app 'X'
    /// disconnected" as a fault, and went after it with `ss`, `ps` and
    /// `pgrep`. Written where that record is (see [`Idle::note_disconnect`]), so
    /// it describes the same departure, and cleared once a connection serves
    /// a tree, so it can never outlive the record it is about. Cleared too
    /// when a connect finds nothing listening, or finds another process
    /// listening than the one let go of (see
    /// [`released_pid`](Self::released_pid)): the app has gone since, so
    /// the departure is no longer only this bridge's doing.
    pub(crate) released: bool,
    /// The [`peer_pid`](Self::peer_pid) of the connection a release let go
    /// of, kept with [`released`](Self::released) while the kernel named
    /// one.
    ///
    /// An app that restarts while this bridge makes no call leaves nothing
    /// for a connect to fail on, and once another bridge holds the new
    /// instance, this one's next connect lands in its backlog: the silence
    /// the held answer explains, which went on calling the departure this
    /// bridge's own. The pid is how that connect tells the instance it let
    /// go of from one that replaced it. Unknown on either side, the flag
    /// stands.
    pub(crate) released_pid: Option<i32>,
    /// The process at the other end of the open connection, as the kernel
    /// reported it at `connect`, or `None` with no connection open or no pid
    /// reported. For a connection still waiting in the listen backlog that is
    /// the process listening at the path, which is how the tool layer tells a
    /// held app from a stopped one: both leave the connection silent.
    pub(crate) peer_pid: Option<i32>,
}

/// Whether `activity` is inside the idle window: a call in flight, or less
/// than `release_after` since the last one ended. [`Duration::ZERO`] keeps the
/// window open for good once a call has run.
fn window_open(activity: &Activity, release_after: Duration) -> bool {
    activity.in_flight > 0
        || activity
            .last_end
            .is_some_and(|end| release_after.is_zero() || end.elapsed() < release_after)
}

/// When a manager with the idle window `release_after` may let go: the end of
/// the window, or [`UNACKED_HOLD`] past it while an input still awaits its
/// ack. `None` while a call is in flight, and for good with a zero window (or
/// one too long to reach).
fn release_at(activity: &Activity, release_after: Duration, unacked: bool) -> Option<TokioInstant> {
    if release_after.is_zero() || activity.in_flight > 0 {
        return None;
    }
    let window_end = activity.last_end?.checked_add(release_after)?;
    if unacked {
        window_end.checked_add(UNACKED_HOLD)
    } else {
        Some(window_end)
    }
}

/// The tool layer's handle on an on-demand manager.
#[derive(Clone)]
pub(crate) struct Demand {
    activity: Arc<watch::Sender<Activity>>,
}

impl Demand {
    /// Count one tool call in, waking the manager if it is idle.
    ///
    /// The call stays counted until the returned [`Call`] drops, which is
    /// also what happens to a call that is cancelled, so a cancelled call
    /// still starts the idle window.
    pub(crate) fn begin(&self) -> Call {
        let mut outcomes = 0;
        self.activity.send_modify(|activity| {
            activity.in_flight += 1;
            activity.calls += 1;
            outcomes = activity.outcomes;
        });
        Call {
            activity: Arc::clone(&self.activity),
            outcomes,
        }
    }

    /// Whether the departure the bridge's `Disconnected` record names was
    /// this bridge letting go after its idle window. See
    /// [`Activity::released`].
    pub(crate) fn released(&self) -> bool {
        self.activity.borrow().released
    }

    /// The pid of the process at the other end of the open connection, if
    /// the kernel reported one. See [`Activity::peer_pid`].
    pub(crate) fn peer_pid(&self) -> Option<i32> {
        self.activity.borrow().peer_pid
    }
}

/// One tool call, counted in the activity window until it drops.
///
/// Holds no borrow, only a share of the channel, so dropping it anywhere is
/// sound, cancellation included.
pub(crate) struct Call {
    activity: Arc<watch::Sender<Activity>>,
    /// [`Activity::outcomes`] when this call began: a higher count is an
    /// attempt that settled after it.
    outcomes: u64,
}

impl Call {
    /// Wait until the bridge has an answer for this call: a snapshot, a
    /// connect that failed, a connection that ended, or a connection that has
    /// been open for [`SILENT_LINK_GRACE`] without one (the app held by
    /// another bridge, or greeting and publishing nothing). Never longer than
    /// [`FIRST_CALL_WAIT`] from the call's start.
    ///
    /// The grace is counted from when the connection opened, so a connect
    /// that ran late still gets all of it, and the tool layer, which reads
    /// the connection's age again after this returns, sees it past the grace
    /// too. Only a connection opened in the bound's last 500 ms is still
    /// young when the call answers.
    ///
    /// Returns at once while a tree is being served, so only the call that
    /// wakes an idle bridge, or finds it between attempts, waits at all. The
    /// bridge's other channels are read only once the manager is awake, since
    /// until then they describe the connection it let go of, and the snapshot
    /// left from that one is not an answer.
    pub(crate) async fn settle(&self, bridge: &BridgeHandle) {
        let deadline = TokioInstant::now() + FIRST_CALL_WAIT;
        let mut activity = self.activity.subscribe();
        let mut state = bridge.state_rx.clone();
        let mut link = bridge.link_rx.clone();
        loop {
            let (awake, outcomes) = {
                let now = activity.borrow_and_update();
                (now.awake, now.outcomes)
            };
            let connected = matches!(&*state.borrow_and_update(), BridgeState::Connected(_));
            let open_for = link.borrow_and_update().open_for();
            let mut wake_at = deadline;
            if awake {
                if connected || outcomes != self.outcomes {
                    return;
                }
                if let Some(open_for) = open_for {
                    // The age is the link's own, on the wall clock its
                    // `since` was taken from; what is left of the grace is
                    // then slept on tokio's. Never an instant converted from
                    // one clock to the other, which a paused test clock turns
                    // into a timer that has fired before the wall clock
                    // agrees, and a loop around it.
                    match SILENT_LINK_GRACE.checked_sub(open_for) {
                        Some(left) if !left.is_zero() => {
                            wake_at = wake_at.min(TokioInstant::now() + left);
                        }
                        _ => return,
                    }
                }
            }
            // A closed channel means the manager is gone, and nothing it
            // could still say is worth the rest of the wait.
            tokio::select! {
                changed = activity.changed() => if changed.is_err() { return },
                changed = state.changed() => if changed.is_err() { return },
                changed = link.changed() => if changed.is_err() { return },
                () = tokio::time::sleep_until(wake_at) => {}
            }
            if TokioInstant::now() >= deadline {
                return;
            }
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.activity.send_modify(|activity| {
            activity.in_flight = activity.in_flight.saturating_sub(1);
            activity.last_end = Some(TokioInstant::now());
        });
    }
}

/// The manager's half of a [`Demand`]: when to connect and when to let go.
struct Idle {
    activity: Arc<watch::Sender<Activity>>,
    /// The manager's own view of the activity, for waiting on it.
    rx: watch::Receiver<Activity>,
    /// The idle window; [`Duration::ZERO`] never lets go.
    release_after: Duration,
    /// [`Activity::calls`] as of the last [`settled`](Self::settled): every
    /// call begun after it is one that outcome did not answer, and cuts the
    /// pause that follows it short.
    calls_answered: u64,
}

/// How a pause between connect attempts ended.
enum Pause {
    /// The backoff ran out, or a tool call cut it short: connect again.
    Over,
    /// The idle window closed with nothing connected: stop trying. Carries
    /// when the last tool call ended, for the log line that says so.
    Released(TokioInstant),
}

impl Idle {
    /// Sleep until a tool call wants the app, then mark the manager awake.
    /// Returns false when the session ended instead.
    ///
    /// Inputs that reach the queue meanwhile were not sent by a tool call
    /// (every call wakes the manager first and refuses to send without a
    /// tree), and have no app to go to, so they are dropped here exactly as
    /// a reconnect drops them.
    async fn until_wanted(&mut self, input_rx: &mut mpsc::Receiver<(InputId, AgentInput)>) -> bool {
        let release_after = self.release_after;
        loop {
            let woke = self.activity.send_if_modified(|activity| {
                if window_open(activity, release_after) {
                    activity.awake = true;
                    true
                } else {
                    false
                }
            });
            if woke {
                return true;
            }
            tokio::select! {
                changed = self.rx.changed() => if changed.is_err() { return false },
                input = input_rx.recv() => match input {
                    None => return false,
                    Some(_) => tracing::warn!(
                        "dropped an agent input queued while the bridge held no connection"
                    ),
                },
            }
        }
    }

    /// Resolve once [`release_at`] has passed. Recomputed
    /// on every change in activity, since every call that begins or ends
    /// moves it; the caller recreates this future after anything else that
    /// moves it (an ack arriving, an input going out).
    async fn release_due(&mut self, unacked: bool) {
        loop {
            let at = release_at(&self.rx.borrow_and_update(), self.release_after, unacked);
            let wait = async {
                match at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = wait => return,
                changed = self.rx.changed() => if changed.is_err() {
                    std::future::pending::<()>().await;
                },
            }
        }
    }

    /// Decide to let go, under the same lock a call counts itself in under.
    ///
    /// `Some` when the manager is now released and must close what it holds,
    /// carrying when the last tool call ended (read under that lock, for the
    /// log line). `None` when it stays: a call is in flight, or arrived first,
    /// or the window has not closed.
    fn try_release(&self, unacked: bool) -> Option<TokioInstant> {
        let release_after = self.release_after;
        let mut quiet_since = None;
        self.activity.send_if_modified(|activity| {
            match (
                release_at(activity, release_after, unacked),
                activity.last_end,
            ) {
                (Some(at), Some(last_end)) if TokioInstant::now() >= at => {
                    activity.awake = false;
                    quiet_since = Some(last_end);
                    true
                }
                _ => false,
            }
        });
        quiet_since
    }

    /// Change what the activity says about the connection without waking
    /// anyone: these facts answer a tool call's question once it asks, and
    /// neither a call waiting in [`Call::settle`] nor the manager's own
    /// waits have anything to recompute over them.
    fn note(&self, change: impl FnOnce(&mut Activity)) {
        self.activity.send_if_modified(|activity| {
            change(activity);
            false
        });
    }

    /// Record how a connection ended, beside the `Disconnected` record the
    /// manager is about to write for it: before that record, so a tool call
    /// that reads the record and then this finds the two in step.
    ///
    /// Only a connection that served a tree writes the record, so only that
    /// one says whether its end was a release. A connection the app closed
    /// before serving one clears the flag anyway: something happened to the
    /// app after the release, and "it is not a fault" is no longer a claim
    /// this bridge can make. A release of a connection that served nothing
    /// (one held in the backlog, say) leaves it, since the record it is
    /// about is unchanged.
    fn note_disconnect(&self, delivered_snapshot: bool, end: &ConnectionEnd) {
        self.note(|activity| {
            let peer_pid = activity.peer_pid.take();
            if delivered_snapshot {
                activity.released = matches!(end, ConnectionEnd::Released(_));
                activity.released_pid = peer_pid.filter(|_| activity.released);
            } else if matches!(end, ConnectionEnd::AppClosed { .. }) {
                activity.released = false;
            }
        });
    }

    /// Record the process at the other end of a connection just made, as
    /// the kernel named it, and retire the release flag when that is not the
    /// process the release let go of: the app has been replaced since, so
    /// its departure is no longer only this bridge's doing. See
    /// [`Activity::released_pid`].
    fn note_connect(&self, peer_pid: Option<i32>) {
        self.note(|activity| {
            activity.peer_pid = peer_pid;
            if let (Some(let_go), Some(now)) = (activity.released_pid, peer_pid)
                && let_go != now
            {
                activity.released = false;
            }
        });
    }

    /// Record a failed connect or an ended connection, which answers any
    /// call waiting in [`Call::settle`]. The calls it answered are read under
    /// the same lock, so a call that begins a moment later still cuts the
    /// pause after it short instead of waiting the pause out.
    fn settled(&mut self) {
        let mut calls = 0;
        self.activity.send_modify(|activity| {
            activity.outcomes += 1;
            calls = activity.calls;
        });
        self.calls_answered = calls;
    }

    /// Wait out `backoff` after a [`settled`](Self::settled) outcome. A tool
    /// call begun since that outcome cuts it short, and the idle window
    /// closing ends it with a release.
    async fn pause(&mut self, backoff: Duration) -> Pause {
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            let at = {
                let activity = self.rx.borrow_and_update();
                if activity.calls != self.calls_answered {
                    return Pause::Over;
                }
                release_at(&activity, self.release_after, false)
            };
            let due = async {
                match at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = &mut sleep => return Pause::Over,
                changed = self.rx.changed() => if changed.is_err() {
                    (&mut sleep).await;
                    return Pause::Over;
                },
                () = due => if let Some(quiet_since) = self.try_release(false) {
                    return Pause::Released(quiet_since);
                },
            }
        }
    }
}

/// Wait out `backoff` between connect attempts, the on-demand way when there
/// is an [`Idle`], and as a plain sleep for an eager manager.
async fn pause(idle: Option<&mut Idle>, backoff: Duration) -> Pause {
    match idle {
        Some(idle) => idle.pause(backoff).await,
        None => {
            tokio::time::sleep(backoff).await;
            Pause::Over
        }
    }
}

/// Why one served connection ended.
enum ConnectionEnd {
    /// The app closed the socket (or an I/O error occurred), with this many
    /// inputs the connection wrote still awaiting their ack: past the idle
    /// window, the ones it was being held for.
    AppClosed { unacked: usize },
    /// Every [`BridgeHandle::input_tx`] clone was dropped: the MCP session is
    /// over, so the manager should exit instead of reconnecting.
    SessionClosed,
    /// An on-demand manager let go of the app after its idle window.
    Released(Release),
}

/// When an on-demand manager let go of the app, and what held it past the end
/// of its idle window, for the log line that says so.
#[derive(Clone, Copy, Debug)]
struct Release {
    /// When the last tool call ended: the idle window ran from here.
    quiet_since: TokioInstant,
    /// What held the app past the end of the window, if anything did.
    hold: Hold,
}

/// What an input awaiting its ack did to a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    /// Nothing: no input was awaiting its ack when the window closed, and
    /// the release came at the end of the window.
    None,
    /// An input was still awaiting its ack when the window closed, and the
    /// release came when the app acked the last of them.
    UntilAcked,
    /// Inputs were still unacked [`UNACKED_HOLD`] past the window, and the
    /// release came anyway, with this many unacked.
    Expired { unacked: usize },
    /// Inputs were still awaiting their ack past the window, and the app
    /// closed the connection before acking them, with this many unacked: the
    /// app leaving cut the hold short.
    AppClosed { unacked: usize },
}

/// Connect-serve-reconnect until the session is torn down. With an [`Idle`],
/// only while tool calls want the app: it starts asleep, and goes back to
/// sleep whenever the idle window closes.
#[allow(clippy::too_many_arguments)]
async fn manager_loop(
    path: PathBuf,
    state_tx: watch::Sender<BridgeState>,
    mut input_rx: mpsc::Receiver<(InputId, AgentInput)>,
    ack_tx: broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: watch::Sender<Option<u32>>,
    rejected_tx: watch::Sender<Option<String>>,
    link_tx: watch::Sender<LinkState>,
    stamps_tx: watch::Sender<bool>,
    mut idle: Option<Idle>,
) {
    let mut backoff = RETRY_MIN;
    'asleep: loop {
        if let Some(idle) = idle.as_mut() {
            if !idle.until_wanted(&mut input_rx).await {
                tracing::debug!("session ended while the bridge was idle; bridge task exiting");
                return;
            }
            tracing::debug!(path = %path.display(), "a tool call wants the app; connecting");
            backoff = RETRY_MIN;
        }
        loop {
            match transport::connect(&path).await {
                Ok(stream) => {
                    tracing::info!(path = %path.display(), "connected to app socket");
                    drain_stale_inputs(&mut input_rx);
                    // A reason a line was rejected belongs to the connection
                    // it arrived on, and that connection is over: kept, it
                    // would be quoted against this one, blaming a restarted
                    // app for its previous instance's line. Only this task
                    // writes the reason, and it does so before the link
                    // below reads as open, so a reason seen while the link
                    // is open is about the open connection. The one
                    // exception is the oversized line, which an app resends
                    // to every connection (see `rejected_rx`).
                    forget_rejected_except_oversized(&rejected_tx);
                    if let Some(idle) = idle.as_ref() {
                        // For a connection the app has not accepted yet, the
                        // process listening at the path.
                        idle.note_connect(transport::peer_pid(&stream));
                    }
                    let connected_at = Instant::now();
                    // Published before the connection is served, not after it
                    // has said something: the whole point of this channel is
                    // the connection that never says anything.
                    link_tx.send_replace(LinkState::Open {
                        since: connected_at,
                    });
                    let mut conn_label = None;
                    let end = serve_connection(
                        stream,
                        &state_tx,
                        &mut input_rx,
                        &ack_tx,
                        &protocol_tx,
                        &rejected_tx,
                        &stamps_tx,
                        &mut conn_label,
                        idle.as_mut(),
                    )
                    .await;
                    // The connection is over: nothing is open again until the
                    // next `connect` returns.
                    link_tx.send_replace(LinkState::Down);
                    // The peer's protocol version belongs to the connection
                    // that declared it; forget it here so the next connection
                    // is never judged by the previous app's handshake. The
                    // same goes for whether it stamps its snapshots.
                    protocol_tx.send_replace(None);
                    stamps_tx.send_replace(false);
                    // The watch only ever holds `Connected` while this
                    // connection was being served (it is demoted below after
                    // every connection), so `Connected` here means this
                    // connection delivered a snapshot.
                    let delivered_snapshot =
                        matches!(&*state_tx.borrow(), BridgeState::Connected(_));
                    if let Some(idle) = idle.as_ref() {
                        idle.note_disconnect(delivered_snapshot, &end);
                    }
                    // Demote `Connected` to `Disconnected`, remembering which
                    // app died and its last seq. A connection that died before
                    // delivering a snapshot leaves the previous state (`Never`,
                    // or the `Disconnected` record of an older app) untouched.
                    // A release is demoted the same way, so that if the app
                    // has really gone by the next call, that call says so in
                    // the words a departure always gets.
                    state_tx.send_if_modified(|state| {
                        if let BridgeState::Connected(snapshot) = state {
                            *state = BridgeState::Disconnected {
                                app_label: conn_label.take(),
                                last_seq: snapshot.parsed.seq,
                            };
                            true
                        } else {
                            false
                        }
                    });
                    match end {
                        ConnectionEnd::AppClosed { unacked } => {
                            if let Some(idle) = idle.as_mut() {
                                // Counted only now, with the channels above
                                // already describing the ended connection,
                                // since the count is what tells a waiting
                                // call to read them.
                                idle.settled();
                                // The window can have closed already, while
                                // the connection was held for an ack. The app
                                // leaving ends that hold, and a reconnect now
                                // would take the app again for a session that
                                // is not using it, only to let go of it at
                                // once. Checked before the reconnect for that
                                // reason, a healthy connection's included.
                                if let Some(quiet_since) = idle.try_release(false) {
                                    tracing::info!(path = %path.display(), "app disconnected");
                                    let (hold, what) = app_closed_release(unacked);
                                    log_release(
                                        &path,
                                        idle.release_after,
                                        Release { quiet_since, hold },
                                        what,
                                    );
                                    continue 'asleep;
                                }
                            }
                            tracing::warn!(path = %path.display(), "app disconnected; reconnecting");
                        }
                        ConnectionEnd::SessionClosed => {
                            tracing::debug!("all input senders dropped; bridge task exiting");
                            return;
                        }
                        ConnectionEnd::Released(release) => {
                            let release_after = idle
                                .as_ref()
                                .map_or(Duration::ZERO, |idle| idle.release_after);
                            log_release(
                                &path,
                                release_after,
                                release,
                                "closed its connection to the app",
                            );
                            continue 'asleep;
                        }
                    }
                    if delivered_snapshot || connected_at.elapsed() >= HEALTHY_CONNECTION_MIN {
                        // A healthy connection: reconnect eagerly, from
                        // scratch.
                        backoff = RETRY_MIN;
                    } else {
                        // Accepted but died young without a snapshot: back
                        // off exactly like a failed connect, or a peer that
                        // always accept-and-drops spins this loop at full CPU.
                        tracing::debug!(
                            retry_in_ms = backoff.as_millis() as u64,
                            "connection died without a snapshot; backing off"
                        );
                        if let Pause::Released(quiet_since) = pause(idle.as_mut(), backoff).await {
                            log_pause_release(&path, idle.as_ref(), quiet_since);
                            continue 'asleep;
                        }
                        backoff = (backoff * 2).min(RETRY_MAX);
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        path = %path.display(),
                        %err,
                        retry_in_ms = backoff.as_millis() as u64,
                        "connect failed; is the app running?"
                    );
                    // An app that overflowed and then exited leaves its reason
                    // behind, and the tools would go on telling the agent that
                    // a running app resends the same tree to every reconnect,
                    // about an app that is not there. Nothing listening is the
                    // one connect failure that says the app is gone.
                    forget_oversized_if_nothing_listens(&err, &rejected_tx);
                    if let Some(idle) = idle.as_mut() {
                        // The release flag goes the same way, and for the
                        // same reason: the app this bridge let go of has
                        // since gone, so "it is not a fault" no longer
                        // describes what happened to it, and a held answer
                        // about the next instance must not say it. Cleared
                        // before the outcome is counted, which is what a
                        // waiting call reads it after.
                        if transport::nothing_listens(&err) {
                            idle.note(|activity| activity.released = false);
                        }
                        idle.settled();
                    }
                    if let Pause::Released(quiet_since) = pause(idle.as_mut(), backoff).await {
                        log_pause_release(&path, idle.as_ref(), quiet_since);
                        continue 'asleep;
                    }
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }
    }
}

/// Say that an on-demand manager let go of the app, after how long with no
/// tool call, and what held it past its idle window if anything did: info
/// level, except a release with inputs still unacked, which the app may now
/// discard.
///
/// The idle time is measured, not the window restated: an input awaiting its
/// ack holds the app past the window, by up to [`UNACKED_HOLD`], and a log
/// that said "60 s" for a release at 75 s sent whoever read it looking for
/// the missing 15.
fn log_release(path: &std::path::Path, release_after: Duration, release: Release, what: &str) {
    let idle = release.quiet_since.elapsed();
    let line = release_line(idle, release_after, release.hold, what);
    let idle_s = idle.as_secs_f64();
    let idle_release_s = release_after.as_secs_f64();
    match release.hold {
        Hold::None | Hold::UntilAcked => tracing::info!(
            path = %path.display(),
            idle_s,
            idle_release_s,
            "{line}"
        ),
        Hold::Expired { unacked } => tracing::warn!(
            path = %path.display(),
            idle_s,
            idle_release_s,
            unacked,
            "{line}"
        ),
        Hold::AppClosed { unacked } => tracing::info!(
            path = %path.display(),
            idle_s,
            idle_release_s,
            unacked,
            "{line}"
        ),
    }
}

/// The words [`log_release`] says a release in, after `idle` with no tool
/// call against the window `release_after`.
fn release_line(idle: Duration, release_after: Duration, hold: Hold, what: &str) -> String {
    let idle_s = idle.as_secs_f64();
    let idle_release_s = release_after.as_secs_f64();
    let past_s = idle.saturating_sub(release_after).as_secs_f64();
    match hold {
        Hold::None => format!(
            "no tool call for {idle_s:.1} s: released the app ({what}); the next tool call \
             reconnects"
        ),
        Hold::UntilAcked => format!(
            "no tool call for {idle_s:.1} s, {past_s:.1} s past the {idle_release_s} s idle \
             window, held there until the app acked every input it was sent: released the app \
             ({what}); the next tool call reconnects"
        ),
        Hold::Expired { unacked } => format!(
            "no tool call for {idle_s:.1} s, {past_s:.1} s past the {idle_release_s} s idle \
             window, the longest an input awaiting its ack holds the app: released the app \
             ({what}) with {unacked} input(s) never acked, which the app may discard; the next \
             tool call reconnects"
        ),
        Hold::AppClosed { unacked } => format!(
            "no tool call for {idle_s:.1} s, {past_s:.1} s past the {idle_release_s} s idle \
             window, held there while {unacked} input(s) awaited the app's ack: released the \
             app ({what}); the next tool call reconnects"
        ),
    }
}

/// What held a release that came because the app closed the connection with
/// the idle window already closed, and what the log says ended it.
///
/// With the window closed, an input still awaiting its ack is what the
/// connection was being held for, and the log says the app's leaving ended
/// that hold rather than calling it a plain release. With none, nothing held
/// it: the app left at about the moment the window closed.
fn app_closed_release(unacked: usize) -> (Hold, &'static str) {
    if unacked > 0 {
        (
            Hold::AppClosed { unacked },
            "the app closed the connection during the ack hold; not reconnecting",
        )
    } else {
        (
            Hold::None,
            "the app closed the connection; not reconnecting",
        )
    }
}

/// [`log_release`] for a manager that let go between connect attempts, which
/// had no connection to hold an input on.
fn log_pause_release(path: &std::path::Path, idle: Option<&Idle>, quiet_since: TokioInstant) {
    let release_after = idle.map_or(Duration::ZERO, |idle| idle.release_after);
    let release = Release {
        quiet_since,
        hold: Hold::None,
    };
    log_release(path, release_after, release, "stopped reconnecting");
}

/// Retire [`OVERSIZED_LINE`] when a failed `connect` means nothing is
/// listening at the path: a socket file nobody is bound to any more
/// (`ConnectionRefused`), or no file at all (`NotFound`), as
/// [`transport::nothing_listens`] tells them. Either way the app that sent the
/// tree has gone. Any other failure (a permission, a resource limit) says
/// nothing about whether the app is still there, and leaves the reason alone.
fn forget_oversized_if_nothing_listens(
    err: &io::Error,
    rejected_tx: &watch::Sender<Option<String>>,
) {
    if transport::nothing_listens(err) {
        forget_oversized(rejected_tx);
    }
}

/// Retire every reason but [`OVERSIZED_LINE`], the counterpart of
/// [`forget_oversized`], for a connection that has just opened: what the last
/// connection's lines failed on says nothing about this one's, while an
/// oversized tree is resent to every connection and overflows on each.
fn forget_rejected_except_oversized(rejected_tx: &watch::Sender<Option<String>>) {
    rejected_tx.send_if_modified(|rejected| {
        if rejected.is_some() && !is_oversized_reason(rejected.as_deref()) {
            *rejected = None;
            true
        } else {
            false
        }
    });
}

/// True when `rejected` is [`OVERSIZED_LINE`].
fn is_oversized_reason(rejected: Option<&str>) -> bool {
    rejected == Some(OVERSIZED_LINE)
}

/// Retire [`OVERSIZED_LINE`], and only that reason: any other one stays until
/// the next connection opens (see [`forget_rejected_except_oversized`]).
fn forget_oversized(rejected_tx: &watch::Sender<Option<String>>) {
    rejected_tx.send_if_modified(|rejected| {
        if is_oversized_reason(rejected.as_deref()) {
            *rejected = None;
            true
        } else {
            false
        }
    });
}

/// Drop any inputs queued while no app was connected.
///
/// The `key`/`act` tools refuse to queue while the watch is `None`, but
/// inputs can still slip in through a race with a dying connection (or a
/// direct [`BridgeHandle::input_tx`] sender). Replaying them into a freshly
/// connected app instance would deliver keystrokes ("q", "y", ...) the agent
/// aimed at a UI that no longer exists, so they are discarded instead.
fn drain_stale_inputs(input_rx: &mut mpsc::Receiver<(InputId, AgentInput)>) {
    let mut drained = 0usize;
    while input_rx.try_recv().is_ok() {
        drained += 1;
    }
    if drained > 0 {
        tracing::warn!(
            drained,
            "dropped agent inputs queued before this connection; they targeted a previous app \
             instance"
        );
    }
}

/// Outcome of reading one line from the app socket.
enum LineRead {
    /// A complete line is in the buffer (newline stripped).
    Line,
    /// The app closed the connection (possibly mid-line).
    Eof,
    /// The line exceeded [`MAX_LINE_BYTES`]; the connection is broken.
    Overflow,
}

/// Read one `\n`-terminated line into `buf` (newline stripped), reading at
/// most [`MAX_LINE_BYTES`] plus the newline.
///
/// Cancel-safe: `read_until` appends partially read bytes to `buf`, and the
/// cap is recomputed from `buf.len()`, so being cancelled by `select!` and
/// re-called continues the same line without loosening the limit.
async fn read_line_capped(
    reader: &mut BufReader<impl AsyncRead + Unpin>,
    buf: &mut Vec<u8>,
) -> io::Result<LineRead> {
    loop {
        if buf.last() == Some(&b'\n') {
            buf.pop();
            return Ok(LineRead::Line);
        }
        if buf.len() > MAX_LINE_BYTES {
            return Ok(LineRead::Overflow);
        }
        let limit = (MAX_LINE_BYTES + 1 - buf.len()) as u64;
        let n = (&mut *reader).take(limit).read_until(b'\n', buf).await?;
        if n == 0 {
            return Ok(LineRead::Eof);
        }
    }
}

/// [`serve_connection`] for a manager that never lets go, which is how the
/// tests below drive one connection by hand.
#[cfg(all(test, unix))]
async fn run_connection(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    state_tx: &watch::Sender<BridgeState>,
    input_rx: &mut mpsc::Receiver<(InputId, AgentInput)>,
    ack_tx: &broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: &watch::Sender<Option<u32>>,
    rejected_tx: &watch::Sender<Option<String>>,
    conn_label: &mut Option<String>,
) -> ConnectionEnd {
    let (stamps_tx, _) = watch::channel(false);
    serve_connection(
        stream,
        state_tx,
        input_rx,
        ack_tx,
        protocol_tx,
        rejected_tx,
        &stamps_tx,
        conn_label,
        None,
    )
    .await
}

/// The inputs this connection wrote that the app has not acked yet, for an
/// on-demand manager deciding whether it may let go. Bounded by
/// [`MAX_UNACKED`].
#[derive(Default)]
struct Unacked {
    ids: BTreeSet<InputId>,
    /// When the ack that left none outstanding arrived, for telling a release
    /// the hold delayed from one at the end of the window.
    cleared_at: Option<TokioInstant>,
}

impl Unacked {
    fn sent(&mut self, id: InputId) {
        self.ids.insert(id);
        if self.ids.len() > MAX_UNACKED {
            self.ids.pop_first();
        }
    }

    /// Any ack settles its input: `Delivered` is sent as the app dequeues it,
    /// and an input the app has dequeued is no longer at risk from a release.
    fn acked(&mut self, id: InputId) {
        if self.ids.remove(&id) && self.ids.is_empty() {
            self.cleared_at = Some(TokioInstant::now());
        }
    }

    fn any(&self) -> bool {
        !self.ids.is_empty()
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    /// What held a release decided now, with `quiet_since` the end of the
    /// last tool call and `release_after` the window. An ack that cleared
    /// the last input after the window closed is what the release waited
    /// for; one that cleared it before had nothing to hold.
    fn hold(&self, quiet_since: TokioInstant, release_after: Duration) -> Hold {
        if self.any() {
            return Hold::Expired {
                unacked: self.len(),
            };
        }
        let window_end = quiet_since.checked_add(release_after);
        match (self.cleared_at, window_end) {
            (Some(cleared), Some(end)) if cleared > end => Hold::UntilAcked,
            _ => Hold::None,
        }
    }
}

/// Serve one connection: pump app lines into the watch and agent inputs onto
/// the socket until either side goes away. The label from the app's `Hello`
/// (if any) is stored in `conn_label` for the disconnect record.
///
/// With an [`Idle`], also the release: once the idle window has closed (and
/// no input this connection wrote still awaits its ack, up to
/// [`UNACKED_HOLD`]), the manager lets go, and the connection ends as
/// [`ConnectionEnd::Released`]. Returning drops the socket, which closes it
/// whether the app accepted it or it is still waiting in the backlog.
///
/// The connection is any stream the transport hands over, split here into
/// the halves the loop below reads and writes.
#[allow(clippy::too_many_arguments)]
async fn serve_connection(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    state_tx: &watch::Sender<BridgeState>,
    input_rx: &mut mpsc::Receiver<(InputId, AgentInput)>,
    ack_tx: &broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: &watch::Sender<Option<u32>>,
    rejected_tx: &watch::Sender<Option<String>>,
    stamps_tx: &watch::Sender<bool>,
    conn_label: &mut Option<String>,
    mut idle: Option<&mut Idle>,
) -> ConnectionEnd {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line_buf: Vec<u8> = Vec::new();
    let mut unacked = Unacked::default();
    // Whether this connection has served a tree yet: the first one retires
    // the release flag, whose departure the tree has superseded.
    let mut served = false;
    loop {
        let awaiting_acks = unacked.any();
        tokio::select! {
            read = read_line_capped(&mut reader, &mut line_buf) => match read {
                Ok(LineRead::Line) => {
                    let acked = handle_app_line(
                        &line_buf,
                        state_tx,
                        ack_tx,
                        protocol_tx,
                        stamps_tx,
                        rejected_tx,
                        conn_label,
                    );
                    if let Some(id) = acked {
                        unacked.acked(id);
                    }
                    line_buf.clear();
                    if !served && matches!(&*state_tx.borrow(), BridgeState::Connected(_)) {
                        served = true;
                        if let Some(idle) = idle.as_deref() {
                            idle.note(|activity| activity.released = false);
                        }
                    }
                }
                Ok(LineRead::Eof) => {
                    return ConnectionEnd::AppClosed {
                        unacked: unacked.len(),
                    };
                }
                Ok(LineRead::Overflow) => {
                    tracing::warn!(
                        cap_bytes = MAX_LINE_BYTES,
                        "app sent an oversized line; treating the connection as broken"
                    );
                    // Kept like a malformed line's reason, and sent before
                    // returning, so it is in place by the time the state is
                    // demoted. Without it the tools could only say the app
                    // went away, and the agent would wait for an app that is
                    // still running and about to send the same line again.
                    rejected_tx.send_replace(Some(OVERSIZED_LINE.to_string()));
                    return ConnectionEnd::AppClosed {
                        unacked: unacked.len(),
                    };
                }
                Err(err) => {
                    tracing::warn!(%err, "read error on app socket");
                    return ConnectionEnd::AppClosed {
                        unacked: unacked.len(),
                    };
                }
            },
            input = input_rx.recv() => {
                let Some((id, input)) = input else {
                    return ConnectionEnd::SessionClosed;
                };
                let msg = BridgeToApp::input(id, input);
                let mut line = match serde_json::to_string(&msg) {
                    Ok(line) => line,
                    Err(err) => {
                        tracing::error!(%err, "failed to serialize agent input; dropping it");
                        continue;
                    }
                };
                line.push('\n');
                if let Err(err) = write_half.write_all(line.as_bytes()).await {
                    tracing::warn!(%err, "write error on app socket");
                    return ConnectionEnd::AppClosed {
                        unacked: unacked.len(),
                    };
                }
                unacked.sent(id);
            }
            () = release_due(idle.as_deref_mut(), awaiting_acks) => {
                if let Some(idle) = idle.as_deref()
                    && let Some(quiet_since) = idle.try_release(awaiting_acks)
                {
                    return ConnectionEnd::Released(Release {
                        quiet_since,
                        hold: unacked.hold(quiet_since, idle.release_after),
                    });
                }
            }
        }
    }
}

/// [`Idle::release_due`] for a manager that has one; never, for one that does
/// not.
async fn release_due(idle: Option<&mut Idle>, unacked: bool) {
    match idle {
        Some(idle) => idle.release_due(unacked).await,
        None => std::future::pending().await,
    }
}

/// Handle one ndjson line from the app. Malformed lines are logged, recorded
/// in `rejected_tx` and skipped, so a buggy app cannot kill the bridge and an
/// agent asking for a tree is told what went wrong instead of being sent to
/// check the socket path.
///
/// Returns the id an ack answered, for the connection's own record of what is
/// still awaiting one.
fn handle_app_line(
    line: &[u8],
    state_tx: &watch::Sender<BridgeState>,
    ack_tx: &broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: &watch::Sender<Option<u32>>,
    stamps_tx: &watch::Sender<bool>,
    rejected_tx: &watch::Sender<Option<String>>,
    conn_label: &mut Option<String>,
) -> Option<InputId> {
    // Decoded from `&str` rather than `&[u8]` because the snapshot arm keeps
    // the line: JSON is defined over UTF-8, so a line that parses is valid
    // UTF-8 anyway, and taking the check first means the kept line is a
    // borrowed `&str` rather than a lossy conversion of bytes.
    let Ok(line) = str::from_utf8(line) else {
        tracing::warn!("ignoring a line from app that is not valid UTF-8");
        rejected_tx.send_replace(Some("the line is not valid UTF-8".to_string()));
        return None;
    };
    match serde_json::from_str::<AppToBridge>(line) {
        Ok(AppToBridge::Hello {
            app_label,
            protocol_version,
            stamps,
            ..
        }) => {
            if protocol_version == PROTOCOL_VERSION {
                tracing::info!(app_label, protocol_version, "app handshake received");
            } else {
                tracing::warn!(
                    app_label,
                    app_protocol = protocol_version,
                    bridge_protocol = PROTOCOL_VERSION,
                    "protocol version mismatch; the app cannot receive input from this bridge, \
                     and its snapshots parse only for as long as their shape has not moved"
                );
            }
            protocol_tx.send_replace(Some(protocol_version));
            stamps_tx.send_replace(stamps);
            *conn_label = Some(app_label);
        }
        Ok(AppToBridge::Snapshot(parsed)) => {
            tracing::debug!(seq = parsed.seq, "snapshot received");
            // A peer that has not greeted this connection is taken at the
            // version its snapshot states. Left unknown, it read as "no
            // mismatch known", and the input tools sent to a peer on another
            // version. Set before the tree is served, so no tool call can
            // see the tree and not the version; a `Hello` replaces it.
            let unannounced = protocol_tx.send_if_modified(|version| {
                if version.is_none() {
                    *version = Some(parsed.protocol_version);
                    true
                } else {
                    false
                }
            });
            if unannounced && parsed.protocol_version != PROTOCOL_VERSION {
                tracing::warn!(
                    app_protocol = parsed.protocol_version,
                    bridge_protocol = PROTOCOL_VERSION,
                    "the app sent a snapshot before any handshake, on another protocol version; \
                     the app cannot receive input from this bridge"
                );
            }
            // A snapshot that fits retires the oversized-line reason, and only
            // that one: a tree the bridge can read has replaced the one it
            // could not. Any other reason stays until the next connection
            // opens (see `forget_rejected_except_oversized`). Cleared before
            // the state is promoted, so a tool call that sees this tree can
            // never pair the next disconnect with the old size.
            forget_oversized(rejected_tx);
            // The line is kept beside the parsed form, not instead of it: the
            // agent reads the line and the bridge reasons about the parse.
            // See [`AppSnapshot`].
            state_tx.send_replace(BridgeState::Connected(AppSnapshot::from_line(line, parsed)));
        }
        Ok(AppToBridge::Ack { id, status, .. }) => {
            tracing::debug!(id = id.get(), ?status, "input ack received");
            // An ack nobody is waiting for is the normal case (the tool call
            // that sent the input has already returned), so a send with no
            // subscribers is not an error worth reporting.
            let _ = ack_tx.send((id, status));
            return Some(id);
        }
        // A message variant added to the protocol after this bridge was
        // written. Not a warning like a malformed line: the app is conforming
        // and merely newer, which the wire format calls an additive change, so
        // the bridge ignores what it has no handler for and keeps serving the
        // rest of the stream.
        Ok(_) => {
            tracing::debug!("ignoring an app message this bridge has no handler for");
        }
        // An unknown `type` is a message variant added in a later version 1
        // release. That is additive on the wire, so it is ignored exactly like
        // the `Ok(_)` arm above rather than reported as malformed: an
        // internally tagged enum with no catch-all fails to parse such a line,
        // so it arrives here as an error, but it is a conforming newer app, not
        // a broken one. Reporting it would put a false "malformed line" reason
        // in front of an agent (see `no_snapshot_reason`) for an app doing
        // nothing wrong, and contradict this module's own "quietly ignored"
        // guarantee for unknown messages.
        Err(_) if is_unknown_message_type(line) => {
            tracing::debug!("ignoring an app message type this bridge has no handler for");
        }
        Err(err) => {
            tracing::warn!(%err, "ignoring malformed line from app");
            // Kept, not just logged. Nobody reads a bridge's log while
            // retrofitting an app; they read the tool's error, and this is
            // the only place that knows why the line was thrown away.
            rejected_tx.send_replace(Some(err.to_string()));
        }
    }
    None
}

/// True when `line` is a JSON object whose `type` names a message this bridge
/// has no variant for: an additive future message to ignore, not a malformed
/// line to report. A known `type` whose fields did not parse is malformed and
/// returns `false`, as does anything that is not a JSON object with a string
/// `type`.
fn is_unknown_message_type(line: &str) -> bool {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    match map.get("type").and_then(serde_json::Value::as_str) {
        // Keep in step with the `AppToBridge` variants this build handles; the
        // `classifies_*` tests guard the pairing.
        Some(ty) => !matches!(ty, "hello" | "snapshot" | "ack"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use taria::{Node, Role};
    #[cfg(unix)]
    use tokio::net::UnixStream;

    use super::*;

    /// The reason is a fixed string, so nothing else keeps its number in step
    /// with the cap the reader enforces.
    #[test]
    fn the_oversized_reason_names_the_limit_the_reader_enforces() {
        assert_eq!(MAX_LINE_BYTES, 1024 * 1024, "the reason says 1 MiB");
        assert!(
            OVERSIZED_LINE.contains(&format!("over 1 MiB ({MAX_LINE_BYTES} bytes)")),
            "{OVERSIZED_LINE}"
        );
    }

    /// The overflow arm records why it ended the connection, and does so
    /// before `run_connection` returns: the manager demotes the state as soon
    /// as it does, and a tool call that sees the demotion has to find the
    /// reason already there.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_oversized_line_is_recorded_before_the_connection_ends() {
        let (bridge_end, mut app_end) = UnixStream::pair().expect("a socket pair");
        let (state_tx, _state_rx) = watch::channel(BridgeState::Never);
        // Held open, so the end of the session cannot be what ends the call.
        let (_input_tx, mut input_rx) = mpsc::channel(1);
        let (ack_tx, _) = broadcast::channel(1);
        let (protocol_tx, _protocol_rx) = watch::channel(None);
        let (rejected_tx, rejected_rx) = watch::channel(None);
        let writer = tokio::spawn(async move {
            // One byte past the cap and no newline. The write fails once the
            // bridge closes its end, which is the outcome under test.
            let _ = app_end.write_all(&vec![b'x'; MAX_LINE_BYTES + 1]).await;
            app_end
        });
        let mut label = None;
        let end = run_connection(
            bridge_end,
            &state_tx,
            &mut input_rx,
            &ack_tx,
            &protocol_tx,
            &rejected_tx,
            &mut label,
        )
        .await;
        assert!(matches!(end, ConnectionEnd::AppClosed { .. }));
        assert_eq!(rejected_rx.borrow().as_deref(), Some(OVERSIZED_LINE));
        drop(writer.await.expect("the writer task"));
    }

    /// A snapshot that parses retires the oversized reason, and only that
    /// one. A handshake does not: every reconnect greets the bridge before it
    /// sends the same tree again.
    #[test]
    fn a_snapshot_that_fits_clears_the_oversized_reason_and_nothing_else() {
        let (state_tx, _state_rx) = watch::channel(BridgeState::Never);
        let (ack_tx, _) = broadcast::channel(1);
        let (protocol_tx, _protocol_rx) = watch::channel(None);
        let (stamps_tx, _stamps_rx) = watch::channel(false);
        let (rejected_tx, rejected_rx) = watch::channel(Some(OVERSIZED_LINE.to_string()));
        let mut label = None;
        let hello = serde_json::to_string(&AppToBridge::hello("demo", PROTOCOL_VERSION))
            .expect("a hello serializes");
        let snapshot = serde_json::to_string(&AppToBridge::Snapshot(Snapshot::new(
            1,
            Node::new("app", Role::App),
        )))
        .expect("a snapshot serializes");
        let mut line = |line: &str| {
            handle_app_line(
                line.as_bytes(),
                &state_tx,
                &ack_tx,
                &protocol_tx,
                &stamps_tx,
                &rejected_tx,
                &mut label,
            );
        };

        line(&hello);
        assert_eq!(rejected_rx.borrow().as_deref(), Some(OVERSIZED_LINE));
        line(&snapshot);
        assert_eq!(rejected_rx.borrow().as_deref(), None);

        // Any other reason is kept: only the next connection retires it.
        rejected_tx.send_replace(Some("expected value at line 1".to_string()));
        line(&snapshot);
        assert_eq!(
            rejected_rx.borrow().as_deref(),
            Some("expected value at line 1")
        );
    }

    /// A `connect` that finds nothing listening retires the oversized reason,
    /// because the app that resent that tree on every reconnect has gone.
    /// The errors are the ones this platform really returns for a socket file
    /// nobody is bound to and for a path with no file, so a kind the matcher
    /// misses fails here rather than leaving the reason in place for good.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_connect_that_finds_nothing_listening_forgets_the_oversized_reason() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let stale = dir.path().join("stale.sock");
        // Bound, then dropped: the file stays and nothing listens on it, which
        // is what an app that crashed leaves behind.
        drop(std::os::unix::net::UnixListener::bind(&stale).expect("bind"));
        let gone = dir.path().join("gone.sock");

        for (name, path) in [("stale socket", &stale), ("no socket", &gone)] {
            let err = UnixStream::connect(path)
                .await
                .expect_err("nothing listens here");
            let (rejected_tx, rejected_rx) = watch::channel(Some(OVERSIZED_LINE.to_string()));
            forget_oversized_if_nothing_listens(&err, &rejected_tx);
            assert_eq!(
                rejected_rx.borrow().as_deref(),
                None,
                "{name}: {err} ({:?})",
                err.kind()
            );

            // Any other reason is about a line, not about the app being there,
            // and stays.
            rejected_tx.send_replace(Some("expected value at line 1".to_string()));
            forget_oversized_if_nothing_listens(&err, &rejected_tx);
            assert_eq!(
                rejected_rx.borrow().as_deref(),
                Some("expected value at line 1"),
                "{name}"
            );
        }

        // A connect that failed for any other reason says nothing about the
        // app, so the reason is left as it was.
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Other,
        ] {
            let (rejected_tx, rejected_rx) = watch::channel(Some(OVERSIZED_LINE.to_string()));
            forget_oversized_if_nothing_listens(&io::Error::from(kind), &rejected_tx);
            assert_eq!(
                rejected_rx.borrow().as_deref(),
                Some(OVERSIZED_LINE),
                "{kind:?}"
            );
        }
    }

    /// A connection that opens retires the last connection's reason for a
    /// rejected line, so the tool layer can quote a reason seen with a
    /// connection open as that connection's (issue #16), and spares the
    /// oversized one, which the app resends to every connection. Nothing is
    /// sent for a reason that is not there.
    #[test]
    fn a_new_connection_forgets_every_reason_but_the_oversized_one() {
        let (rejected_tx, mut rejected_rx) =
            watch::channel(Some("expected value at line 1".to_string()));
        rejected_rx.borrow_and_update();
        forget_rejected_except_oversized(&rejected_tx);
        assert_eq!(rejected_rx.borrow_and_update().as_deref(), None);

        forget_rejected_except_oversized(&rejected_tx);
        assert!(!rejected_rx.has_changed().expect("the sender is alive"));

        rejected_tx.send_replace(Some(OVERSIZED_LINE.to_string()));
        rejected_rx.borrow_and_update();
        forget_rejected_except_oversized(&rejected_tx);
        assert_eq!(rejected_rx.borrow().as_deref(), Some(OVERSIZED_LINE));
        assert!(!rejected_rx.has_changed().expect("the sender is alive"));
    }

    /// A peer that publishes before it greets is taken at the version its
    /// snapshot states, so a peer on another version is refused input even
    /// with no `hello`; a `hello` later replaces it, and a version the
    /// handshake set is not overridden by a snapshot's.
    #[test]
    fn a_snapshot_sets_the_version_only_where_no_hello_has() {
        let (state_tx, _state_rx) = watch::channel(BridgeState::Never);
        let (ack_tx, _) = broadcast::channel(1);
        let (protocol_tx, protocol_rx) = watch::channel(None);
        let (stamps_tx, _stamps_rx) = watch::channel(false);
        let (rejected_tx, _rejected_rx) = watch::channel(None);
        let mut label = None;
        let mut line = |message: &AppToBridge| {
            let line = serde_json::to_string(message).expect("a message serializes");
            handle_app_line(
                line.as_bytes(),
                &state_tx,
                &ack_tx,
                &protocol_tx,
                &stamps_tx,
                &rejected_tx,
                &mut label,
            );
        };
        let snapshot = |version: u32| {
            let mut snapshot = Snapshot::new(1, Node::new("app", Role::App));
            snapshot.protocol_version = version;
            AppToBridge::Snapshot(snapshot)
        };

        line(&snapshot(2));
        assert_eq!(*protocol_rx.borrow(), Some(2));
        // The first snapshot's version holds: later ones do not move it.
        line(&snapshot(1));
        assert_eq!(*protocol_rx.borrow(), Some(2));
        line(&AppToBridge::hello("demo", 1));
        assert_eq!(*protocol_rx.borrow(), Some(1), "a hello replaces it");
        line(&snapshot(2));
        assert_eq!(*protocol_rx.borrow(), Some(1), "and a snapshot never does");
    }

    /// The flag that says a departure was this bridge's own release follows
    /// the `Disconnected` record it describes. Only a connection that served
    /// a tree writes that record, so only such a connection sets the flag;
    /// a connection the app closed clears it whether it served or not; and a
    /// release of one that served nothing leaves it, since the record is
    /// unchanged. The peer's pid goes with every connection.
    #[test]
    fn the_release_flag_follows_the_departure_it_describes() {
        let (idle, demand) = idle_and_demand(Duration::from_secs(1));
        let release = || {
            ConnectionEnd::Released(Release {
                quiet_since: TokioInstant::now(),
                hold: Hold::None,
            })
        };
        let closed = ConnectionEnd::AppClosed { unacked: 0 };
        assert!(!demand.released(), "nothing has been let go of yet");

        idle.note_disconnect(true, &release());
        assert!(demand.released());
        idle.note_disconnect(false, &release());
        assert!(
            demand.released(),
            "a held connection let go of names nothing new"
        );
        idle.note_disconnect(false, &closed);
        assert!(
            !demand.released(),
            "the app closed a connection after the release"
        );

        idle.note_disconnect(false, &release());
        assert!(!demand.released(), "the record names an older departure");
        idle.note_disconnect(true, &release());
        idle.note_disconnect(true, &closed);
        assert!(!demand.released(), "a served connection the app closed");

        idle.note(|activity| activity.peer_pid = Some(42));
        assert_eq!(demand.peer_pid(), Some(42));
        idle.note_disconnect(true, &closed);
        assert_eq!(
            demand.peer_pid(),
            None,
            "the pid was the ended connection's"
        );
    }

    /// A release is this bridge's own doing only while the app it let go of
    /// is the one still listening. An app that restarted while this bridge
    /// made no call, and was taken by another bridge before the next one,
    /// is met by the next connect under another pid, and the claim goes.
    /// The same pid keeps it, and so does a pid the kernel did not name, on
    /// either side: nothing to tell the two apart by.
    #[test]
    fn a_connect_to_another_process_retires_the_release_flag() {
        let (idle, demand) = idle_and_demand(Duration::from_secs(1));
        let release = || {
            ConnectionEnd::Released(Release {
                quiet_since: TokioInstant::now(),
                hold: Hold::None,
            })
        };
        // A served connection to pid 42, let go of after the window.
        idle.note_connect(Some(42));
        idle.note_disconnect(true, &release());
        assert!(demand.released());

        idle.note_connect(Some(42));
        assert!(demand.released(), "the app it let go of, held by another");
        idle.note_disconnect(false, &release());
        idle.note_connect(None);
        assert!(demand.released(), "a peer the kernel did not name");
        idle.note_disconnect(false, &release());
        assert!(demand.released(), "a held connection let go of");
        idle.note_connect(Some(43));
        assert!(!demand.released(), "another process listens at the path");
        assert_eq!(demand.peer_pid(), Some(43));

        // A release of a peer the kernel did not name has nothing to compare.
        idle.note_disconnect(false, &release());
        idle.note_connect(None);
        idle.note_disconnect(true, &release());
        assert!(demand.released());
        idle.note_connect(Some(44));
        assert!(demand.released(), "the pid let go of is unknown");
    }

    /// A connection that serves a tree retires the release flag at once, so
    /// it cannot outlive the departure it was about, whatever ends the
    /// connection later.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_connection_that_serves_a_tree_retires_the_release_flag() {
        let (mut idle, demand) = idle_and_demand(Duration::from_secs(1));
        let mut wiring = Wiring::new();
        idle.note(|activity| activity.released = true);
        let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");
        drop(demand.begin());
        let app = async {
            let (read, mut write) = app_end.into_split();
            let mut snapshot = serde_json::to_string(&AppToBridge::Snapshot(Snapshot::new(
                1,
                Node::new("app", Role::App),
            )))
            .expect("a snapshot serializes");
            snapshot.push('\n');
            write
                .write_all(snapshot.as_bytes())
                .await
                .expect("send the snapshot");
            // Polled: the flag changes without waking anyone. Far inside
            // the window, which is the next thing the clock could jump to.
            in_small_steps(async {
                while demand.released() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await;
            let mut rest = Vec::new();
            BufReader::new(read)
                .read_to_end(&mut rest)
                .await
                .expect("read until the bridge closes");
        };
        let ((end, _at), ()) = tokio::join!(wiring.serve(bridge_end, &mut idle), app);
        released(end);
    }

    #[test]
    fn classifies_unknown_type_as_ignorable() {
        // A well-formed message a later version 1 release added.
        assert!(is_unknown_message_type(r#"{"type":"capability"}"#));
        assert!(is_unknown_message_type(r#"{"type":"paste","data":"x"}"#));
    }

    #[test]
    fn classifies_known_types_as_not_ignorable() {
        // A known `type` reaching the error path failed on its fields, which is
        // malformed, not additive, so it must still be reported.
        for ty in ["hello", "snapshot", "ack"] {
            assert!(
                !is_unknown_message_type(&format!(r#"{{"type":"{ty}"}}"#)),
                "{ty} is a known message type"
            );
        }
    }

    #[test]
    fn classifies_non_messages_as_not_ignorable() {
        assert!(!is_unknown_message_type("not json at all"));
        assert!(!is_unknown_message_type(r#"["an","array"]"#));
        assert!(!is_unknown_message_type(r#"{"no":"type field"}"#));
        assert!(!is_unknown_message_type(r#"{"type":42}"#));
    }

    // The idle window and the unacked hold. The waits are tokio timers, so
    // these run on a paused clock: thirty seconds of hold cost nothing, and an
    // instant judged by another clock than the timer's would show up here as
    // a test that spins on the wall clock instead of finishing at once.

    /// The channels one connection is served with, for driving
    /// [`serve_connection`] by hand.
    #[cfg(unix)]
    struct Wiring {
        state_tx: watch::Sender<BridgeState>,
        input_tx: mpsc::Sender<(InputId, AgentInput)>,
        input_rx: mpsc::Receiver<(InputId, AgentInput)>,
        ack_tx: broadcast::Sender<(InputId, InputStatus)>,
        protocol_tx: watch::Sender<Option<u32>>,
        rejected_tx: watch::Sender<Option<String>>,
    }

    #[cfg(unix)]
    impl Wiring {
        fn new() -> Self {
            let (state_tx, _) = watch::channel(BridgeState::Never);
            let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
            let (ack_tx, _) = broadcast::channel(ACK_QUEUE);
            let (protocol_tx, _) = watch::channel(None);
            let (rejected_tx, _) = watch::channel(None);
            Self {
                state_tx,
                input_tx,
                input_rx,
                ack_tx,
                protocol_tx,
                rejected_tx,
            }
        }

        /// Serve `stream` the on-demand way until it ends: how, and when on
        /// tokio's clock.
        async fn serve(
            &mut self,
            stream: UnixStream,
            idle: &mut Idle,
        ) -> (ConnectionEnd, TokioInstant) {
            let mut label = None;
            let (stamps_tx, _) = watch::channel(false);
            let end = serve_connection(
                stream,
                &self.state_tx,
                &mut self.input_rx,
                &self.ack_tx,
                &self.protocol_tx,
                &self.rejected_tx,
                &stamps_tx,
                &mut label,
                Some(idle),
            )
            .await;
            (end, TokioInstant::now())
        }
    }

    /// The app's end of a connection, held open until the bridge closes it.
    #[cfg(unix)]
    async fn hold_open(app_end: UnixStream) {
        let mut rest = Vec::new();
        let mut app_end = app_end;
        app_end
            .read_to_end(&mut rest)
            .await
            .expect("read until the bridge closes");
    }

    /// Run `io` with the paused clock held to millisecond steps.
    ///
    /// A paused clock jumps to the next timer whenever the runtime has nothing
    /// to run, and a socket that the other end has already written to is not
    /// something to run until the I/O driver has turned once, which happens in
    /// the same park the jump is decided in. So a read that is ready at once
    /// can find the clock already moved to the next deadline (here, typically
    /// the end of the hold), as if the peer had taken that long. A 1 ms timer,
    /// rearmed for as long as `io` runs, caps every such jump at 1 ms.
    ///
    /// What `io` waits on is something the other end has already done, so it
    /// is ready within a step or two. One still pending after
    /// [`SMALL_STEPS_BOUND`] of the paused clock never will be (a bridge that
    /// stopped reading, say), and fails the test rather than stepping the
    /// clock forever.
    #[cfg(unix)]
    async fn in_small_steps<T>(io: impl Future<Output = T>) -> T {
        let bound = TokioInstant::now() + SMALL_STEPS_BOUND;
        tokio::pin!(io);
        loop {
            tokio::select! {
                out = &mut io => return out,
                () = tokio::time::sleep(Duration::from_millis(1)) => assert!(
                    TokioInstant::now() < bound,
                    "still pending after {SMALL_STEPS_BOUND:?} of 1 ms steps"
                ),
            }
        }
    }

    /// How long [`in_small_steps`] steps the paused clock before giving up:
    /// far longer than anything it waits on takes, and far shorter than
    /// [`UNACKED_HOLD`], so a missed ack fails before the hold ends.
    #[cfg(unix)]
    const SMALL_STEPS_BOUND: Duration = Duration::from_secs(5);

    /// The id of the next input the bridge writes on `reader`.
    #[cfg(unix)]
    async fn next_input_id<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> InputId {
        let mut line = String::new();
        in_small_steps(reader.read_line(&mut line))
            .await
            .expect("an input line");
        match serde_json::from_str(&line).expect("a bridge message") {
            BridgeToApp::Input { id, .. } => id,
            other => panic!("expected an input, got {other:?}"),
        }
    }

    /// The release of `end`, failing the test for any other ending.
    #[cfg(unix)]
    fn released(end: ConnectionEnd) -> Release {
        match end {
            ConnectionEnd::Released(release) => release,
            ConnectionEnd::AppClosed { .. } => panic!("the connection ended as AppClosed"),
            ConnectionEnd::SessionClosed => panic!("the connection ended as SessionClosed"),
        }
    }

    /// `at` is `want` after `from`, give or take the milliseconds tokio's
    /// timers round their deadlines up to and [`in_small_steps`] leaves.
    #[cfg(unix)]
    fn assert_at(from: TokioInstant, at: TokioInstant, want: Duration) {
        let took = at - from;
        assert!(
            took >= want && took <= want + Duration::from_millis(5),
            "released {took:?} after the window started, not {want:?}"
        );
    }

    /// The release comes when the window closes, not a tick before, and the
    /// window runs from the end of the last call: one that ends just before
    /// the window closes starts it again, and one still running when it
    /// would close holds it open until it ends.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn the_window_runs_from_the_end_of_the_last_call() {
        let window = Duration::from_secs(1);
        let (mut idle, demand) = idle_and_demand(window);
        let mut wiring = Wiring::new();
        let wall = Instant::now();

        // One call, and nothing after it.
        let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");
        let t0 = TokioInstant::now();
        drop(demand.begin());
        let ((end, at), ()) = tokio::join!(wiring.serve(bridge_end, &mut idle), hold_open(app_end));
        let release = released(end);
        assert_eq!(release.quiet_since, t0);
        assert_eq!(release.hold, Hold::None);
        assert_at(t0, at, window);

        // A call that ends a millisecond before the window closes.
        let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");
        let t1 = TokioInstant::now();
        drop(demand.begin());
        let late_call = async {
            tokio::time::sleep_until(t1 + window - Duration::from_millis(1)).await;
            drop(demand.begin());
        };
        let ((end, at), (), ()) = tokio::join!(
            wiring.serve(bridge_end, &mut idle),
            late_call,
            hold_open(app_end)
        );
        assert_eq!(released(end).hold, Hold::None);
        assert_at(t1, at, window * 2 - Duration::from_millis(1));

        // A call that is still running when the window would close, and ends
        // three seconds in.
        let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");
        let t2 = TokioInstant::now();
        drop(demand.begin());
        let long_call = async {
            tokio::time::sleep_until(t2 + window / 2).await;
            let call = demand.begin();
            tokio::time::sleep_until(t2 + Duration::from_secs(3)).await;
            drop(call);
        };
        let ((end, at), (), ()) = tokio::join!(
            wiring.serve(bridge_end, &mut idle),
            long_call,
            hold_open(app_end)
        );
        assert_eq!(released(end).quiet_since, t2 + Duration::from_secs(3));
        assert_at(t2, at, Duration::from_secs(3) + window);

        // Five seconds of windows on the paused clock, in no time on the
        // wall clock: nothing waited on a deadline by another clock.
        assert!(
            wall.elapsed() < Duration::from_secs(3),
            "{:?}",
            wall.elapsed()
        );
    }

    /// An app that never acks an input holds the app past the window, and
    /// for [`UNACKED_HOLD`] past it, not for good: an adapter older than the
    /// ack contract never acks anything, and a bridge that held such an app
    /// forever is the lock-out the release exists to end.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_input_never_acked_holds_the_app_for_the_cap_and_no_longer() {
        let window = Duration::from_secs(1);
        let (mut idle, demand) = idle_and_demand(window);
        let mut wiring = Wiring::new();
        let wall = Instant::now();
        let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");

        // Sent while a call is in flight, as every input is.
        let call = demand.begin();
        wiring
            .input_tx
            .send((InputId::new(7), AgentInput::key("x")))
            .await
            .expect("queued");
        let t0 = TokioInstant::now();
        drop(call);
        let app = async move {
            let mut reader = BufReader::new(app_end);
            assert_eq!(next_input_id(&mut reader).await, InputId::new(7));
            let mut rest = Vec::new();
            reader
                .read_to_end(&mut rest)
                .await
                .expect("read until the bridge closes");
        };
        let ((end, at), ()) = tokio::join!(wiring.serve(bridge_end, &mut idle), app);
        let release = released(end);
        assert_eq!(release.quiet_since, t0);
        assert_eq!(release.hold, Hold::Expired { unacked: 1 });
        assert_at(t0, at, window + UNACKED_HOLD);
        // Thirty-one seconds on the paused clock. A release judged by the
        // wall clock against a tokio deadline spins here until the wall clock
        // catches up.
        assert!(
            wall.elapsed() < Duration::from_secs(3),
            "{:?}",
            wall.elapsed()
        );
    }

    /// An ack that arrives during the hold ends it at once, and the release
    /// says the hold delayed it; an ack before the window closed held
    /// nothing.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_ack_during_the_hold_releases_at_once() {
        let window = Duration::from_secs(1);
        let (mut idle, demand) = idle_and_demand(window);
        let mut wiring = Wiring::new();
        let wall = Instant::now();

        for (acked_after, hold) in [
            (Duration::from_secs(5), Hold::UntilAcked),
            (window / 2, Hold::None),
        ] {
            let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");
            let call = demand.begin();
            wiring
                .input_tx
                .send((InputId::new(9), AgentInput::key("x")))
                .await
                .expect("queued");
            let t0 = TokioInstant::now();
            drop(call);
            let mut acks = wiring.ack_tx.subscribe();
            let app = async move {
                let (read, mut write) = app_end.into_split();
                let mut reader = BufReader::new(read);
                assert_eq!(next_input_id(&mut reader).await, InputId::new(9));
                tokio::time::sleep_until(t0 + acked_after).await;
                let mut ack = serde_json::to_string(&AppToBridge::ack(
                    InputId::new(9),
                    InputStatus::Delivered,
                ))
                .expect("an ack serializes");
                ack.push('\n');
                write.write_all(ack.as_bytes()).await.expect("send the ack");
                // Until the bridge has read it: the next timer is the end of
                // the hold, and the clock must not jump there first.
                in_small_steps(acks.recv())
                    .await
                    .expect("the bridge read the ack");
                let mut rest = Vec::new();
                reader
                    .read_to_end(&mut rest)
                    .await
                    .expect("read until the bridge closes");
            };
            let ((end, at), ()) = tokio::join!(wiring.serve(bridge_end, &mut idle), app);
            assert_eq!(released(end).hold, hold, "acked after {acked_after:?}");
            assert_at(t0, at, acked_after.max(window));
        }
        // Six seconds of window and hold on the paused clock, in no time on
        // the wall clock.
        assert!(
            wall.elapsed() < Duration::from_secs(3),
            "{:?}",
            wall.elapsed()
        );
    }

    /// An app that closes the connection while it is held for an ack ends
    /// the connection with the inputs still unacked counted, which is how
    /// the release that follows is logged as the hold cut short rather than
    /// as a plain end of the window.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn the_app_leaving_during_the_hold_is_reported_with_the_inputs_it_held() {
        let window = Duration::from_secs(1);
        let (mut idle, demand) = idle_and_demand(window);
        let mut wiring = Wiring::new();
        let wall = Instant::now();
        let (bridge_end, app_end) = UnixStream::pair().expect("a socket pair");

        let call = demand.begin();
        wiring
            .input_tx
            .send((InputId::new(11), AgentInput::key("x")))
            .await
            .expect("queued");
        let t0 = TokioInstant::now();
        drop(call);
        let leaves_after = window + Duration::from_secs(5);
        let (served_tx, served_rx) = tokio::sync::oneshot::channel();
        let serve = async {
            let out = wiring.serve(bridge_end, &mut idle).await;
            let _ = served_tx.send(());
            out
        };
        let app = async move {
            let mut reader = BufReader::new(app_end);
            assert_eq!(next_input_id(&mut reader).await, InputId::new(11));
            tokio::time::sleep_until(t0 + leaves_after).await;
            // Gone with the input unacked, five seconds into the hold. Shut
            // down before the drop: a child process another test spawns holds
            // a copy of every open descriptor between its fork and its exec,
            // and while it does, closing this one sends no EOF, and the paused
            // clock jumps to the end of the hold before the bridge sees one.
            reader
                .get_mut()
                .shutdown()
                .await
                .expect("shut the app's end down");
            drop(reader);
            // Until the bridge has seen the end: the next timer is the end
            // of the hold, and the clock must not jump there first.
            in_small_steps(served_rx)
                .await
                .expect("the bridge ended the connection");
        };
        let ((end, at), ()) = tokio::join!(serve, app);
        assert!(
            matches!(end, ConnectionEnd::AppClosed { unacked: 1 }),
            "the connection did not end as the app leaving with one input unacked"
        );
        assert_at(t0, at, leaves_after);
        // The window had closed, so the manager lets go rather than reconnect.
        assert_eq!(idle.try_release(false), Some(t0));
        assert!(
            wall.elapsed() < Duration::from_secs(3),
            "{:?}",
            wall.elapsed()
        );
    }

    /// An app that closes the connection once the window has closed is let
    /// go of rather than reconnected to, and the log says what that ended:
    /// the hold, with the inputs it was for, or nothing at all when none was
    /// awaiting its ack, which is a plain release that the app's leaving
    /// timed.
    #[test]
    fn an_app_leaving_past_the_window_is_logged_by_what_it_held() {
        let window = Duration::from_secs(60);
        let idle = Duration::from_millis(75_500);
        for (unacked, hold, line) in [
            (
                0,
                Hold::None,
                "no tool call for 75.5 s: released the app (the app closed the connection; not \
                 reconnecting); the next tool call reconnects",
            ),
            (
                2,
                Hold::AppClosed { unacked: 2 },
                "no tool call for 75.5 s, 15.5 s past the 60 s idle window, held there while 2 \
                 input(s) awaited the app's ack: released the app (the app closed the \
                 connection during the ack hold; not reconnecting); the next tool call \
                 reconnects",
            ),
        ] {
            let (chosen, what) = app_closed_release(unacked);
            assert_eq!(chosen, hold, "{unacked} unacked");
            assert_eq!(release_line(idle, window, chosen, what), line);
        }
    }

    /// The set of unacked inputs is bounded, forgetting the oldest, so an app
    /// that never acks cannot grow it without limit; and what matters, whether
    /// any input is outstanding, is still answered by the ones it kept.
    #[test]
    fn the_unacked_inputs_are_bounded_and_the_oldest_forgotten() {
        let mut unacked = Unacked::default();
        let past = MAX_UNACKED as u64 + 5;
        for id in 1..=past {
            unacked.sent(InputId::new(id));
        }
        assert_eq!(unacked.len(), MAX_UNACKED);
        assert_eq!(
            unacked.ids.first(),
            Some(&InputId::new(6)),
            "the five oldest are forgotten"
        );

        // Acking a forgotten input changes nothing, and is no error.
        unacked.acked(InputId::new(1));
        assert_eq!(unacked.len(), MAX_UNACKED);
        assert!(unacked.cleared_at.is_none());

        let now = TokioInstant::now();
        let window = Duration::from_secs(60);
        assert_eq!(
            unacked.hold(now, window),
            Hold::Expired {
                unacked: MAX_UNACKED
            }
        );
        for id in 6..=past {
            unacked.acked(InputId::new(id));
        }
        assert!(!unacked.any());
        assert!(unacked.cleared_at.is_some());
        // Cleared now, inside a window that started now: nothing held it.
        assert_eq!(unacked.hold(now, window), Hold::None);
        // Cleared after a window that had already closed: the hold did.
        let long_ago = now
            .checked_sub(window * 2)
            .expect("the test clock is not that young");
        assert_eq!(unacked.hold(long_ago, window), Hold::UntilAcked);
    }

    /// The lock protocol, with no timing in it. A call counts itself in, and
    /// the manager decides to let go, each under the activity lock, so exactly
    /// one of them wins: either the manager sees the call and stays, or the
    /// call sees the manager gone and waits for a fresh connection. A call
    /// may never send to a connection the manager has decided to close.
    #[test]
    fn a_call_and_a_release_cannot_both_win() {
        let window = Duration::from_secs(1);
        let closed_window = |idle: &Idle| {
            idle.activity.send_modify(|activity| {
                activity.awake = true;
                activity.last_end = Some(
                    TokioInstant::now()
                        .checked_sub(window * 2)
                        .expect("the test clock is not that young"),
                );
            });
        };

        // The release first: the call that follows finds the manager gone,
        // and is a reason for it to wake.
        let (idle, demand) = idle_and_demand(window);
        closed_window(&idle);
        assert!(idle.try_release(false).is_some(), "the window had closed");
        let call = demand.begin();
        {
            let activity = demand.activity.borrow();
            assert!(
                !activity.awake,
                "the call must not see the released connection"
            );
            assert!(
                window_open(&activity, window),
                "the manager must wake for the call"
            );
        }
        assert!(idle.try_release(false).is_none(), "nothing left to let go");
        drop(call);

        // The call first: the release that follows finds it and stays, with
        // an input outstanding or without, and the call's end restarts the
        // window rather than releasing.
        let (idle, demand) = idle_and_demand(window);
        closed_window(&idle);
        let call = demand.begin();
        assert!(idle.try_release(false).is_none());
        assert!(idle.try_release(true).is_none());
        assert!(
            demand.activity.borrow().awake,
            "the call's connection stays"
        );
        drop(call);
        assert!(idle.try_release(false).is_none());
        assert!(idle.try_release(true).is_none());
        assert!(demand.activity.borrow().awake);

        // A zero window never lets go at all.
        let (idle, demand) = idle_and_demand(Duration::ZERO);
        closed_window(&idle);
        drop(demand.begin());
        assert!(idle.try_release(false).is_none());
    }

    /// A [`BridgeHandle`] wired by hand, with every sender kept, so no
    /// channel closes under a call that waits on it.
    struct HandWired {
        handle: BridgeHandle,
        link_tx: watch::Sender<LinkState>,
        _state_tx: watch::Sender<BridgeState>,
        _protocol_tx: watch::Sender<Option<u32>>,
        _rejected_tx: watch::Sender<Option<String>>,
        _input_rx: mpsc::Receiver<(InputId, AgentInput)>,
    }

    impl HandWired {
        fn new() -> Self {
            let (state_tx, state_rx) = watch::channel(BridgeState::Never);
            let (input_tx, input_rx) = mpsc::channel(1);
            let (ack_tx, _) = broadcast::channel(1);
            let (protocol_tx, protocol_rx) = watch::channel(None);
            let (rejected_tx, rejected_rx) = watch::channel(None);
            let (link_tx, link_rx) = watch::channel(LinkState::Down);
            Self {
                handle: BridgeHandle {
                    state_rx,
                    input_tx,
                    ack_tx,
                    protocol_rx,
                    rejected_rx,
                    link_rx,
                    stamps_rx: watch::channel(false).1,
                    send_order: Default::default(),
                },
                link_tx,
                _state_tx: state_tx,
                _protocol_tx: protocol_tx,
                _rejected_tx: rejected_tx,
                _input_rx: input_rx,
            }
        }
    }

    /// The grace a silent connection gets is counted from when it opened, so
    /// a connect that ran late still gets all of it: the call answers once the
    /// connection is old enough for its silence to be diagnosed, and the tool
    /// layer, reading the age after it, says the app is held rather than
    /// calling the connection young. One opened too late for that is answered
    /// at the bound, still young, which the tool layer says too.
    ///
    /// Real time: the link's age is read on the wall clock it was stamped by,
    /// so each case leaves a loaded host room on the side it can fail on. The
    /// first opens 250 ms in, where a grace counted from the call's start
    /// would answer at 500 ms about a link 250 ms old; the 750 ms it should
    /// take leaves 450 ms before the 1.2 s bound. The second opens 1.1 s in
    /// and is answered at the bound about a link 100 ms old, which leaves
    /// 400 ms for the answer to run late before the link is old enough to
    /// pass for one the call waited the grace out on (that answer would come
    /// at 1.6 s, about a link 500 ms old).
    #[tokio::test]
    async fn the_first_call_counts_the_grace_from_when_its_connection_opened() {
        for (opens_after, diagnosed) in [
            (Duration::from_millis(250), true),
            (Duration::from_millis(1_100), false),
        ] {
            let (idle, demand) = idle_and_demand(Duration::from_secs(30));
            let wired = HandWired::new();
            // The manager is awake and connecting.
            idle.activity.send_modify(|activity| activity.awake = true);
            let call = demand.begin();
            let started = Instant::now();
            // Each side reports its own moment as it happens. Measured after
            // both had finished, an opener that fired past the bound left a
            // link young enough for the second case to pass on, with the
            // call never having seen it open.
            let settle = async {
                call.settle(&wired.handle).await;
                (started.elapsed(), wired.handle.link_rx.borrow().open_for())
            };
            let opener = async {
                tokio::time::sleep(opens_after).await;
                wired.link_tx.send_replace(LinkState::Open {
                    since: Instant::now(),
                });
                started.elapsed()
            };
            let ((waited, age), opened) = tokio::join!(settle, opener);
            assert!(
                opened < FIRST_CALL_WAIT,
                "the link opened {opened:?} in, past the call's bound, so the case tests nothing"
            );
            let age = age.unwrap_or_else(|| {
                panic!("opened {opened:?} in, the call answered at {waited:?} with no link open")
            });
            if diagnosed {
                assert!(
                    age >= SILENT_LINK_GRACE && waited < FIRST_CALL_WAIT,
                    "opened {opened:?} in, the call answered at {waited:?} about a link {age:?} \
                     old"
                );
            } else {
                assert!(
                    waited >= FIRST_CALL_WAIT && waited < FIRST_CALL_WAIT * 2,
                    "opened {opened:?} in, the call answered at {waited:?}"
                );
                assert!(
                    age < SILENT_LINK_GRACE,
                    "opened {opened:?} in, the call answered at {waited:?} about a link {age:?} \
                     old"
                );
            }
        }
    }
}
