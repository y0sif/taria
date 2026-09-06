//! Socket manager: owns the connection to the app's taria Unix socket.
//!
//! A single background task connects (retrying forever with capped backoff),
//! reads `AppToBridge` ndjson lines into a [`watch`] channel holding the
//! latest [`Snapshot`], and writes queued [`AgentInput`]s out as
//! `BridgeToApp::Input` lines. On disconnect the watch is cleared to `None`
//! so tool calls fail fast instead of acting on a stale tree.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use taria::wire::{AppToBridge, BridgeToApp};
use taria::{AgentInput, PROTOCOL_VERSION, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{mpsc, watch};

/// First reconnect delay after a failed connect attempt.
const RETRY_MIN: Duration = Duration::from_millis(250);
/// Backoff cap; the manager keeps retrying at this pace forever.
const RETRY_MAX: Duration = Duration::from_secs(2);
/// Queued agent inputs awaiting the socket writer.
const INPUT_QUEUE: usize = 32;
/// Longest accepted ndjson line from the app. A peer that streams more than
/// this without a newline is treated as a broken connection (disconnect and
/// reconnect) so the bridge never buffers a line unboundedly.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Handles the MCP tool layer uses to talk to the socket-manager task.
#[derive(Clone)]
pub struct BridgeHandle {
    /// Latest snapshot from the app; `None` while disconnected or before the
    /// first snapshot arrives.
    pub snapshot_rx: watch::Receiver<Option<Snapshot>>,
    /// Queue of agent inputs to forward to the app.
    pub input_tx: mpsc::Sender<AgentInput>,
}

/// Spawn the socket-manager task for `socket_path` on the current runtime.
pub fn spawn(socket_path: PathBuf) -> BridgeHandle {
    let (snapshot_tx, snapshot_rx) = watch::channel(None);
    let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
    tokio::spawn(manager_loop(socket_path, snapshot_tx, input_rx));
    BridgeHandle {
        snapshot_rx,
        input_tx,
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
    snapshot_tx: watch::Sender<Option<Snapshot>>,
    mut input_rx: mpsc::Receiver<AgentInput>,
) {
    let mut backoff = RETRY_MIN;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                tracing::info!(path = %path.display(), "connected to app socket");
                backoff = RETRY_MIN;
                let end = run_connection(stream, &snapshot_tx, &mut input_rx).await;
                snapshot_tx.send_replace(None);
                match end {
                    ConnectionEnd::AppClosed => {
                        tracing::warn!(path = %path.display(), "app disconnected; reconnecting");
                    }
                    ConnectionEnd::SessionClosed => {
                        tracing::debug!("all input senders dropped; bridge task exiting");
                        return;
                    }
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
/// the socket until either side goes away.
async fn run_connection(
    stream: UnixStream,
    snapshot_tx: &watch::Sender<Option<Snapshot>>,
    input_rx: &mut mpsc::Receiver<AgentInput>,
) -> ConnectionEnd {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line_buf: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            read = read_line_capped(&mut reader, &mut line_buf) => match read {
                Ok(LineRead::Line) => {
                    handle_app_line(&line_buf, snapshot_tx);
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
                let Some(input) = input else {
                    return ConnectionEnd::SessionClosed;
                };
                let msg = BridgeToApp::Input(input);
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
fn handle_app_line(line: &[u8], snapshot_tx: &watch::Sender<Option<Snapshot>>) {
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
        }
        Ok(AppToBridge::Snapshot(snapshot)) => {
            tracing::debug!(seq = snapshot.seq, "snapshot received");
            snapshot_tx.send_replace(Some(snapshot));
        }
        Err(err) => {
            tracing::warn!(%err, "ignoring malformed line from app");
        }
    }
}
