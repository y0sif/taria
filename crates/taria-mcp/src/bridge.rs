//! Socket manager: owns the connection to the app's taria Unix socket.
//!
//! A single background task connects (retrying forever with capped backoff),
//! reads `AppToBridge` ndjson lines into a [`watch`] channel holding the
//! latest [`BridgeState`], and writes queued [`AgentInput`]s out as
//! `BridgeToApp::Input` lines. A snapshot is stored as an [`AppSnapshot`],
//! which keeps the app's line (minus the message framing) alongside the parse
//! of it, so the tool layer relays what the app wrote instead of
//! re-serializing what this build could decode. On disconnect the watch flips
//! to
//! [`BridgeState::Disconnected`] so tool calls fail fast (instead of acting
//! on a stale tree) with an error that says which app went away.
//!
//! Every outgoing input carries an [`InputId`] and the app answers it with an
//! `AppToBridge::Ack`. Acks are republished on a [`broadcast`] channel rather
//! than kept here, because only the tool call that sent an input knows which
//! id it is waiting for; the manager stays free of per-input bookkeeping.
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
//! wrote while it waited, unread and unacked.)

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus, MAX_LINE_BYTES};
use taria::{AgentInput, PROTOCOL_VERSION, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{broadcast, mpsc, watch};

use crate::server::MAX_KEY_REPEAT;

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
/// real name had been on the wire all along for the bridge to pass on. The one
/// thing dropped on the way is the message's own framing key, which belongs to
/// the transport rather than to the tree; see
/// [`from_line`](Self::from_line).
///
/// The **parsed form is authoritative for what the bridge decides**: `act`
/// validates node ids and advertised actions against it, and the tool layer
/// compares it to tell a tree that changed from one that did not. Neither of
/// those may read the relayed text, which is untyped and, across a version
/// mismatch, not even shaped like this build's [`Snapshot`].
///
/// A line reaches here only after it parsed, so a malformed line is still
/// skipped rather than forwarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSnapshot {
    /// The app's snapshot as the agent reads it: its own line, minus the
    /// message framing. What `read_tree` and every tool result hand back.
    /// Built by [`from_line`](Self::from_line), which is the only thing that
    /// removes anything.
    pub tree_json: String,
    /// The same message decoded into this build's types. For validation and
    /// change detection, never for output, except the action names in an
    /// outline, which have to be the names validation accepts.
    pub parsed: Snapshot,
}

impl AppSnapshot {
    /// Pair one app line with its parse, dropping the transport envelope from
    /// the copy the agent reads.
    ///
    /// [`AppToBridge`] is internally tagged, so the line carries a
    /// `"type":"snapshot"` key beside the snapshot's own fields. That key
    /// frames the message on the wire and says nothing about the tree, while
    /// `read_tree` promises the app's tree, so handing it on reads as a field
    /// the app published.
    ///
    /// Removed through a `serde_json::Value`, which holds every key the line
    /// has, including the roles, actions and fields this build has never
    /// heard of. Only the one key goes; key order and whitespace become the
    /// serializer's again, and neither is information the app was carrying.
    pub fn from_line(line: &str, parsed: Snapshot) -> Self {
        Self {
            tree_json: strip_envelope(line),
            parsed,
        }
    }
}

/// `line` without its `"type"` key.
///
/// Both fallbacks return the line untouched, and neither is reachable from a
/// line that got here: it parsed as an `AppToBridge::Snapshot`, so it is a
/// JSON object, and a `Value` built from JSON serializes back. Relaying the
/// envelope is the wrong answer for both, but it is the one that keeps the
/// tree, which is what an agent came for.
fn strip_envelope(line: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    if let Some(object) = value.as_object_mut() {
        object.remove("type");
    }
    serde_json::to_string(&value).unwrap_or_else(|_| line.to_string())
}

/// What the bridge currently knows about the app, as seen by the tool layer.
///
/// The distinction between [`Never`](Self::Never) and
/// [`Disconnected`](Self::Disconnected) exists purely for error messages:
/// "the app was never there" and "the app was there and died" call for very
/// different next steps from the agent.
#[derive(Clone, Debug, PartialEq)]
pub enum BridgeState {
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
/// the first one leaves) leave the state at [`BridgeState::Never`], and the two
/// call for opposite next steps: check the path, or leave the path alone
/// because the bridge is connected to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
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
    pub fn open_for(self) -> Option<Duration> {
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
pub struct BridgeHandle {
    /// Latest connection state; holds the current [`Snapshot`] while an app
    /// is connected.
    pub state_rx: watch::Receiver<BridgeState>,
    /// Queue of identified agent inputs to forward to the app.
    pub input_tx: mpsc::Sender<(InputId, AgentInput)>,
    /// Every ack the app sends, republished for whoever is waiting on one.
    pub ack_tx: broadcast::Sender<(InputId, InputStatus)>,
    /// The `protocol_version` the connected app declared in its handshake.
    ///
    /// `None` before any handshake, and again once the app goes away, so a
    /// stale version is never read as the current peer's. An app that sends
    /// no `Hello` at all leaves it `None`, which reads as "no mismatch
    /// known": the compatibility path for adapters that predate the
    /// handshake.
    ///
    /// The tool layer needs this because a peer on another protocol version
    /// still delivers parseable snapshots while being unable to parse the
    /// inputs sent back to it, and reporting an input as sent to such a peer
    /// would be a lie.
    pub protocol_rx: watch::Receiver<Option<u32>>,
    /// Why the most recent line from an app was thrown away, if one was.
    ///
    /// Set for every line the reader could not turn into an [`AppToBridge`],
    /// and kept past the connection, because the question it answers outlives
    /// it: an app that connects, publishes nothing this bridge can read and
    /// dies leaves the state at [`BridgeState::Never`], which on its own is
    /// indistinguishable from an app that never started. Reporting the reason
    /// is what stops `read_tree` sending an adopter to check a socket path the
    /// app plainly reached.
    ///
    /// Also set for a line over [`MAX_LINE_BYTES`], which ends the connection
    /// instead of being skipped. That reason is a fixed string, and the one
    /// value this channel ever clears, on either of two events. The next
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
    pub rejected_rx: watch::Receiver<Option<String>>,
    /// Whether this bridge currently holds an open connection, and since when.
    ///
    /// The one fact that separates "nothing is there" from "this bridge is
    /// connected and the peer has not said a word", which are the same
    /// [`BridgeState::Never`] and want opposite answers. See [`LinkState`].
    ///
    /// Read only where no snapshot is available, like
    /// [`rejected_rx`](Self::rejected_rx): once a tree is being served, the
    /// connection carrying it is not what an agent needs to hear about.
    pub link_rx: watch::Receiver<LinkState>,
}

impl BridgeHandle {
    /// Claim the id for the next input to send.
    ///
    /// Takes `&self` so the id source stays an implementation detail of the
    /// handle: callers ask the bridge for an id rather than reaching for a
    /// counter of their own, and two tool calls racing still get different
    /// ids.
    pub fn next_input_id(&self) -> InputId {
        NEXT_INPUT_ID.fetch_add(1, Ordering::Relaxed)
    }

    /// Start receiving acks from now on.
    ///
    /// A broadcast receiver only sees what is published after it subscribes,
    /// so a caller that waits on an input must subscribe *before* sending it:
    /// the app can ack before the sending task is scheduled again.
    pub fn subscribe_acks(&self) -> broadcast::Receiver<(InputId, InputStatus)> {
        self.ack_tx.subscribe()
    }
}

/// Spawn the socket-manager task for `socket_path` on the current runtime.
pub fn spawn(socket_path: PathBuf) -> BridgeHandle {
    let (state_tx, state_rx) = watch::channel(BridgeState::Never);
    let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
    let (ack_tx, _) = broadcast::channel(ACK_QUEUE);
    let (protocol_tx, protocol_rx) = watch::channel(None);
    let (rejected_tx, rejected_rx) = watch::channel(None);
    let (link_tx, link_rx) = watch::channel(LinkState::Down);
    tokio::spawn(manager_loop(
        socket_path,
        state_tx,
        input_rx,
        ack_tx.clone(),
        protocol_tx,
        rejected_tx,
        link_tx,
    ));
    BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
        protocol_rx,
        rejected_rx,
        link_rx,
    }
}

/// Why one served connection ended.
enum ConnectionEnd {
    /// The app closed the socket (or an I/O error occurred).
    AppClosed,
    /// Every [`BridgeHandle::input_tx`] clone was dropped: the MCP session is
    /// over, so the manager should exit instead of reconnecting.
    SessionClosed,
}

/// Connect-serve-reconnect forever (until the session is torn down).
async fn manager_loop(
    path: PathBuf,
    state_tx: watch::Sender<BridgeState>,
    mut input_rx: mpsc::Receiver<(InputId, AgentInput)>,
    ack_tx: broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: watch::Sender<Option<u32>>,
    rejected_tx: watch::Sender<Option<String>>,
    link_tx: watch::Sender<LinkState>,
) {
    let mut backoff = RETRY_MIN;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "connected to app socket");
                drain_stale_inputs(&mut input_rx);
                let connected_at = Instant::now();
                // Published before the connection is served, not after it has
                // said something: the whole point of this channel is the
                // connection that never says anything.
                link_tx.send_replace(LinkState::Open {
                    since: connected_at,
                });
                let mut conn_label = None;
                let end = run_connection(
                    stream,
                    &state_tx,
                    &mut input_rx,
                    &ack_tx,
                    &protocol_tx,
                    &rejected_tx,
                    &mut conn_label,
                )
                .await;
                // The connection is over: nothing is open again until the next
                // `connect` returns.
                link_tx.send_replace(LinkState::Down);
                // The peer's protocol version belongs to the connection that
                // declared it; forget it here so the next connection is never
                // judged by the previous app's handshake.
                protocol_tx.send_replace(None);
                // The watch only ever holds `Connected` while this connection
                // was being served (it is demoted below after every
                // connection), so `Connected` here means this connection
                // delivered a snapshot.
                let delivered_snapshot = matches!(&*state_tx.borrow(), BridgeState::Connected(_));
                // Demote `Connected` to `Disconnected`, remembering which app
                // died and its last seq. A connection that died before
                // delivering a snapshot leaves the previous state (`Never`,
                // or the `Disconnected` record of an older app) untouched.
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
                    ConnectionEnd::AppClosed => {
                        tracing::warn!(path = %path.display(), "app disconnected; reconnecting");
                    }
                    ConnectionEnd::SessionClosed => {
                        tracing::debug!("all input senders dropped; bridge task exiting");
                        return;
                    }
                }
                if delivered_snapshot || connected_at.elapsed() >= HEALTHY_CONNECTION_MIN {
                    // A healthy connection: reconnect eagerly, from scratch.
                    backoff = RETRY_MIN;
                } else {
                    // Accepted but died young without a snapshot: back off
                    // exactly like a failed connect, or a peer that always
                    // accept-and-drops spins this loop at full CPU.
                    tracing::debug!(
                        retry_in_ms = backoff.as_millis() as u64,
                        "connection died without a snapshot; backing off"
                    );
                    tokio::time::sleep(backoff).await;
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
                // behind, and the tools would go on telling the agent that a
                // running app resends the same tree to every reconnect, about
                // an app that is not there. Nothing listening is the one
                // connect failure that says the app is gone.
                forget_oversized_if_nothing_listens(&err, &rejected_tx);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
}

/// Retire [`OVERSIZED_LINE`] when a failed `connect` means nothing is
/// listening at the path: a socket file nobody is bound to any more
/// (`ConnectionRefused`), or no file at all (`NotFound`). Either way the app
/// that sent the tree has gone. Any other failure (a permission, a resource
/// limit) says nothing about whether the app is still there, and leaves the
/// reason alone.
fn forget_oversized_if_nothing_listens(
    err: &io::Error,
    rejected_tx: &watch::Sender<Option<String>>,
) {
    if matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    ) {
        forget_oversized(rejected_tx);
    }
}

/// Retire [`OVERSIZED_LINE`], and only that reason: any other one stays, as it
/// always has.
fn forget_oversized(rejected_tx: &watch::Sender<Option<String>>) {
    rejected_tx.send_if_modified(|rejected| {
        if rejected.as_deref() == Some(OVERSIZED_LINE) {
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
    reader: &mut BufReader<OwnedReadHalf>,
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

/// Serve one connection: pump app lines into the watch and agent inputs onto
/// the socket until either side goes away. The label from the app's `Hello`
/// (if any) is stored in `conn_label` for the disconnect record.
async fn run_connection(
    stream: UnixStream,
    state_tx: &watch::Sender<BridgeState>,
    input_rx: &mut mpsc::Receiver<(InputId, AgentInput)>,
    ack_tx: &broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: &watch::Sender<Option<u32>>,
    rejected_tx: &watch::Sender<Option<String>>,
    conn_label: &mut Option<String>,
) -> ConnectionEnd {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line_buf: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            read = read_line_capped(&mut reader, &mut line_buf) => match read {
                Ok(LineRead::Line) => {
                    handle_app_line(
                        &line_buf,
                        state_tx,
                        ack_tx,
                        protocol_tx,
                        rejected_tx,
                        conn_label,
                    );
                    line_buf.clear();
                }
                Ok(LineRead::Eof) => return ConnectionEnd::AppClosed,
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
                    return ConnectionEnd::AppClosed;
                }
                Err(err) => {
                    tracing::warn!(%err, "read error on app socket");
                    return ConnectionEnd::AppClosed;
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
                    return ConnectionEnd::AppClosed;
                }
            }
        }
    }
}

/// Handle one ndjson line from the app. Malformed lines are logged, recorded
/// in `rejected_tx` and skipped, so a buggy app cannot kill the bridge and an
/// agent asking for a tree is told what went wrong instead of being sent to
/// check the socket path.
fn handle_app_line(
    line: &[u8],
    state_tx: &watch::Sender<BridgeState>,
    ack_tx: &broadcast::Sender<(InputId, InputStatus)>,
    protocol_tx: &watch::Sender<Option<u32>>,
    rejected_tx: &watch::Sender<Option<String>>,
    conn_label: &mut Option<String>,
) {
    // Decoded from `&str` rather than `&[u8]` because the snapshot arm keeps
    // the line: JSON is defined over UTF-8, so a line that parses is valid
    // UTF-8 anyway, and taking the check first means the kept line is a
    // borrowed `&str` rather than a lossy conversion of bytes.
    let Ok(line) = str::from_utf8(line) else {
        tracing::warn!("ignoring a line from app that is not valid UTF-8");
        rejected_tx.send_replace(Some("the line is not valid UTF-8".to_string()));
        return;
    };
    match serde_json::from_str::<AppToBridge>(line) {
        Ok(AppToBridge::Hello {
            app_label,
            protocol_version,
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
            *conn_label = Some(app_label);
        }
        Ok(AppToBridge::Snapshot(parsed)) => {
            tracing::debug!(seq = parsed.seq, "snapshot received");
            // A snapshot that fits retires the oversized-line reason, and only
            // that one: a tree the bridge can read has replaced the one it
            // could not. Any other reason stays, as it always has. Cleared
            // before the state is promoted, so a tool call that sees this tree
            // can never pair the next disconnect with the old size.
            forget_oversized(rejected_tx);
            // The line is kept beside the parsed form, not instead of it: the
            // agent reads the line and the bridge reasons about the parse.
            // See [`AppSnapshot`].
            state_tx.send_replace(BridgeState::Connected(AppSnapshot::from_line(line, parsed)));
        }
        Ok(AppToBridge::Ack { id, status, .. }) => {
            tracing::debug!(id, ?status, "input ack received");
            // An ack nobody is waiting for is the normal case (the tool call
            // that sent the input has already returned), so a send with no
            // subscribers is not an error worth reporting.
            let _ = ack_tx.send((id, status));
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
        assert!(matches!(end, ConnectionEnd::AppClosed));
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
                &rejected_tx,
                &mut label,
            );
        };

        line(&hello);
        assert_eq!(rejected_rx.borrow().as_deref(), Some(OVERSIZED_LINE));
        line(&snapshot);
        assert_eq!(rejected_rx.borrow().as_deref(), None);

        // Any other reason is kept, as it always was.
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
}
