//! Socket manager: owns the connection to the app's taria Unix socket.
//!
//! A single background task connects (retrying forever with capped backoff),
//! reads `AppToBridge` ndjson lines into a [`watch`] channel holding the
//! latest [`BridgeState`], and writes queued [`AgentInput`]s out as
//! `BridgeToApp::Input` lines. On disconnect the watch flips to
//! [`BridgeState::Disconnected`] so tool calls fail fast (instead of acting
//! on a stale tree) with an error that says which app went away.
//!
//! Every outgoing input carries an [`InputId`] and the app answers it with an
//! `AppToBridge::Ack`. Acks are republished on a [`broadcast`] channel rather
//! than kept here, because only the tool call that sent an input knows which
//! id it is waiting for; the manager stays free of per-input bookkeeping.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{AgentInput, PROTOCOL_VERSION, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{broadcast, mpsc, watch};

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
/// Acks buffered per subscriber. A waiter only needs the acks published while
/// its own input is in flight, and one input draws at most two of them, so
/// this leaves room for a burst of concurrent tool calls without ever making
/// a slow subscriber miss the answer it is waiting for.
const ACK_QUEUE: usize = 64;
/// Longest accepted ndjson line from the app. A peer that streams more than
/// this without a newline is treated as a broken connection (disconnect and
/// reconnect) so the bridge never buffers a line unboundedly.
const MAX_LINE_BYTES: usize = 1024 * 1024;

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
    Connected(Snapshot),
    /// A previously connected app went away after delivering snapshots.
    Disconnected {
        /// Label from the app's handshake, if one was received.
        app_label: Option<String>,
        /// `seq` of the last snapshot delivered before the app went away.
        last_seq: u64,
    },
}

/// Source of the [`InputId`]s the tool layer stamps on outgoing inputs.
///
/// Process-wide and monotonic. The protocol only asks for ids that are unique
/// within one connection, and monotonic ids give that for free; they also make
/// an ack left over from a previous connection trivially non-matching, since
/// no waiter is ever looking for an id that low again.
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
    tokio::spawn(manager_loop(
        socket_path,
        state_tx,
        input_rx,
        ack_tx.clone(),
    ));
    BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
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
) {
    let mut backoff = RETRY_MIN;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "connected to app socket");
                drain_stale_inputs(&mut input_rx);
                let connected_at = Instant::now();
                let mut conn_label = None;
                let end =
                    run_connection(stream, &state_tx, &mut input_rx, &ack_tx, &mut conn_label)
                        .await;
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
                            last_seq: snapshot.seq,
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
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
    }
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
    conn_label: &mut Option<String>,
) -> ConnectionEnd {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line_buf: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            read = read_line_capped(&mut reader, &mut line_buf) => match read {
                Ok(LineRead::Line) => {
                    handle_app_line(&line_buf, state_tx, ack_tx, conn_label);
                    line_buf.clear();
                }
                Ok(LineRead::Eof) => return ConnectionEnd::AppClosed,
                Ok(LineRead::Overflow) => {
                    tracing::warn!(
                        cap_bytes = MAX_LINE_BYTES,
                        "app sent an oversized line; treating the connection as broken"
                    );
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
                let msg = BridgeToApp::Input { id, input };
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

/// Handle one ndjson line from the app. Malformed lines are logged and
/// skipped so a buggy app cannot kill the bridge.
fn handle_app_line(
    line: &[u8],
    state_tx: &watch::Sender<BridgeState>,
    ack_tx: &broadcast::Sender<(InputId, InputStatus)>,
    conn_label: &mut Option<String>,
) {
    match serde_json::from_slice::<AppToBridge>(line) {
        Ok(AppToBridge::Hello {
            app_label,
            protocol_version,
        }) => {
            if protocol_version == PROTOCOL_VERSION {
                tracing::info!(app_label, protocol_version, "app handshake received");
            } else {
                tracing::warn!(
                    app_label,
                    app_protocol = protocol_version,
                    bridge_protocol = PROTOCOL_VERSION,
                    "protocol version mismatch; continuing, but messages may misparse"
                );
            }
            *conn_label = Some(app_label);
        }
        Ok(AppToBridge::Snapshot(snapshot)) => {
            tracing::debug!(seq = snapshot.seq, "snapshot received");
            state_tx.send_replace(BridgeState::Connected(snapshot));
        }
        Ok(AppToBridge::Ack { id, status }) => {
            tracing::debug!(id, ?status, "input ack received");
            // An ack nobody is waiting for is the normal case (the tool call
            // that sent the input has already returned), so a send with no
            // subscribers is not an error worth reporting.
            let _ = ack_tx.send((id, status));
        }
        Err(err) => {
            tracing::warn!(%err, "ignoring malformed line from app");
        }
    }
}
