//! The [`TariaLayer`]: socket lifecycle, background threads, and snapshot
//! publishing for a ratatui app.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use taria::wire::{AppToBridge, BridgeToApp};
use taria::{AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};

use crate::FrameRecorder;

/// How long a snapshot write may stall on a slow or stuck bridge before the
/// connection is dropped. Protects the app from a peer that stops reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The embeddable taria endpoint for a ratatui app.
///
/// Binding a layer opens a Unix domain socket and spawns a listener thread
/// that serves one bridge client at a time: on connect it sends a
/// [`AppToBridge::Hello`] handshake plus the latest snapshot, then streams
/// every newly published snapshot and forwards incoming [`AgentInput`]
/// messages to the app via [`try_recv`](TariaLayer::try_recv) /
/// [`recv_timeout`](TariaLayer::recv_timeout).
///
/// Dropping the layer shuts the threads down and removes the socket file
/// (best-effort).
pub struct TariaLayer {
    socket_path: PathBuf,
    shared: Arc<Shared>,
    input_rx: Receiver<AgentInput>,
    listener: Option<JoinHandle<()>>,
    seq: u64,
    last_root: Option<Node>,
}

impl TariaLayer {
    /// Bind the taria socket for this app and start serving.
    ///
    /// The socket path is resolved in order of preference:
    ///
    /// 1. `$TARIA_SOCK` verbatim, if set and non-empty;
    /// 2. `$XDG_RUNTIME_DIR/taria/<app_label>.sock`;
    /// 3. `<temp_dir>/taria-<uid or user>/<app_label>.sock`.
    ///
    /// The parent directory is created (mode `0700`, best-effort) and a stale
    /// socket file at the path is removed before binding.
    pub fn bind(app_label: &str) -> io::Result<Self> {
        Self::bind_at(app_label, resolve_socket_path(app_label))
    }

    /// Like [`bind`](Self::bind), but at an explicit socket path, skipping
    /// resolution. Useful for tests and apps that manage their own runtime
    /// directories.
    pub fn bind_at(app_label: &str, socket_path: impl Into<PathBuf>) -> io::Result<Self> {
        let socket_path = socket_path.into();

        if let Some(parent) = socket_path.parent() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            builder.mode(0o700);
            // Best-effort: an existing directory keeps its mode, and a real
            // failure surfaces as a bind error below.
            let _ = builder.create(parent);
        }
        // Remove a stale socket left by a previous run; if this fails for any
        // reason other than the file being absent, bind reports the real error.
        let _ = fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        let shared = Arc::new(Shared {
            app_label: app_label.to_string(),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
        });
        let (input_tx, input_rx) = mpsc::channel();

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
            socket_path,
            shared,
            input_rx,
            listener: Some(handle),
            seq: 0,
            last_root: None,
        })
    }

    /// The path of the Unix socket this layer is serving on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The label passed to [`bind`](Self::bind), used in the handshake and as
    /// the auto-generated root node's label.
    pub fn app_label(&self) -> &str {
        &self.shared.app_label
    }

    /// Begin recording a new frame. Record nodes with
    /// [`FrameRecorder::push`] or [`sem`](crate::sem), then call
    /// [`FrameRecorder::publish`].
    pub fn frame(&mut self) -> FrameRecorder<'_> {
        FrameRecorder::new(self)
    }

    /// Non-blocking poll for the next agent input, if one has arrived.
    pub fn try_recv(&self) -> Option<AgentInput> {
        self.input_rx.try_recv().ok()
    }

    /// Wait up to `timeout` for the next agent input.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AgentInput> {
        self.input_rx.recv_timeout(timeout).ok()
    }

    /// Publish a frame's recorded top-level nodes as a new snapshot.
    ///
    /// Wraps the nodes in an auto-generated `app` root (focused only if no
    /// recorded node is), bumps `seq`, and hands the snapshot to the writer
    /// thread. Skipped entirely when the tree is identical to the previous
    /// publish. Never blocks the render path beyond a brief mutex hold.
    pub(crate) fn publish_nodes(&mut self, nodes: Vec<Node>) {
        let focus_recorded = nodes.iter().any(subtree_has_focus);
        let root = Node::new("app", Role::App)
            .label(self.shared.app_label.clone())
            .focused(!focus_recorded)
            .children(nodes);

        if self.last_root.as_ref() == Some(&root) {
            return;
        }
        self.seq += 1;
        let snapshot = Snapshot::new(self.seq, root.clone());
        self.last_root = Some(root);

        let mut state = self.shared.lock_state();
        state.latest = Some(snapshot);
        state.epoch += 1;
        drop(state);
        self.shared.cv.notify_all();
    }

    /// The most recently published snapshot, if any. Test-only introspection.
    #[cfg(test)]
    pub(crate) fn latest_snapshot(&self) -> Option<Snapshot> {
        self.shared.lock_state().latest.clone()
    }
}

impl Drop for TariaLayer {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock_state();
            state.shutdown = true;
            self.shared.cv.notify_all();
        }
        // Wake the listener thread if it is blocked in accept(); the dummy
        // connection is never served because the shutdown flag is checked
        // right after accept returns.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(handle) = self.listener.take() {
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
}

#[derive(Default)]
struct State {
    /// Latest published snapshot; sent to newly connected clients and, via
    /// `epoch` bumps, streamed to the current client.
    latest: Option<Snapshot>,
    /// Bumped on every publish so the writer knows something new exists.
    epoch: u64,
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
}

/// Does `node` or any of its descendants have focus?
fn subtree_has_focus(node: &Node) -> bool {
    node.focused || node.children.iter().any(subtree_has_focus)
}

/// Accept loop: serves one bridge client at a time until shutdown.
fn accept_loop(listener: UnixListener, shared: Arc<Shared>, input_tx: Sender<AgentInput>) {
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
    input_tx: &Sender<AgentInput>,
) -> io::Result<()> {
    // SO_SNDTIMEO is shared across the duplicated fds; only writes block long
    // enough to need it.
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut write_stream = stream.try_clone()?;

    // Capture the snapshot and epoch atomically so the writer loop neither
    // misses nor duplicates a publish that races the handshake.
    let (initial_snapshot, initial_epoch) = {
        let state = shared.lock_state();
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
    Ok(())
}

/// Reader half of a connection: parse `BridgeToApp` lines and forward agent
/// inputs to the app. Malformed lines are ignored.
fn reader_loop(
    stream: UnixStream,
    input_tx: Sender<AgentInput>,
    alive: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<BridgeToApp>(&line) else {
            continue;
        };
        let BridgeToApp::Input(input) = msg;
        if input_tx.send(input).is_err() {
            break;
        }
    }
    alive.store(false, Ordering::SeqCst);
    // Notify while holding the lock so the writer cannot miss the wakeup
    // between checking `alive` and parking on the condvar.
    let guard = shared.lock_state();
    shared.cv.notify_all();
    drop(guard);
}

/// Writer half of a connection: send each newly published snapshot as a line.
/// Returns when the layer shuts down, the reader reports the client gone, or
/// a write fails.
fn writer_loop(stream: &mut UnixStream, shared: &Shared, alive: &AtomicBool, mut last_epoch: u64) {
    loop {
        let snapshot = {
            let mut state = shared.lock_state();
            loop {
                if state.shutdown || !alive.load(Ordering::SeqCst) {
                    return;
                }
                if state.epoch != last_epoch {
                    last_epoch = state.epoch;
                    break state.latest.clone();
                }
                state = shared
                    .cv
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
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

/// Resolve the default socket path for `app_label` from the environment.
fn resolve_socket_path(app_label: &str) -> PathBuf {
    resolve_socket_path_from(
        env::var_os("TARIA_SOCK"),
        env::var_os("XDG_RUNTIME_DIR"),
        &env::temp_dir(),
        &user_identity(),
        app_label,
    )
}

/// Pure resolution logic, split out so it can be tested without touching the
/// process environment.
fn resolve_socket_path_from(
    taria_sock: Option<OsString>,
    xdg_runtime_dir: Option<OsString>,
    temp_dir: &Path,
    user: &str,
    app_label: &str,
) -> PathBuf {
    if let Some(path) = taria_sock
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    if let Some(dir) = xdg_runtime_dir
        && !dir.is_empty()
    {
        return PathBuf::from(dir)
            .join("taria")
            .join(format!("{app_label}.sock"));
    }
    temp_dir
        .join(format!("taria-{user}"))
        .join(format!("{app_label}.sock"))
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
    use super::*;

    #[test]
    fn taria_sock_env_wins() {
        let path = resolve_socket_path_from(
            Some("/custom/app.sock".into()),
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
        assert_eq!(path, PathBuf::from("/custom/app.sock"));
    }

    #[test]
    fn empty_taria_sock_is_ignored() {
        let path = resolve_socket_path_from(
            Some("".into()),
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
        assert_eq!(path, PathBuf::from("/run/user/1000/taria/demo.sock"));
    }

    #[test]
    fn xdg_runtime_dir_is_second_choice() {
        let path = resolve_socket_path_from(
            None,
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
        assert_eq!(path, PathBuf::from("/run/user/1000/taria/demo.sock"));
    }

    #[test]
    fn temp_dir_is_last_resort() {
        let path = resolve_socket_path_from(None, None, Path::new("/tmp"), "1000", "demo");
        assert_eq!(path, PathBuf::from("/tmp/taria-1000/demo.sock"));
    }

    #[test]
    fn empty_xdg_falls_through_to_temp_dir() {
        let path =
            resolve_socket_path_from(None, Some("".into()), Path::new("/tmp"), "alice", "demo");
        assert_eq!(path, PathBuf::from("/tmp/taria-alice/demo.sock"));
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
