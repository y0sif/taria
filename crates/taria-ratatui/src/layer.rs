//! The [`TariaLayer`]: socket lifecycle, background threads, snapshot
//! publishing, and input acknowledgement for a ratatui app.

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};

use crate::FrameRecorder;

/// How long a snapshot write may stall on a slow or stuck bridge before the
/// connection is dropped. Protects the app from a peer that stops reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest accepted ndjson line from a connected client. A peer that streams
/// more than this without a newline is treated as broken and disconnected,
/// so a hostile or buggy bridge cannot make the app buffer unboundedly.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Capacity of the agent-input queue between the socket thread and the app.
/// When the app is not draining inputs, the newest input is dropped, counted
/// (see [`TariaLayer::dropped_inputs`]) and acked
/// [`Dropped`](InputStatus::Dropped), instead of blocking the socket thread
/// or queueing without bound.
const INPUT_QUEUE: usize = 256;

/// The embeddable taria endpoint for a ratatui app.
///
/// Binding a layer opens a Unix domain socket and spawns a listener thread
/// that serves one bridge client at a time: on connect it sends an
/// [`AppToBridge::Hello`] handshake plus the latest snapshot, then streams
/// every newly published snapshot and forwards incoming [`AgentInput`]
/// messages to the app via [`try_recv`](Self::try_recv) /
/// [`recv_timeout`](Self::recv_timeout). Every forwarded input is answered
/// with an [`AppToBridge::Ack`], so an agent can tell an input the app acted
/// on from one it never saw.
///
/// A layer may also be *disabled*: [`bind_or_disabled`](Self::bind_or_disabled)
/// hands back an inert layer rather than an error, so taria failing to bind
/// can never stop an app from starting. Every method stays callable on a
/// disabled layer, which is what lets an app keep one code path.
///
/// Dropping the layer shuts the threads down and removes the socket file
/// (best-effort).
pub struct TariaLayer {
    app_label: String,
    socket_path: PathBuf,
    /// Everything that exists only once the socket is bound. `None` on a
    /// disabled layer, so no method can reach machinery that is not running.
    inner: Option<Inner>,
    /// Why binding failed, on a disabled layer.
    bind_error: Option<io::Error>,
}

/// The parts of a layer that exist only while its socket is bound.
struct Inner {
    shared: Arc<Shared>,
    input_rx: Receiver<(InputId, AgentInput)>,
    listener: Option<JoinHandle<()>>,
    seq: u64,
    last_root: Option<Node>,
}

impl TariaLayer {
    /// Bind the taria socket for this app and start serving.
    ///
    /// The socket path is resolved by [`taria::socket::resolve_path`], in
    /// order of preference:
    ///
    /// 1. `$TARIA_SOCK` verbatim, if set and non-empty;
    /// 2. `$XDG_RUNTIME_DIR/taria/<app_label>.sock`;
    /// 3. `<temp_dir>/taria-<uid or user>/<app_label>.sock`.
    ///
    /// The parent directory is created with mode `0700` and then verified to
    /// be a private directory (a real directory, owned by the current user,
    /// with no group/other permission bits); binding is refused otherwise,
    /// because a directory another user controls would let them replace or
    /// redirect the socket. A stale socket file at the path is removed before
    /// binding.
    ///
    /// Apps that would rather run without taria than not run at all should
    /// use [`bind_or_disabled`](Self::bind_or_disabled).
    pub fn bind(app_label: &str) -> io::Result<Self> {
        Self::bind_at(app_label, resolve_socket_path(app_label))
    }

    /// Like [`bind`](Self::bind), but at an explicit socket path, skipping
    /// resolution. Useful for tests and apps that manage their own runtime
    /// directories. The same parent-directory creation and privacy checks as
    /// [`bind`](Self::bind) apply: a relative path (including a bare
    /// filename) is resolved against the current directory first, so the
    /// parent directory that would hold the socket is always vetted.
    pub fn bind_at(app_label: &str, socket_path: impl Into<PathBuf>) -> io::Result<Self> {
        // Absolutize before looking at the parent: a bare filename like
        // "app.sock" has an empty parent, which must not bypass the privacy
        // check by silently binding in an unvetted current directory.
        let socket_path = absolutize(socket_path.into())?;

        // Check the length here, before anything is created: over the limit,
        // `bind` below would fail with `InvalidInput: path must be shorter
        // than SUN_LEN`, which names neither the path, its length, the limit,
        // nor a way out. `SocketPathTooLong` names all four.
        taria::socket::check_path_len(&socket_path)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;

        if let Some(parent) = socket_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }
        // Remove a stale socket left by a previous run; if this fails for any
        // reason other than the file being absent, bind reports the real error.
        let _ = fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        let shared = Arc::new(Shared {
            app_label: app_label.to_string(),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            dropped_inputs: AtomicU64::new(0),
        });
        let (input_tx, input_rx) = mpsc::sync_channel(INPUT_QUEUE);

        let thread_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("taria-listener".into())
            .spawn(move || accept_loop(listener, thread_shared, input_tx));
        let handle = match handle {
            Ok(handle) => handle,
            Err(err) => {
                let _ = fs::remove_file(&socket_path);
                return Err(err);
            }
        };

        Ok(Self {
            app_label: app_label.to_string(),
            socket_path,
            inner: Some(Inner {
                shared,
                input_rx,
                listener: Some(handle),
                seq: 0,
                last_root: None,
            }),
            bind_error: None,
        })
    }

    /// Bind like [`bind`](Self::bind), or return a disabled layer when that
    /// fails, so an app never has to choose between starting and having
    /// taria.
    ///
    /// A disabled layer answers every method: publishing does nothing, no
    /// input ever arrives, and [`frame`](Self::frame) still hands back a
    /// recorder, so the render path needs no branch on whether taria came up.
    /// [`is_enabled`](Self::is_enabled) and [`bind_error`](Self::bind_error)
    /// report what happened.
    ///
    /// The layer deliberately prints nothing itself: once the app owns the
    /// alternate screen, a stray print garbles the display. Print
    /// [`bind_error`](Self::bind_error) at the moment the app chooses, which
    /// is usually before entering the alternate screen or after leaving it.
    pub fn bind_or_disabled(app_label: &str) -> Self {
        Self::bind_or_disabled_at(app_label, resolve_socket_path(app_label))
    }

    /// [`bind_or_disabled`](Self::bind_or_disabled) at an explicit path, so
    /// the disabled branch can be exercised without mutating the process
    /// environment (which no test can do safely while other tests run).
    fn bind_or_disabled_at(app_label: &str, socket_path: PathBuf) -> Self {
        match Self::bind_at(app_label, socket_path.clone()) {
            Ok(layer) => layer,
            Err(err) => Self {
                app_label: app_label.to_string(),
                socket_path,
                inner: None,
                bind_error: Some(err),
            },
        }
    }

    /// Is this layer serving on a socket?
    ///
    /// False only for a disabled layer from
    /// [`bind_or_disabled`](Self::bind_or_disabled); see
    /// [`bind_error`](Self::bind_error) for why it is disabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Why binding failed, on a disabled layer. `None` while the layer is
    /// serving.
    ///
    /// The layer never prints this itself, because a print while the
    /// alternate screen is up garbles the display. The app decides when it is
    /// safe to show and whether it is worth showing at all.
    pub fn bind_error(&self) -> Option<&io::Error> {
        self.bind_error.as_ref()
    }

    /// The path of the Unix socket this layer is serving on, or the path it
    /// tried to bind if it is disabled, so an app can print either.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The label passed to [`bind`](Self::bind), used in the handshake and as
    /// the auto-generated root node's label.
    pub fn app_label(&self) -> &str {
        &self.app_label
    }

    /// Begin recording a new frame. Record nodes with
    /// [`FrameRecorder::push`] or [`sem`](crate::sem), then call
    /// [`FrameRecorder::publish`].
    ///
    /// Works on a disabled layer too, where publishing is simply a no-op, so
    /// the render path never branches on whether taria is up.
    pub fn frame(&mut self) -> FrameRecorder<'_> {
        FrameRecorder::new(self)
    }

    /// Non-blocking poll for the next agent input, if one has arrived.
    ///
    /// Acks the input [`Delivered`](InputStatus::Delivered) on its way out.
    /// Delivered means the app's event loop dequeued it, nothing more; an app
    /// that then decides to do nothing with it should say so with
    /// [`ack`](Self::ack), whose id comes from
    /// [`try_recv_with_id`](Self::try_recv_with_id).
    ///
    /// Always `None` on a disabled layer.
    pub fn try_recv(&self) -> Option<AgentInput> {
        self.try_recv_with_id().map(|(_, input)| input)
    }

    /// Wait up to `timeout` for the next agent input.
    ///
    /// Acks [`Delivered`](InputStatus::Delivered) exactly like
    /// [`try_recv`](Self::try_recv). A disabled layer has nothing to wait for
    /// but still waits out the timeout, so an app that paces its loop on this
    /// call keeps its timing whether or not taria bound.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AgentInput> {
        self.recv_timeout_with_id(timeout).map(|(_, input)| input)
    }

    /// [`try_recv`](Self::try_recv), keeping the [`InputId`] so the app can
    /// answer the input again later with [`ack`](Self::ack).
    ///
    /// The ack still happens here, so an app can never leave an input
    /// unacknowledged by forgetting to call something.
    pub fn try_recv_with_id(&self) -> Option<(InputId, AgentInput)> {
        let inner = self.inner.as_ref()?;
        let (id, input) = inner.input_rx.try_recv().ok()?;
        inner.shared.queue_ack(id, InputStatus::Delivered);
        Some((id, input))
    }

    /// [`recv_timeout`](Self::recv_timeout), keeping the [`InputId`] so the
    /// app can answer the input again later with [`ack`](Self::ack).
    pub fn recv_timeout_with_id(&self, timeout: Duration) -> Option<(InputId, AgentInput)> {
        let Some(inner) = self.inner.as_ref() else {
            // No socket, so no input will ever arrive. Sleep it out anyway: a
            // caller using this as its clock must not spin because taria
            // happened to be unavailable.
            thread::sleep(timeout);
            return None;
        };
        let (id, input) = inner.input_rx.recv_timeout(timeout).ok()?;
        inner.shared.queue_ack(id, InputStatus::Delivered);
        Some((id, input))
    }

    /// Hand every queued agent input to `f`, in arrival order.
    ///
    /// The drain loop an app otherwise writes by hand, once per place it
    /// polls for input, with nothing keeping the copies in step.
    pub fn drain(&self, mut f: impl FnMut(AgentInput)) {
        while let Some(input) = self.try_recv() {
            f(input);
        }
    }

    /// [`drain`](Self::drain), passing each input's [`InputId`] alongside it
    /// so the handler can [`ack`](Self::ack) whatever it chose to ignore.
    pub fn drain_with_ids(&self, mut f: impl FnMut(InputId, AgentInput)) {
        while let Some((id, input)) = self.try_recv_with_id() {
            f(id, input);
        }
    }

    /// Answer an input again, overriding the ack it already has.
    ///
    /// Last ack wins. An app that dequeued an input (acked
    /// [`Delivered`](InputStatus::Delivered) by that dequeue) and then
    /// deliberately did nothing with it, say an act blocked by a modal dialog
    /// or naming a node it does not know, can refine that to
    /// [`Ignored`](InputStatus::Ignored) so an agent waiting on an effect
    /// stops waiting.
    ///
    /// The ack travels the same path as snapshots, so it can never overtake
    /// the snapshot published after it. Does nothing on a disabled layer.
    pub fn ack(&self, id: InputId, status: InputStatus) {
        if let Some(inner) = self.inner.as_ref() {
            inner.shared.queue_ack(id, status);
        }
    }

    /// How many agent inputs have been dropped so far because the input
    /// queue was full. Always 0 on a disabled layer.
    ///
    /// When an agent floods inputs faster than the app drains them, the layer
    /// drops the newest input rather than blocking the socket thread. The
    /// agent learns of each drop from its [`Dropped`](InputStatus::Dropped)
    /// ack; this counter is the app-facing tally of the same event, kept
    /// because the layer must not print while the app owns the alternate
    /// screen. It is monotonic and accumulates across reconnects for the
    /// lifetime of the layer.
    ///
    /// Apps should read it after restoring the terminal (for example right
    /// before exit, once the alternate screen has been left) and report a
    /// nonzero value on stderr or in a log.
    pub fn dropped_inputs(&self) -> u64 {
        self.inner.as_ref().map_or(0, |inner| {
            inner.shared.dropped_inputs.load(Ordering::Relaxed)
        })
    }

    /// Publish `nodes` as a snapshot, without going through a
    /// [`FrameRecorder`].
    ///
    /// For apps that build their node list separately from drawing, where the
    /// recorder's frame, push-loop and publish triplet is three lines saying
    /// one thing. The nodes are wrapped in the same auto-generated root, so
    /// both paths produce the same tree.
    pub fn publish(&mut self, nodes: impl IntoIterator<Item = Node>) {
        self.publish_nodes(nodes.into_iter().collect());
    }

    /// Publish a frame's recorded top-level nodes as a new snapshot.
    ///
    /// Wraps the nodes in an auto-generated `app` root (focused only if no
    /// recorded node is), bumps `seq`, and hands the snapshot to the writer
    /// thread. Skipped entirely when the tree is identical to the previous
    /// publish, and on a disabled layer. Never blocks the render path beyond
    /// a brief mutex hold.
    pub(crate) fn publish_nodes(&mut self, nodes: Vec<Node>) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let focus_recorded = nodes.iter().any(subtree_has_focus);
        let root = Node::new("app", Role::App)
            .label(inner.shared.app_label.clone())
            .focused(!focus_recorded)
            .children(nodes);

        if inner.last_root.as_ref() == Some(&root) {
            return;
        }
        inner.seq += 1;
        let snapshot = Snapshot::new(inner.seq, root.clone());
        inner.last_root = Some(root);

        let mut state = inner.shared.lock_state();
        state.latest = Some(snapshot);
        state.epoch += 1;
        drop(state);
        inner.shared.cv.notify_all();
    }

    /// The most recently published snapshot, if any. Test-only introspection.
    #[cfg(test)]
    pub(crate) fn latest_snapshot(&self) -> Option<Snapshot> {
        self.inner.as_ref()?.shared.lock_state().latest.clone()
    }
}

impl Drop for TariaLayer {
    /// Stop the threads and unlink the socket. A disabled layer has neither,
    /// so it touches no filesystem at all.
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        {
            let mut state = inner.shared.lock_state();
            state.shutdown = true;
            inner.shared.cv.notify_all();
        }
        // Wake the listener thread if it is blocked in accept(); the dummy
        // connection is never served because the shutdown flag is checked
        // right after accept returns.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(handle) = inner.listener.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// State shared between the app thread and the connection threads.
struct Shared {
    app_label: String,
    state: Mutex<State>,
    cv: Condvar,
    /// Agent inputs dropped because the input queue was full. Written by
    /// reader threads, read via [`TariaLayer::dropped_inputs`].
    dropped_inputs: AtomicU64,
}

#[derive(Default)]
struct State {
    /// Latest published snapshot; sent to newly connected clients and, via
    /// `epoch` bumps, streamed to the current client.
    latest: Option<Snapshot>,
    /// Bumped on every publish so the writer knows something new exists.
    epoch: u64,
    /// Acks waiting to go out, oldest first. Queued for the writer rather
    /// than written where they are produced, so that one thread owns the
    /// stream and an ack can never overtake the snapshot published after it.
    acks: VecDeque<(InputId, InputStatus)>,
    shutdown: bool,
}

impl Shared {
    /// Lock the state, recovering from poisoning (a panicking connection
    /// thread must not take the app down with it).
    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_shutdown(&self) -> bool {
        self.lock_state().shutdown
    }

    /// Queue an ack for the writer and wake it.
    fn queue_ack(&self, id: InputId, status: InputStatus) {
        let mut state = self.lock_state();
        state.acks.push_back((id, status));
        drop(state);
        self.cv.notify_all();
    }

    /// Forget every queued ack. An [`InputId`] is unique only within one
    /// connection, so an ack outliving its connection would name an input the
    /// next client never sent.
    fn clear_acks(&self) {
        self.lock_state().acks.clear();
    }
}

/// Does `node` or any of its descendants have focus?
fn subtree_has_focus(node: &Node) -> bool {
    node.focused || node.children.iter().any(subtree_has_focus)
}

/// Accept loop: serves one bridge client at a time until shutdown.
fn accept_loop(
    listener: UnixListener,
    shared: Arc<Shared>,
    input_tx: SyncSender<(InputId, AgentInput)>,
) {
    loop {
        if shared.is_shutdown() {
            return;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                if shared.is_shutdown() {
                    return;
                }
                // Errors just drop this client; the loop keeps accepting.
                let _ = serve_client(stream, &shared, &input_tx);
            }
            Err(_) => {
                if shared.is_shutdown() {
                    return;
                }
                // Avoid a hot loop on a persistently failing listener.
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Serve one connected bridge: handshake, then reader thread + writer loop
/// until the client disconnects or the layer shuts down.
fn serve_client(
    stream: UnixStream,
    shared: &Arc<Shared>,
    input_tx: &SyncSender<(InputId, AgentInput)>,
) -> io::Result<()> {
    // SO_SNDTIMEO is shared across the duplicated fds; only writes block long
    // enough to need it.
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut write_stream = stream.try_clone()?;

    // Capture the snapshot and epoch atomically so the writer loop neither
    // misses nor duplicates a publish that races the handshake. Acks left
    // over from the previous connection go here: they name ids this client
    // never used. An input queued before that connection dropped can still be
    // dequeued (and so acked) after this one starts, so a bridge must ignore
    // acks for ids it has no input pending for.
    let (initial_snapshot, initial_epoch) = {
        let mut state = shared.lock_state();
        state.acks.clear();
        (state.latest.clone(), state.epoch)
    };
    write_line(
        &mut write_stream,
        &AppToBridge::Hello {
            app_label: shared.app_label.clone(),
            protocol_version: PROTOCOL_VERSION,
        },
    )?;
    if let Some(snapshot) = initial_snapshot {
        write_line(&mut write_stream, &AppToBridge::Snapshot(snapshot))?;
    }

    let alive = Arc::new(AtomicBool::new(true));
    let reader = thread::Builder::new().name("taria-reader".into()).spawn({
        let alive = Arc::clone(&alive);
        let shared = Arc::clone(shared);
        let input_tx = input_tx.clone();
        move || reader_loop(stream, input_tx, alive, shared)
    })?;

    writer_loop(&mut write_stream, shared, &alive, initial_epoch);

    // Unblock the reader (it is parked in a blocking read) and reap it before
    // going back to accept the next client.
    let _ = write_stream.shutdown(Shutdown::Both);
    let _ = reader.join();
    // Whatever is still queued can no longer be delivered, and means nothing
    // to the next client.
    shared.clear_acks();
    Ok(())
}

/// Reader half of a connection: parse `BridgeToApp` lines and forward agent
/// inputs to the app. Malformed lines are ignored. A line longer than
/// [`MAX_LINE_BYTES`] marks the connection broken (the loop exits and the
/// client is disconnected) instead of buffering it. When the input queue is
/// full the newest input is dropped, acked [`Dropped`](InputStatus::Dropped)
/// so the agent learns of it, and counted for the app (see
/// [`TariaLayer::dropped_inputs`]); a flood of inputs can never block this
/// thread.
fn reader_loop(
    stream: UnixStream,
    input_tx: SyncSender<(InputId, AgentInput)>,
    alive: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Read at most one byte past the cap: a line that fits ends in `\n`
        // within the limit; anything longer is an oversized frame.
        let read = (&mut reader)
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut buf);
        let n = match read {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break; // EOF: the client closed the connection.
        }
        if buf.last() != Some(&b'\n') {
            // Oversized line (or EOF mid-line): treat the connection as
            // broken rather than accumulating an unbounded buffer.
            break;
        }
        let Ok(msg) = serde_json::from_slice::<BridgeToApp>(&buf) else {
            continue;
        };
        let BridgeToApp::Input { id, input } = msg;
        match input_tx.try_send((id, input)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                shared.dropped_inputs.fetch_add(1, Ordering::Relaxed);
                shared.queue_ack(id, InputStatus::Dropped);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    alive.store(false, Ordering::SeqCst);
    // Notify while holding the lock so the writer cannot miss the wakeup
    // between checking `alive` and parking on the condvar.
    let guard = shared.lock_state();
    shared.cv.notify_all();
    drop(guard);
}

/// Writer half of a connection: send queued acks, then each newly published
/// snapshot, as lines. Acks go first in every pass, so an ack always reaches
/// the client before the snapshot published after it; an agent that saw the
/// snapshot first could not tell whether it reflects its input yet.
///
/// Returns when the layer shuts down, the reader reports the client gone, or
/// a write fails.
fn writer_loop(stream: &mut UnixStream, shared: &Shared, alive: &AtomicBool, mut last_epoch: u64) {
    loop {
        let (acks, snapshot) = {
            let mut state = shared.lock_state();
            loop {
                if state.shutdown || !alive.load(Ordering::SeqCst) {
                    return;
                }
                if !state.acks.is_empty() || state.epoch != last_epoch {
                    let acks = std::mem::take(&mut state.acks);
                    let snapshot = if state.epoch == last_epoch {
                        None
                    } else {
                        last_epoch = state.epoch;
                        state.latest.clone()
                    };
                    break (acks, snapshot);
                }
                state = shared
                    .cv
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        for (id, status) in acks {
            if write_line(stream, &AppToBridge::Ack { id, status }).is_err() {
                return;
            }
        }
        if let Some(snapshot) = snapshot
            && write_line(stream, &AppToBridge::Snapshot(snapshot)).is_err()
        {
            return;
        }
    }
}

/// Serialize one message as a newline-terminated JSON frame and write it.
fn write_line(stream: &mut UnixStream, msg: &AppToBridge) -> io::Result<()> {
    let mut line = serde_json::to_string(msg).map_err(io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Make `path` absolute, resolving a relative path (including a bare
/// filename) against the current directory. Ensures the socket path always
/// has a real parent directory for [`ensure_private_dir`] to vet.
fn absolutize(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

/// Create (if needed) and vet the directory that will hold the socket.
///
/// The socket's parent directory decides who can replace or redirect the
/// socket: if it is a symlink, owned by another user, or accessible to
/// group/others, a local attacker can swap the socket for their own and
/// impersonate the app (or intercept the bridge). The directory is created
/// with mode `0700`, then verified via `symlink_metadata`: it must be a
/// real directory (not a symlink), owned by the current user, with no
/// group/other permission bits. Any violation is an error.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    // An existing directory is fine here; whether it is *acceptable* is
    // decided by the checks below, which also surface real create failures.
    let _ = builder.create(dir);

    // symlink_metadata so a planted symlink is seen as itself, not followed.
    let meta = fs::symlink_metadata(dir).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("socket directory {} is unusable: {err}", dir.display()),
        )
    })?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::other(format!(
            "socket directory {} is not a real directory (it may be a symlink planted by \
             another user to hijack the socket); refusing to use it",
            dir.display()
        )));
    }
    let uid = current_uid()?;
    if meta.uid() != uid {
        return Err(io::Error::other(format!(
            "socket directory {} is owned by uid {} instead of the current user (uid {}); \
             its owner could replace the socket to hijack the connection; refusing to use it",
            dir.display(),
            meta.uid(),
            uid
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "socket directory {} is group/world-accessible (mode {:03o}); other users could \
             replace the socket to hijack the connection; re-create it with mode 0700",
            dir.display(),
            meta.mode() & 0o777
        )));
    }
    Ok(())
}

/// The current effective uid, without `unsafe` or extra dependencies.
///
/// On Linux, `/proc/self` is owned by this process's effective uid. On other
/// Unixes, fall back to creating a probe file in the system temp dir: a file
/// this process creates is owned by its effective uid.
fn current_uid() -> io::Result<u32> {
    if let Ok(meta) = fs::metadata("/proc/self") {
        return Ok(meta.uid());
    }
    let probe = env::temp_dir().join(format!("taria-uid-probe-{}", std::process::id()));
    let _ = fs::remove_file(&probe);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)?;
    let uid = file.metadata()?.uid();
    drop(file);
    let _ = fs::remove_file(&probe);
    Ok(uid)
}

/// Resolve the default socket path for `app_label` from the environment.
///
/// The rule itself lives in [`taria::socket::resolve_path`], shared with the
/// bridge so the two sides cannot look for the socket in different places.
/// Reading the environment stays here, where the process actually is.
fn resolve_socket_path(app_label: &str) -> PathBuf {
    taria::socket::resolve_path(
        env::var_os("TARIA_SOCK"),
        env::var_os("XDG_RUNTIME_DIR"),
        &env::temp_dir(),
        &user_identity(),
        app_label,
    )
}

/// Uid where available (via `/proc/self` on Linux), else `$USER`/`$LOGNAME`,
/// else a fixed fallback. Only used to namespace the temp-dir fallback path.
fn user_identity() -> String {
    if let Ok(meta) = fs::metadata("/proc/self") {
        return meta.uid().to_string();
    }
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::test_util::bind_test_layer;

    /// Unique, absent scratch path under the system temp dir.
    fn scratch_path(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("taria-dirtest-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        let _ = fs::remove_file(&path);
        path
    }

    /// A private (0700) temp dir that removes itself when the test ends, so
    /// nothing is left in the system temp dir even on panic.
    fn private_temp_dir(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .expect("create test dir")
    }

    #[test]
    fn ensure_private_dir_accepts_fresh_0700_dir() {
        let dir = scratch_path("fresh");
        ensure_private_dir(&dir).expect("fresh private dir should be accepted");
        let meta = fs::symlink_metadata(&dir).unwrap();
        assert!(meta.file_type().is_dir());
        assert_eq!(meta.mode() & 0o077, 0, "mode: {:03o}", meta.mode() & 0o777);
        // And it stays acceptable on a second call (existing private dir).
        ensure_private_dir(&dir).expect("existing private dir should be accepted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_private_dir_rejects_symlinked_dir() {
        let target = scratch_path("symlink-target");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let link = scratch_path("symlink-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = ensure_private_dir(&link).expect_err("symlinked dir must be rejected");
        assert!(err.to_string().contains("symlink"), "err: {err}");

        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn ensure_private_dir_rejects_world_accessible_dir() {
        let dir = scratch_path("world-writable");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        let err = ensure_private_dir(&dir).expect_err("0777 dir must be rejected");
        assert!(err.to_string().contains("group/world"), "err: {err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolutize_joins_relative_paths_onto_cwd() {
        let cwd = env::current_dir().unwrap();

        let bare = absolutize(PathBuf::from("bare.sock")).unwrap();
        assert!(bare.is_absolute());
        assert_eq!(bare, cwd.join("bare.sock"));
        // The parent is now the (vetable) cwd, not the empty path.
        assert_eq!(bare.parent(), Some(cwd.as_path()));

        let nested = absolutize(PathBuf::from("sub/dir/app.sock")).unwrap();
        assert_eq!(nested, cwd.join("sub/dir/app.sock"));

        let absolute = absolutize(PathBuf::from("/already/abs.sock")).unwrap();
        assert_eq!(absolute, PathBuf::from("/already/abs.sock"));
    }

    /// A bare filename must not bypass the parent-directory privacy check:
    /// it resolves against the cwd, and binding succeeds only when the cwd
    /// itself passes [`ensure_private_dir`]. Tested without touching
    /// `set_current_dir` (racy across parallel tests) by comparing against
    /// vetting the cwd directly.
    #[test]
    fn bind_at_bare_filename_vets_cwd() {
        let cwd = env::current_dir().unwrap();
        let name = format!("taria-bare-vet-{}.sock", std::process::id());
        let result = TariaLayer::bind_at("bare-demo", &name);

        match ensure_private_dir(&cwd) {
            Ok(()) => {
                let layer = result.expect("private cwd: bare filename should bind in it");
                assert_eq!(layer.socket_path(), cwd.join(&name));
                drop(layer); // removes the socket file
            }
            Err(_) => {
                assert!(
                    result.is_err(),
                    "bare filename in an unvetted cwd must be refused, not bound"
                );
                assert!(
                    !cwd.join(&name).exists(),
                    "no socket may be created in an unvetted cwd"
                );
            }
        }
    }

    /// Over the kernel's limit, `bind` alone would say only
    /// `InvalidInput: path must be shorter than SUN_LEN`. The named error has
    /// to survive the trip through `io::Error`.
    #[test]
    fn bind_at_refuses_an_over_long_socket_path() {
        let dir = private_temp_dir("taria-longpath-");
        let path = dir.path().join(format!("{}.sock", "x".repeat(120)));

        let Err(err) = TariaLayer::bind_at("toolong", &path) else {
            panic!("over-long path must be refused, not bound");
        };
        let message = err.to_string();
        assert!(message.contains("107 byte"), "err: {message}");
        assert!(message.contains("TARIA_SOCK"), "err: {message}");
        assert!(!path.exists(), "no socket file may be left behind");
    }

    /// The whole point of a disabled layer: every method still answers, and
    /// none of them does anything.
    #[test]
    fn disabled_layer_is_inert() {
        let dir = private_temp_dir("taria-disabled-");
        // A regular file where the socket's parent directory should be: the
        // bind cannot succeed, and nothing about that is timing-dependent.
        let blocker = dir.path().join("not-a-directory");
        fs::write(&blocker, b"").expect("create blocker file");
        let socket_path = blocker.join("app.sock");

        let mut layer = TariaLayer::bind_or_disabled_at("disabled", socket_path.clone());

        assert!(!layer.is_enabled());
        assert!(layer.bind_error().is_some(), "a disabled layer says why");
        assert_eq!(layer.socket_path(), socket_path);
        assert_eq!(layer.app_label(), "disabled");
        assert_eq!(layer.dropped_inputs(), 0);

        assert_eq!(layer.try_recv(), None);
        assert_eq!(layer.try_recv_with_id(), None);
        assert_eq!(layer.recv_timeout(Duration::from_millis(1)), None);
        assert_eq!(layer.recv_timeout_with_id(Duration::from_millis(1)), None);
        layer.ack(0, InputStatus::Ignored);

        let mut drained = 0;
        layer.drain(|_| drained += 1);
        layer.drain_with_ids(|_, _| drained += 1);
        assert_eq!(drained, 0, "a disabled layer has nothing to drain");

        // The render path works unchanged; it just publishes nowhere.
        let mut rec = layer.frame();
        rec.push(Node::new("pane", Role::Pane));
        rec.publish();
        layer.publish([Node::new("pane", Role::Pane)]);
        assert_eq!(layer.latest_snapshot(), None);

        drop(layer);
        assert!(!socket_path.exists(), "a disabled layer creates no socket");
        assert!(blocker.exists(), "dropping it must unlink nothing");
    }

    #[test]
    fn publish_wraps_nodes_like_the_recorder_does() {
        let mut layer = bind_test_layer("taria-layer-", "publish");
        layer.publish([
            Node::new("a", Role::Text),
            Node::new("b", Role::Button).focused(true),
        ]);

        let root = layer.latest_snapshot().expect("published").root;
        assert_eq!(root.id.0, "app");
        assert_eq!(root.label.as_deref(), Some("publish"));
        assert!(!root.focused, "a focused node unfocuses the auto root");
        let ids: Vec<&str> = root.children.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn focus_detection_is_recursive() {
        let unfocused = Node::new("a", Role::Pane).child(Node::new("b", Role::Text));
        assert!(!subtree_has_focus(&unfocused));

        let deep_focus = Node::new("a", Role::Pane)
            .child(Node::new("b", Role::List).child(Node::new("c", Role::ListItem).focused(true)));
        assert!(subtree_has_focus(&deep_focus));
    }
}
