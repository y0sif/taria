//! The [`TariaLayer`]: socket lifecycle, background threads, snapshot
//! publishing, and input acknowledgement for a ratatui app.

use std::collections::VecDeque;
use std::env;
use std::io::{self, BufRead, BufReader, Read};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus, MAX_LINE_BYTES};
use taria::{AgentInput, MAX_NODE_DEPTH, Node, PROTOCOL_VERSION, Role, Snapshot, TreeTooDeep};

use crate::FrameRecorder;
use crate::bound::{self, ROOT_BUDGET, SnapshotTooLarge};
use crate::transport::{self, Accepted, Connection, Listener, Reader, Writer};

/// How long the writer may spend on any one line before the connection is
/// dropped. Protects the app from a peer that stops reading, or reads too
/// slowly to take a whole line in that time.
///
/// The bound is on the line, not on each wait for room in the socket, which
/// is all a socket's own write timeout bounds (see
/// [`Writer::write_all_before`]).
/// Bounded that way, a line far larger than the socket buffers could stall
/// for one timeout per wait: a bridge that had stopped reading held a 900 KB
/// snapshot for two timeouts, and one taking 64 KB every 3 s, which freed
/// room before any wait timed out, held it for forty seconds. [`Drop`] waits
/// for the writer, so either held the app's exit too. Now a bridge that
/// cannot take a whole line, a snapshot of up to [`MAX_LINE_BYTES`] included,
/// within this time is dropped, and reconnects to be sent the latest snapshot
/// afresh.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the writer may spend, in total, on the acks still queued when the
/// layer shuts down.
///
/// A common exit drains input, answers it and drops the layer in one frame:
/// a `q` acted on as quit, a `drain` that returns
/// [`Ignored`](InputStatus::Ignored) on the way out. Nothing gives the writer
/// a turn between those calls, so every one of those answers can still be
/// queued, the [`Delivered`](InputStatus::Delivered) of the dequeue included,
/// and closing the connection without them leaves the agent waiting out a
/// timeout on an input the app did take. Measured before this flush existed,
/// such an app got no ack out at all in 16 runs of 20. So they are written
/// before the connection closes.
///
/// Bounded, because [`Drop`] waits for the writer and an exit must not hang on
/// a bridge that stopped reading; [`WRITE_TIMEOUT`] is far too long for that.
/// The bound is total, not per write, and a peer that reads too slowly loses
/// the acks that miss it, which costs the same timeout the flush exists to
/// spare it. A snapshot still waiting is not written at all: it describes a UI
/// that is about to be gone, and it can be large enough to spend the whole
/// budget on its own.
const SHUTDOWN_FLUSH: Duration = Duration::from_millis(250);

/// Capacity of the agent-input queue between the socket thread and the app.
/// When the app is not draining inputs, the newest input is dropped, counted
/// (see [`LayerStats::dropped_inputs`]) and acked
/// [`Dropped`](InputStatus::Dropped), instead of blocking the socket thread
/// or queueing without bound.
const INPUT_QUEUE: usize = 256;

/// How many acks may wait for the writer before the oldest is dropped.
///
/// Every other queue in the layer is bounded already: inputs stop at
/// [`INPUT_QUEUE`], snapshots coalesce into a single latest. An ack queue that
/// is not would let a bridge which reads steadily but far slower than an agent
/// sends input, never slowly enough for [`WRITE_TIMEOUT`] to fire, grow the
/// app's memory until the process is killed.
///
/// The *oldest* ack is the one dropped, the opposite end from the input queue:
/// an ack answers one specific input, and the answers an agent is still
/// waiting on are the newest, while the oldest name inputs whose waiter has
/// long since timed out. Drops are counted (see [`LayerStats::dropped_acks`]).
const ACK_QUEUE: usize = 1024;

/// How many delivered inputs stay answerable with [`TariaLayer::ack`].
///
/// An id is remembered from the moment the app dequeues it until its
/// connection ends, or until this many further inputs have been delivered on
/// that same connection. Remembering them is what lets [`TariaLayer::ack`]
/// tell an id the live bridge is waiting on from one an earlier bridge sent
/// (ids restart with each bridge process). Forgetting one only loses a late
/// ack, which the peer already has to survive; remembering them without bound
/// would be a leak in an app that runs for days.
const ACKABLE_INPUTS: usize = INPUT_QUEUE;

/// The embeddable taria endpoint for a ratatui app.
///
/// Binding a layer opens the app's endpoint, a Unix domain socket on Linux and
/// macOS or a named pipe on Windows, and spawns a listener thread that serves
/// one bridge client at a time: on connect it sends an
/// [`AppToBridge::Hello`] handshake plus the latest snapshot, then streams
/// every newly published snapshot and forwards incoming [`AgentInput`]
/// messages to the app via [`try_recv`](Self::try_recv) /
/// [`recv_timeout`](Self::recv_timeout). Every forwarded input is answered
/// with an [`AppToBridge::Ack`], so an agent can tell an input the app acted
/// on from one it never saw. An input belongs to the connection it arrived
/// on: one whose bridge disconnected before the app dequeued it is discarded
/// rather than applied, because it was aimed at a session that is gone, and
/// so is one a client wrote while it waited its turn, before the handshake
/// that opens its session (see [`LayerStats::stale_inputs`]).
///
/// A layer may also be *disabled*: [`bind_or_disabled`](Self::bind_or_disabled)
/// hands back an inert layer rather than an error, so taria failing to bind
/// can never stop an app from starting. Every method stays callable on a
/// disabled layer, which is what lets an app keep one code path.
///
/// Dropping the layer shuts the threads down and removes the socket file, but
/// only while that file is still the one this layer bound: a second instance
/// that took the path over is serving a socket of its own there, and unlinking
/// it would leave that instance running and unreachable. A pipe leaves no
/// file: on Windows the layer closes its end, and the pipe and its name go
/// once no bridge holds the other end either.
pub struct TariaLayer {
    app_label: String,
    socket_path: PathBuf,
    /// Everything that exists only once the socket is bound. `None` on a
    /// disabled layer, so no method can reach machinery that is not running.
    inner: Option<Inner>,
    /// Why binding failed, on a disabled layer.
    bind_error: Option<io::Error>,
}

/// What a [`TariaLayer`] has lost or cut so far, from
/// [`TariaLayer::stats`].
///
/// The layer never prints, because a print while the app owns the alternate
/// screen garbles the display. These counts are the app-facing tally instead.
/// Every count is monotonic for the lifetime of the layer and accumulates
/// across reconnects. On a disabled layer every count is 0 and both `last_*`
/// fields are `None`.
///
/// Read it after the terminal is restored, never while the app owns the
/// alternate screen, and report what is nonzero:
///
/// ```no_run
/// # let layer = taria_ratatui::TariaLayer::bind_or_disabled("my-app");
/// // After `ratatui::restore()`.
/// let stats = layer.stats();
/// if stats.dropped_inputs > 0 {
///     eprintln!("dropped {} agent input(s)", stats.dropped_inputs);
/// }
/// ```
///
/// The struct is `#[non_exhaustive]`, so a later release can add a count
/// without breaking an app that reads these. Outside this crate it cannot be
/// built with a struct literal; [`LayerStats::default`] is the all-zero one.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LayerStats {
    /// Agent inputs dropped because the input queue was full.
    ///
    /// When an agent floods inputs faster than the app drains them, the layer
    /// drops the newest input rather than blocking the socket thread. The
    /// agent learns of each drop from its [`Dropped`](InputStatus::Dropped)
    /// ack; this is the app-facing tally of the same event.
    pub dropped_inputs: u64,

    /// Agent inputs discarded, rather than handed to the app, because they
    /// were not aimed at the session that was live when they were read.
    ///
    /// Two things put an input here. The first is a bridge connection that
    /// ended before the app dequeued what arrived on it. Such an input was
    /// aimed at a bridge session that no longer exists: applying it would act
    /// on an agent's intent minutes or milliseconds after the agent that
    /// formed it is gone, with nobody left to receive the result. A `key q`
    /// that arrives just before the bridge restarts would otherwise quit the
    /// app on the next frame. The bridge drops its own queue on reconnect for
    /// the mirror-image reason, so both sides agree that an input belongs to
    /// one connection.
    ///
    /// The second is input a client wrote while it waited behind another.
    /// Only one bridge is served at a time, so a client that connects while
    /// another is attached waits in the listen backlog, and whatever it writes
    /// meanwhile sits unread in the kernel's socket buffer until its turn
    /// comes. That client has seen no tree, so its inputs name nodes and keys
    /// it guessed at, and applying them once the first client leaves would
    /// act on a UI it never looked at. So a connection that was already
    /// waiting when the previous session ended has what it wrote read off and
    /// thrown away just before its hello goes out, and every complete line of
    /// that which is an input is counted here, a kind this build cannot read
    /// included. None of it is acked: the session it would be answered on had
    /// not begun. Clients waiting back to back are each treated so.
    ///
    /// A client that waited behind nobody keeps what it wrote before reading
    /// its hello: the first connection of a process, and one that connects
    /// after the listener last found nobody waiting. A script that connects
    /// to an idle app and writes at once is such a client, and its input is
    /// its session's, as it was in 0.3.0. A client that waits for the hello
    /// before writing is never affected either way, and taria-mcp is one,
    /// whose input tools refuse to send until the app's first snapshot has
    /// arrived.
    pub stale_inputs: u64,

    /// Agent inputs answered without reaching the app because their `kind`
    /// is one this build cannot read.
    ///
    /// An input like that parses as [`AgentInput::Unknown`], which carries
    /// neither the kind it arrived under nor what it asked for, so there is
    /// nothing for the app to apply. The layer acks it
    /// [`Ignored`](InputStatus::Ignored) itself and never queues it; see
    /// [`TariaLayer::try_recv`], which is why an app never has to think about
    /// the variant.
    ///
    /// A nonzero count means the bridge on the other end is built against a
    /// newer taria than this app, and the agent is asking for things this app
    /// cannot do yet. Raising the app's taria dependency is the fix.
    pub unknown_inputs: u64,

    /// Acks dropped because more answers were waiting for the bridge than the
    /// queue holds.
    ///
    /// Acks queue for the writer thread, and a bridge that reads steadily but
    /// far slower than an agent sends input never stalls long enough for the
    /// write timeout to disconnect it. The queue is bounded so the app cannot
    /// be grown out of memory by such a peer; the oldest answers go first,
    /// because the ones an agent is still waiting on are the newest.
    ///
    /// A nonzero count means some agent requests were answered nowhere and
    /// their callers waited out a timeout instead. The slow end is the bridge,
    /// not the app.
    pub dropped_acks: u64,

    /// Published snapshots that had a branch cut for being deeper than
    /// [`taria::MAX_NODE_DEPTH`].
    ///
    /// It counts snapshots, not frames. A frame whose tree, once cut, is the
    /// snapshot already published is not sent again and is not counted
    /// again, so a deep tree that sits still while the app redraws it sixty
    /// times a second counts once, and a deep tree that changes counts once
    /// per change. The number says how many trees the agent was handed cut,
    /// which is the thing worth knowing, rather than how often the app drew
    /// one.
    ///
    /// See [`TariaLayer::publish`] for what the cut does and why.
    /// [`last_truncation`](Self::last_truncation) names the branch.
    pub truncated_snapshots: u64,

    /// The depth failure behind the most recent truncation, or `None` if no
    /// published tree has been cut.
    ///
    /// [`TreeTooDeep`] carries the depth measured and the id of a node found
    /// at it. An app whose tree is generated from data has no other way to
    /// tell which branch ran away, which is the whole reason the count alone
    /// is not enough here.
    pub last_truncation: Option<TreeTooDeep>,

    /// Published snapshots that had something cut to fit in one line of
    /// [`taria::wire::MAX_LINE_BYTES`].
    ///
    /// Counted like [`truncated_snapshots`](Self::truncated_snapshots):
    /// snapshots, not frames, so a tree that stays too large while the app
    /// redraws it counts once, and one that changes counts once per change.
    ///
    /// See [`TariaLayer::publish`] for what is cut and why.
    /// [`last_oversize`](Self::last_oversize) says how large the line would
    /// have been and where the bytes were.
    pub oversized_snapshots: u64,

    /// What was cut from the most recent oversized snapshot, or `None` if no
    /// published tree has had to be cut to fit.
    ///
    /// [`SnapshotTooLarge`] carries the size the line would have had and the
    /// id of the node holding the longest string. A tree that runs over has
    /// usually run away in one place, a draft, a log, a list holding every
    /// row, and the count alone cannot say which.
    pub last_oversize: Option<SnapshotTooLarge>,
}

/// The parts of a layer that exist only while its socket is bound.
struct Inner {
    shared: Arc<Shared>,
    input_rx: Receiver<QueuedInput>,
    listener: Option<JoinHandle<()>>,
    seq: u64,
    /// The root of the newest snapshot published.
    last_root: Option<Node>,
    /// The tree the newest frame built, as assembled and before anything was
    /// cut from it, when a cut made it differ from `last_root`. `None` when
    /// that frame's tree is `last_root` itself, so a tree that fits is held
    /// once rather than twice, and the next frame is compared against
    /// `last_root` instead.
    ///
    /// Kept for a frame that was not published too: a tree that changed only
    /// past a cut is cut to the snapshot already sent, and remembering it is
    /// what lets the next frame, drawn unchanged, skip the cut.
    last_raw: Option<Node>,
    /// Snapshots published with a branch cut for depth, and the failure
    /// behind the newest cut. Owned here rather than in [`Shared`] because
    /// only the app thread publishes and only the app thread reads these.
    truncated: u64,
    last_truncation: Option<TreeTooDeep>,
    /// Snapshots published with something cut to fit one line, and what was
    /// cut from the newest of them. Owned here for the same reason.
    oversized: u64,
    last_oversize: Option<SnapshotTooLarge>,
}

impl Inner {
    /// Is `root` the tree the newest frame built, as assembled?
    ///
    /// Compared against what that frame kept: the tree it was cut from, when
    /// it had to be cut, or else the snapshot, which is then the same tree.
    fn is_newest_frame(&mut self, root: &mut Node) -> bool {
        match self.last_raw.as_mut().or(self.last_root.as_mut()) {
            Some(newest) => same_tree(root, newest),
            None => false,
        }
    }
}

impl Drop for Inner {
    /// Free the trees without recursing: `last_raw` is the tree as built,
    /// which can be deeper than the call stack.
    fn drop(&mut self) {
        for tree in [self.last_raw.take(), self.last_root.take()]
            .into_iter()
            .flatten()
        {
            drop_iteratively(tree);
        }
    }
}

/// One agent input on its way from a connection's reader thread to the app.
///
/// The generation tags the connection it arrived on, so an input queued by a
/// bridge that has since disconnected can be told apart from one the live
/// bridge is waiting on.
struct QueuedInput {
    generation: u64,
    id: InputId,
    input: AgentInput,
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
    /// redirect the socket.
    ///
    /// A socket file left at the path by a run that never got to clean up is
    /// removed before binding. Nothing else is: a file that is not a socket
    /// stays where it is, and so does a socket another instance is still
    /// serving. Both refuse the bind instead, with an error saying which it
    /// was; see [`bind_at`](Self::bind_at).
    ///
    /// The label becomes the socket's file name, so it must be one: a label
    /// carrying a path separator, or `.` or `..`, is refused here rather than
    /// resolved into a path this layer would go on to bind and unlink. An app
    /// that needs a path of its own passes it to
    /// [`bind_at`](Self::bind_at).
    ///
    /// Apps that would rather run without taria than not run at all should
    /// use [`bind_or_disabled`](Self::bind_or_disabled).
    ///
    /// # On Windows
    ///
    /// The endpoint is a named pipe, and its name is resolved by
    /// [`taria::socket::resolve_pipe_name`]: `$TARIA_SOCK` if set and
    /// non-empty, which must then be a pipe on this machine
    /// (`\\.\pipe\<name>`) or the bind is refused, else
    /// `\\.\pipe\taria-<SID>-<app_label>`, with the SID of the user the
    /// process runs as, read from its token. The label may not contain `\`
    /// either. There is no directory to vet and no file to clear: the pipe is
    /// created as the first and only instance of its name, refuses clients on
    /// other machines, and is owned by the current user with a DACL that lets
    /// only that user in. A name another instance is serving, or that
    /// something else holds, is refused with
    /// [`AddrInUse`](io::ErrorKind::AddrInUse), as on Unix.
    pub fn bind(app_label: &str) -> io::Result<Self> {
        let socket_path = resolve_socket_path(app_label).map_err(invalid_label)?;
        Self::bind_at(app_label, socket_path)
    }

    /// Like [`bind`](Self::bind), but at an explicit socket path, skipping
    /// resolution. Useful for tests and apps that manage their own runtime
    /// directories. The same parent-directory creation and privacy checks as
    /// [`bind`](Self::bind) apply: a relative path (including a bare
    /// filename) is resolved against the current directory first, so the
    /// parent directory that would hold the socket is always vetted.
    ///
    /// Apps that would rather run without taria than not run at all should
    /// use [`bind_or_disabled_at`](Self::bind_or_disabled_at).
    ///
    /// # What is already at the path
    ///
    /// Binding needs the path to be free, and only a dead socket is cleared
    /// to make it so:
    ///
    /// * nothing there: bound;
    /// * a socket nothing is listening on, left by a run that was killed
    ///   before it could clean up: unlinked, then bound;
    /// * a socket another instance is serving: refused with
    ///   [`AddrInUse`](io::ErrorKind::AddrInUse), because taking it over
    ///   would leave that instance running with no way for a bridge to reach
    ///   it. Run the second instance on a socket of its own instead;
    /// * anything that is not a socket: refused with
    ///   [`AlreadyExists`](io::ErrorKind::AlreadyExists), the file untouched.
    ///   The path can come verbatim from `$TARIA_SOCK`, and a typo there is
    ///   not a reason to delete a file the user meant to keep.
    ///
    /// # On Windows
    ///
    /// `socket_path` is a pipe name, and must be a pipe on this machine,
    /// `\\.\pipe\<name>` in any letter case with one name after the prefix,
    /// as [`taria::socket::check_pipe_name`] holds it; anything else, a file
    /// path or a pipe on another machine included, is refused with
    /// [`InvalidInput`](io::ErrorKind::InvalidInput) before anything is
    /// opened. Nothing is ever cleared from a name: one another instance is
    /// serving, or that something else holds, is refused with
    /// [`AddrInUse`](io::ErrorKind::AddrInUse). See [`bind`](Self::bind) for
    /// how the pipe is protected.
    pub fn bind_at(app_label: &str, socket_path: impl Into<PathBuf>) -> io::Result<Self> {
        // The transport vets the path, frees it of a dead socket and binds
        // there, or says why it will not. It cleans up after itself when
        // dropped, which is why a failure below needs no unlink here: the
        // listener goes down with whatever failed to take it.
        let listener = Listener::bind(socket_path.into())?;
        let socket_path = listener.path().to_path_buf();
        let shared = Arc::new(Shared {
            app_label: app_label.to_string(),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            dropped_inputs: AtomicU64::new(0),
            stale_inputs: AtomicU64::new(0),
            unknown_inputs: AtomicU64::new(0),
            dropped_acks: AtomicU64::new(0),
        });
        let (input_tx, input_rx) = mpsc::sync_channel(INPUT_QUEUE);

        let thread_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("taria-listener".into())
            .spawn(move || accept_loop(listener, thread_shared, input_tx))?;

        Ok(Self {
            app_label: app_label.to_string(),
            socket_path,
            inner: Some(Inner {
                shared,
                input_rx,
                listener: Some(handle),
                seq: 0,
                last_root: None,
                last_raw: None,
                truncated: 0,
                last_truncation: None,
                oversized: 0,
                last_oversize: None,
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
        match resolve_socket_path(app_label) {
            Ok(socket_path) => Self::bind_or_disabled_at(app_label, socket_path),
            // A refused label resolves to no path at all, so the disabled
            // layer carries an empty one: the error names the label, which is
            // the thing that has to change, and an invented path would only
            // look like somewhere the socket might be.
            Err(err) => Self::disabled(app_label, PathBuf::new(), invalid_label(err)),
        }
    }

    /// Bind like [`bind_at`](Self::bind_at), or return a disabled layer when
    /// that fails: [`bind_or_disabled`](Self::bind_or_disabled) at an
    /// explicit socket path, skipping resolution.
    ///
    /// For an app that manages its own runtime directory and would still
    /// rather run without taria than not run at all. A disabled layer keeps
    /// the path it tried in [`socket_path`](Self::socket_path) and why binding
    /// failed in [`bind_error`](Self::bind_error), and prints nothing, for the
    /// reason [`bind_or_disabled`](Self::bind_or_disabled) gives.
    pub fn bind_or_disabled_at(app_label: &str, socket_path: impl Into<PathBuf>) -> Self {
        let socket_path = socket_path.into();
        match Self::bind_at(app_label, socket_path.clone()) {
            Ok(layer) => layer,
            Err(err) => Self::disabled(app_label, socket_path, err),
        }
    }

    /// An inert layer that answers every method and serves nothing.
    fn disabled(app_label: &str, socket_path: PathBuf, err: io::Error) -> Self {
        Self {
            app_label: app_label.to_string(),
            socket_path,
            inner: None,
            bind_error: Some(err),
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

    /// The endpoint this layer is serving on, or the one it tried to bind if
    /// it is disabled, so an app can print either: the Unix socket's path on
    /// Linux and macOS, the named pipe's name (`\\.\pipe\...`) on Windows.
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

    /// Non-blocking poll for the next agent input, if one has arrived, with
    /// the [`InputId`] that answers it again through [`ack`](Self::ack).
    ///
    /// Acks the input [`Delivered`](InputStatus::Delivered) on its way out,
    /// so an app can never leave an input unacknowledged by forgetting to
    /// call something. Delivered means the app's event loop dequeued it,
    /// nothing more; an app that then decides to do nothing with it should
    /// say so with [`ack`](Self::ack).
    ///
    /// Reach for [`drain`](Self::drain) first: it loops this and sends those
    /// acks itself. Loop this by hand when the app needs the id for something
    /// else, such as remembering which input started a job:
    ///
    /// ```no_run
    /// # use taria_ratatui::{InputId, InputStatus, TariaLayer};
    /// # enum Outcome { Applied, Refused, Started }
    /// # fn apply(_input: taria_ratatui::taria::AgentInput) -> Outcome {
    /// #     Outcome::Applied
    /// # }
    /// # let layer = TariaLayer::bind_or_disabled("my-app");
    /// let mut jobs: Vec<InputId> = Vec::new();
    /// while let Some((id, input)) = layer.try_recv() {
    ///     match apply(input) {
    ///         Outcome::Applied => {}
    ///         // Now, before the frame that follows publishes: the layer
    ///         // sends every queued ack ahead of the next snapshot.
    ///         Outcome::Refused => layer.ack(id, InputStatus::Ignored),
    ///         // Under way: the next frame publishes a node saying so, and
    ///         // the job's result lands in a later one.
    ///         Outcome::Started => jobs.push(id),
    ///     }
    /// }
    /// ```
    ///
    /// An [`Ignored`](InputStatus::Ignored) ack belongs before the first
    /// snapshot the app publishes after dequeuing the input. A bridge that
    /// reads stamps (section 5.3 of `docs/protocol.md`) answers on that
    /// snapshot, so an `Ignored` sent a frame later arrives after the agent
    /// has been told the input changed nothing. The layer writes queued acks
    /// ahead of the next snapshot, so an ack sent before the frame that
    /// follows the dequeue is always in time. An app that can only decide
    /// later should not hold the ack back: it publishes a status node saying
    /// the input is under way, and the tree carries the outcome when it comes.
    ///
    /// Inputs left over from a bridge connection that has since ended are
    /// discarded here rather than handed over, and input a client wrote before
    /// its handshake never reaches the queue at all; see
    /// [`LayerStats::stale_inputs`].
    ///
    /// [`AgentInput::Unknown`] never arrives here either. An input whose
    /// `kind` this build cannot read carries nothing to apply, so the layer
    /// acks it [`Ignored`](InputStatus::Ignored) where it is parsed and never
    /// queues it; see [`LayerStats::unknown_inputs`].
    ///
    /// Always `None` on a disabled layer.
    pub fn try_recv(&self) -> Option<(InputId, AgentInput)> {
        let inner = self.inner.as_ref()?;
        loop {
            let queued = inner.input_rx.try_recv().ok()?;
            if inner.shared.deliver(queued.generation, queued.id) {
                return Some((queued.id, queued.input));
            }
        }
    }

    /// Wait up to `timeout` for the next agent input, with the [`InputId`]
    /// that answers it again through [`ack`](Self::ack).
    ///
    /// Acks [`Delivered`](InputStatus::Delivered) and discards stale inputs
    /// exactly like [`try_recv`](Self::try_recv); a discard consumes none of
    /// the budget, the wait resumes for what is left of `timeout`. A disabled
    /// layer has nothing to wait for but still waits out the timeout, so an
    /// app that paces its loop on this call keeps its timing whether or not
    /// taria bound.
    ///
    /// A `timeout` so large that no clock can hold the deadline (near
    /// [`Duration::MAX`]) waits for an input for as long as the queue can
    /// deliver one, which is what such a deadline asks for anyway. On a
    /// disabled layer that is no time at all: its queue can never deliver, so
    /// the call returns at once rather than parking the app forever on a wait
    /// with no end.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<(InputId, AgentInput)> {
        let Some(inner) = self.inner.as_ref() else {
            // No socket, so no input will ever arrive. Sleep the timeout out
            // anyway: a caller using this as its clock must not spin because
            // taria happened to be unavailable.
            //
            // Except for a timeout no clock can hold, which asks to wait
            // until an input arrives. A disabled layer's queue can never
            // deliver one, so that wait would have no end, and freezing
            // because taria could not bind is the very failure
            // `bind_or_disabled` exists to prevent: an app pacing its loop on
            // this call would run bound and hang unbound. Returning at once
            // is also what the enabled path does with a queue that can no
            // longer deliver, where `recv` on a channel whose senders are
            // gone returns immediately.
            if Instant::now().checked_add(timeout).is_some() {
                thread::sleep(timeout);
            }
            return None;
        };
        // Discarding a stale input must not cut the wait short: an app that
        // paces its event loop on this call would otherwise spin through the
        // rest of its budget the moment a bridge disconnects.
        //
        // `Instant + Duration` panics when the deadline is not representable,
        // and a library crate must not take the app down over an argument.
        // A deadline that far out is indistinguishable from none at all, so
        // it becomes a plain blocking wait instead.
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let queued = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    inner.input_rx.recv_timeout(remaining).ok()?
                }
                None => inner.input_rx.recv().ok()?,
            };
            if inner.shared.deliver(queued.generation, queued.id) {
                return Some((queued.id, queued.input));
            }
        }
    }

    /// Hand every queued agent input to `f`, in arrival order, and send the
    /// ack each returned status adds.
    ///
    /// The drain loop an app otherwise writes by hand, once per place it
    /// polls for input, with nothing keeping the copies in step.
    ///
    /// Return [`Ignored`](InputStatus::Ignored) for an input the app looked at
    /// and deliberately did nothing with, and the layer sends it, so no
    /// [`InputId`] passes through the app. Two independent apps wrote the
    /// same dequeue-compare-[`ack`](Self::ack) loop by hand, each with its own
    /// two-variant verdict enum in front, and this is that loop.
    ///
    /// Return [`Delivered`](InputStatus::Delivered) for an input the app
    /// acted on. It sends nothing: the dequeue already acked it, and
    /// re-sending would double this layer's ack traffic, which on a bridge
    /// reading slower than the app answers costs the refinements queued
    /// behind it (see [`LayerStats::dropped_acks`]).
    ///
    /// The statuses go out before the snapshot of the frame that follows, the
    /// order a bridge that reads stamps needs: an `Ignored` it reads after the
    /// first tree published after the input comes too late to change its
    /// answer (see [`try_recv`](Self::try_recv)).
    ///
    /// An app that needs the id for something else, such as remembering which
    /// input started a job, loops [`try_recv`](Self::try_recv) instead and
    /// calls [`ack`](Self::ack) itself, before that frame publishes.
    pub fn drain(&self, mut f: impl FnMut(AgentInput) -> InputStatus) {
        while let Some((id, input)) = self.try_recv() {
            let status = f(input);
            if status != InputStatus::Delivered {
                self.ack(id, status);
            }
        }
    }

    /// Answer an input again, overriding the ack it already has.
    ///
    /// Last ack wins. An app that dequeued an input (acked
    /// [`Delivered`](InputStatus::Delivered) by that dequeue) and then
    /// deliberately did nothing with it can refine that to
    /// [`Ignored`](InputStatus::Ignored) so an agent waiting on an effect
    /// stops waiting. That is every input kind, not only an act a modal dialog
    /// blocks or naming a node this app does not know: a `set_value` carrying
    /// no value, an [`AgentInput::Text`] sent while nothing here is accepting
    /// typing, an [`AgentInput::Key`] this app could not lower, and an input
    /// kind this build has never heard of. [`InputStatus::Ignored`] draws the
    /// line, including why a key bound to nothing is still delivered.
    ///
    /// The ack travels the same path as snapshots and goes out ahead of the
    /// next one, so the snapshot published after it can never overtake it.
    /// Send it before the app publishes its first snapshot after dequeuing
    /// the input: a bridge that reads stamps answers on that snapshot, and an
    /// `Ignored` that follows it arrives after the agent was told the input
    /// changed nothing (see [`try_recv`](Self::try_recv)). Does nothing on a
    /// disabled layer.
    ///
    /// An id whose bridge connection has since ended is answered nowhere.
    /// Ids are unique only within a bridge process and a fresh bridge counts
    /// from the start again, so an id an app kept across a disconnect can
    /// already name a live input of the next bridge's, and that bridge's
    /// waiter must not be handed a verdict from a session it never saw.
    /// Holding an id across a disconnect is not a mistake, so the drop is
    /// silent: the peer this ack was for is gone, and there is nobody left to
    /// tell. An id also stops being answerable once its connection has
    /// delivered a queue's worth of newer inputs past it, which costs at most
    /// a late ack the peer already has to survive.
    pub fn ack(&self, id: InputId, status: InputStatus) {
        if let Some(inner) = self.inner.as_ref() {
            inner.shared.queue_app_ack(id, status);
        }
    }

    /// What this layer has lost or cut so far: agent inputs it dropped,
    /// discarded or could not read, acks it could not send, and snapshots it
    /// had to cut. [`LayerStats`] says what each count means. All zero on a
    /// disabled layer.
    ///
    /// The layer keeps these because it must not print while the app owns
    /// the alternate screen. Read them after restoring the terminal (for
    /// example right before exit, once the alternate screen has been left)
    /// and report a nonzero count on stderr or in a log.
    pub fn stats(&self) -> LayerStats {
        let Some(inner) = self.inner.as_ref() else {
            return LayerStats::default();
        };
        let shared = &inner.shared;
        LayerStats {
            dropped_inputs: shared.dropped_inputs.load(Ordering::Relaxed),
            stale_inputs: shared.stale_inputs.load(Ordering::Relaxed),
            unknown_inputs: shared.unknown_inputs.load(Ordering::Relaxed),
            dropped_acks: shared.dropped_acks.load(Ordering::Relaxed),
            truncated_snapshots: inner.truncated,
            last_truncation: inner.last_truncation.clone(),
            oversized_snapshots: inner.oversized,
            last_oversize: inner.last_oversize.clone(),
        }
    }

    /// Publish `nodes` as a snapshot, without going through a
    /// [`FrameRecorder`].
    ///
    /// For apps that build their node list separately from drawing, where the
    /// recorder's frame, push-loop and publish triplet is three lines saying
    /// one thing. The nodes are wrapped in the same auto-generated root, so
    /// both paths produce the same tree.
    ///
    /// # Disabled nodes
    ///
    /// A node marked [`disabled`](Node::disabled) is published with no
    /// actions, whatever it was built with: the mark says the app takes none
    /// until it enables the node again, so an advertised action there would
    /// be a promise the app has already said it will not keep. Nothing counts
    /// this, because it is the app saying what it means, not something lost.
    /// The node keeps everything else, its focus and its children included.
    ///
    /// # Trees too deep to send
    ///
    /// A tree deeper than [`taria::MAX_NODE_DEPTH`], counting the root this
    /// method adds, is cut here: every node at the limit publishes without
    /// its children. The cut is per branch, so everything above it reaches
    /// the agent unchanged.
    ///
    /// Cutting rather than refusing, because the two alternatives are both
    /// worse than losing the deep part. Publishing the tree as built is what
    /// breaks the peer: a snapshot past the limit exceeds what a JSON parser
    /// will recurse into, the bridge skips the line exactly as it skips a
    /// truncated one, and the agent is left reading a stale tree or told no
    /// app has published yet, with nothing anywhere saying why. Skipping the
    /// publish is that same stale tree, chosen deliberately.
    ///
    /// The app is told instead of the agent, because the app is the only one
    /// that can fix it: see [`stats`](Self::stats), whose
    /// [`truncated_snapshots`](LayerStats::truncated_snapshots) is the count
    /// and [`last_truncation`](LayerStats::last_truncation) the branch. The
    /// fix is almost always to publish what the widget draws rather than the
    /// data behind it, which for a deep tree view is the expanded path and
    /// the rows currently on screen.
    ///
    /// # Trees too large to send
    ///
    /// A snapshot travels as one line, and a bridge reads a line up to
    /// [`taria::wire::MAX_LINE_BYTES`] and no further: past that it cannot
    /// find where the next message starts, so it drops the connection.
    /// Dropping it fixes nothing. The bridge reconnects, is sent the latest
    /// snapshot, which is the same one, and drops that connection too, so an
    /// app holding one long value (a draft, a log tail, a transcript) leaves
    /// the agent reconnecting for as long as the value stays long, with
    /// nothing on either side saying why.
    ///
    /// So every tree is bounded here, after the depth cut, to fit that line
    /// at any `seq`. The bound cuts what costs the most first and stops as
    /// soon as the tree fits:
    ///
    /// 1. nodes that could never fit, one whose id, role and actions, with
    ///    its strings at the floor, cannot fit beside its ancestors, are
    ///    dropped with everything under them before any string is cut, so
    ///    that they cost their siblings nothing;
    /// 2. strings are cut: the longest values and descriptions first, all
    ///    down to one shared length, so the biggest shrink first and shorter
    ///    ones are untouched, then labels with them, the same way, when those
    ///    alone are not enough. A cut value ends in `…`, except that a
    ///    [`Role::Log`] or [`Role::Terminal`] value keeps its end and starts
    ///    with `…` instead, because the end is where those grow. A string of
    ///    256 bytes of JSON or less, the floor, is never cut;
    /// 3. every node keeps at most one shared number of children, the
    ///    largest that fits, so a list of fifty thousand rows loses its last
    ///    rows while a status bar beside it keeps its place;
    /// 4. shallowest first, the children that cap removed go back while they
    ///    fit, each parent keeping a run of its first children with no gap,
    ///    apart from a node that could never fit, and the first child too
    ///    large for the room left ends its parent's run;
    /// 5. if all else fails, a bare root, which always fits.
    ///
    /// The third and fourth stages plan with every string at the floor, but
    /// the strings of the nodes they keep are then cut only as far as the
    /// kept tree needs, and not at all when it fits as it is.
    ///
    /// Ids and custom action names are never changed, so whatever an agent
    /// can read it can still act on. Focus is decided after the bound, for
    /// the reason it is decided after the depth cut: the focused node can be
    /// one of the rows dropped.
    ///
    /// A frame whose tree is the one the frame before it built stops before
    /// any of this: it is compared and nothing else, neither measured nor
    /// cut, so a tree that sits still costs the same whether or not it fits.
    /// A tree that changed is measured exactly, by serializing it into a
    /// counter that keeps nothing, and one that does not fit is cut again on
    /// every frame it changes, so the bound is a guard, not a way to publish:
    /// see [`stats`](Self::stats), whose
    /// [`oversized_snapshots`](LayerStats::oversized_snapshots) and
    /// [`last_oversize`](LayerStats::last_oversize) say what was cut and where.
    /// The fix is the one for depth, to publish what the widget draws: the
    /// lines of a log the pane shows, the rows of a list on screen, the
    /// visible part of a draft.
    ///
    /// # Stamps
    ///
    /// Every snapshot carries the newest input the app has dequeued on the
    /// live connection, through [`drain`](Self::drain),
    /// [`try_recv`](Self::try_recv) or [`recv_timeout`](Self::recv_timeout)
    /// ([`Snapshot::after_input`]). A bridge that reads the stamp credits a
    /// tree to an input only once it is stamped at that input, and answers
    /// "did not change" as soon as such a tree is unchanged, so an input the
    /// app takes a second over is still answered with its own tree.
    ///
    /// That makes one frame after a drain special: the first tree published
    /// after an input is dequeued is the one credited to it, so it should
    /// carry the effect, or a status saying it is under way. Publish after
    /// applying what you drained, not between the two. A frame drawn
    /// unchanged after a dequeue is not skipped: the snapshot already
    /// published goes out again with its stamp moved, under the same `seq`,
    /// neither measured nor cut again. An app that publishes only when its
    /// tree changes leaves a bridge waiting out its window on every input
    /// that changes nothing, so publish every frame and let the layer skip
    /// the rest.
    pub fn publish(&mut self, nodes: impl IntoIterator<Item = Node>) {
        self.publish_nodes(nodes.into_iter().collect());
    }

    /// Publish a frame's recorded top-level nodes as a new snapshot.
    ///
    /// Wraps the nodes in an auto-generated `app` root (focused only if no
    /// recorded node is), bumps `seq`, and hands the snapshot to the writer
    /// thread. Skipped when the tree is the one the previous frame built,
    /// before anything is cut or measured; when what is left once it is cut
    /// is the previous snapshot; and on a disabled layer. In the first two
    /// cases the previous snapshot is sent again, stamped, if the app has
    /// dequeued an input since it went out. Never blocks the render path
    /// beyond a brief mutex hold.
    ///
    /// A tree over [`MAX_NODE_DEPTH`] is cut before any of that, and then a
    /// tree too large for one line; see [`publish`](Self::publish).
    pub(crate) fn publish_nodes(&mut self, nodes: Vec<Node>) {
        let Some(inner) = self.inner.as_mut() else {
            // Freed without recursing even here: the tree as built can be
            // deeper than the call stack, and a disabled layer must not be
            // the one thing that cannot take it.
            nodes.into_iter().for_each(drop_iteratively);
            return;
        };
        let mut root = Node::new("app", Role::App)
            .label(inner.shared.app_label.clone())
            .children(nodes);
        // Before anything compares or measures the tree, so the tree kept to
        // compare the next frame against is the one that goes out.
        strip_disabled_actions(&mut root);
        // Focus is decided on the tree as built, so that a tree no cut
        // touches is already the snapshot it goes out as, and the next frame
        // can be compared against that snapshot with no second copy kept.
        root.focused = !focused_below(&root);
        // A frame drawn unchanged stops here, before anything is cut or
        // measured. Everything below is a function of the tree alone: the
        // root's label is the app's, fixed when the layer was bound, the
        // depth limit and the budget are constants, and focus reads nothing
        // but the tree. So the same tree would be cut to the same snapshot,
        // the one already sent, and skipping the work changes nothing but
        // what it costs, which for a tree too large for the line is the
        // whole bound on every frame the app draws. The tree as built is
        // freed without recursing, here and wherever else the layer lets go
        // of one, because the depth cut below exists for trees deeper than
        // the call stack, and the drop glue `Node` derives recurses.
        //
        // What it may still do is send the snapshot already published again,
        // as it is but for its stamp, when the app has dequeued an input
        // since that snapshot went out. See `Shared::restamp`.
        if inner.is_newest_frame(&mut root) {
            drop_iteratively(root);
            inner.shared.restamp();
            return;
        }

        // The root added here counts towards the depth, so the check has to
        // run on the assembled tree rather than on the caller's nodes. A
        // tree over the limit goes out as a copy cut to it, and the tree as
        // built is kept to compare the next frame against. Focus is decided
        // again on the copy: cutting a branch can remove the only focused
        // node, and a snapshot has to carry focus somewhere.
        let too_deep = root.check_depth().err();
        let (mut tree, mut raw) = if too_deep.is_some() {
            let mut cut = copy_within_depth(&mut root);
            cut.focused = !focused_below(&cut);
            (cut, Some(root))
        } else {
            (root, None)
        };

        // Then the size, which has to come after the depth cut: measuring
        // serializes the tree, which recurses, and the cut is what makes
        // that safe. Focus was decided before the measure so that the size
        // the bound reports is that of the tree as it would otherwise have
        // gone out, and is decided again after a cut, because a dropped row
        // can be the focused one. The tree as built is copied only when it
        // is about to be cut, so a tree that fits is never copied here.
        let before = bound::measure_root(&tree);
        let too_large = if before.is_some_and(|bytes| bytes <= ROOT_BUDGET) {
            None
        } else {
            if raw.is_none() {
                raw = Some(tree.clone());
            }
            let bounded = bound::cut_to_fit(&mut tree, ROOT_BUDGET, before);
            tree.focused = !focused_below(&tree);
            Some(bounded)
        };
        if let Some(replaced) = mem::replace(&mut inner.last_raw, raw) {
            drop_iteratively(replaced);
        }

        // Compared again once cut: a tree that changed only past a cut, a
        // draft typed into beyond what fits, is cut to the snapshot already
        // sent. Restamped like a frame drawn unchanged.
        if inner.last_root.as_ref() == Some(&tree) {
            inner.shared.restamp();
            return;
        }
        inner.seq += 1;
        // Counted here, past the dedupe, so both count snapshots the agent
        // was sent rather than frames the app drew: an over-deep or
        // oversized tree that sits still on screen would otherwise count
        // sixty times a second.
        if let Some(too_deep) = too_deep {
            inner.truncated = inner.truncated.saturating_add(1);
            inner.last_truncation = Some(too_deep);
        }
        if let Some(bounded) = too_large {
            inner.oversized = inner.oversized.saturating_add(1);
            inner.last_oversize = Some(SnapshotTooLarge::new(bounded));
        }
        let snapshot = Snapshot::new(inner.seq, tree.clone());
        inner.last_root = Some(tree);
        inner.shared.publish(snapshot);
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
    ///
    /// Acks still queued for a connected bridge are written before its
    /// connection is closed, so an app that answers an input and exits in the
    /// same frame does not leave the agent waiting on that answer; a snapshot
    /// still waiting is not. See `SHUTDOWN_FLUSH` for why the one and not the
    /// other, and for the bound on it.
    ///
    /// How long this takes depends only on what the threads are doing, never
    /// on what is at the socket path. The listener polls the shutdown flag
    /// rather than waiting to be woken by a connection, so it returns whether
    /// the socket file was replaced by a second instance, removed, or is
    /// being served by some other process. With no bridge connected that is
    /// at most one `ACCEPT_POLL` nap. With one, the line being written when
    /// the shutdown came is finished or given up within `WRITE_TIMEOUT` of
    /// starting, however large it is and however the bridge reads, no other
    /// line but the queued acks is started after it, and those get
    /// `SHUTDOWN_FLUSH`. So dropping never takes longer than `ACCEPT_POLL` +
    /// `WRITE_TIMEOUT` + `SHUTDOWN_FLUSH`, about 5.3 s, and takes longer than
    /// one `ACCEPT_POLL` only against a bridge too slow to take the line it
    /// is being sent.
    ///
    /// The trees the layer still holds are freed without recursing, because
    /// the one the newest frame built can be deeper than the call stack.
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        {
            let mut state = inner.shared.lock_state();
            state.shutdown = true;
            inner.shared.cv.notify_all();
        }
        // The listener thread owns the transport's listener, which unlinks
        // the socket file as it is dropped on the way out, and only while the
        // file is still the one it bound. So once the join returns, the
        // socket is gone, or belongs to somebody else and is left alone.
        if let Some(handle) = inner.listener.take() {
            let _ = handle.join();
        }
    }
}

/// State shared between the app thread and the connection threads.
struct Shared {
    app_label: String,
    state: Mutex<State>,
    cv: Condvar,
    /// Agent inputs dropped because the input queue was full. Written by
    /// reader threads, read via [`TariaLayer::stats`].
    dropped_inputs: AtomicU64,
    /// Agent inputs discarded rather than delivered: at dequeue because the
    /// connection they arrived on had ended, and before the handshake because
    /// the client wrote them while it waited its turn. Written by the app
    /// thread for the first and by the listener thread for the second, both
    /// with an atomic add, so neither can lose the other's count; read via
    /// [`TariaLayer::stats`].
    stale_inputs: AtomicU64,
    /// Agent inputs answered [`Ignored`](InputStatus::Ignored) by the reader
    /// without reaching the app, because their `kind` did not parse. Written
    /// by reader threads, read via [`TariaLayer::stats`].
    unknown_inputs: AtomicU64,
    /// Acks dropped because [`ACK_QUEUE`] answers were already waiting for a
    /// bridge that is not reading them fast enough. Written by whichever
    /// thread queues an ack, read via [`TariaLayer::stats`].
    dropped_acks: AtomicU64,
}

#[derive(Default)]
struct State {
    /// Latest published snapshot; sent to newly connected clients and, via
    /// `epoch` bumps, streamed to the current client.
    ///
    /// It carries the stamp it was published with, which belongs to the
    /// connection that was live then, so a new connection is sent it with
    /// the stamp taken off (see [`serve_client`]).
    latest: Option<Snapshot>,
    /// The stamp `latest` carries, with the generation it was taken on, or
    /// `None` when it carries none. Compared with [`stamp`](State::stamp) to
    /// tell whether the app has dequeued an input since `latest` went out:
    /// ids restart with each bridge process, so the id alone could match one
    /// a previous connection's bridge used.
    latest_stamp: Option<(u64, InputId)>,
    /// Bumped on every publish so the writer knows something new exists.
    epoch: u64,
    /// Acks waiting to go out, oldest first. Queued for the writer rather
    /// than written where they are produced, so that one thread owns the
    /// stream and an ack can never overtake the snapshot published after it.
    /// Bounded at [`ACK_QUEUE`]; see [`Shared::push_ack`].
    acks: VecDeque<(InputId, InputStatus)>,
    /// Input ids handed to the app on `delivered_generation`, oldest first
    /// and capped at [`ACKABLE_INPUTS`]. [`TariaLayer::ack`] answers only an
    /// id in here, which is how a late ack for a bridge that is gone is told
    /// from one the live bridge is waiting on.
    delivered: VecDeque<InputId>,
    /// The generation `delivered` belongs to. Compared with `generation`
    /// rather than cleared wherever a connection ends, so no path that
    /// retires a generation can forget to clear it.
    delivered_generation: u64,
    /// Which bridge connection is live. Bumped when a connection starts and
    /// again when it ends, so the value between connections matches nothing.
    /// Every queued input carries the generation it arrived on, which is what
    /// lets the app tell an input the live bridge is waiting on from one that
    /// outlived its sender.
    generation: u64,
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
    ///
    /// For acks the layer produces itself, about an input the app never saw:
    /// a [`Dropped`](InputStatus::Dropped) for one the queue had no room for,
    /// an [`Ignored`](InputStatus::Ignored) for one this build cannot read.
    /// The app's own acks go through [`queue_app_ack`](Self::queue_app_ack),
    /// which first checks the input still belongs to the live connection.
    fn queue_ack(&self, id: InputId, status: InputStatus) {
        let mut state = self.lock_state();
        self.push_ack(&mut state, id, status);
        drop(state);
        self.cv.notify_all();
    }

    /// Queue an ack the app asked for, if the input it answers came from the
    /// connection that is live now.
    ///
    /// See [`TariaLayer::ack`] for why an id from a connection that has ended
    /// is dropped in silence rather than sent or reported.
    fn queue_app_ack(&self, id: InputId, status: InputStatus) {
        let mut state = self.lock_state();
        if !state.was_delivered(id) {
            return;
        }
        self.push_ack(&mut state, id, status);
        drop(state);
        self.cv.notify_all();
    }

    /// Append an ack, dropping the oldest one waiting when the queue is full.
    ///
    /// The caller holds the lock; waking the writer is the caller's job too,
    /// because [`deliver`](Self::deliver) has more to do under the same lock.
    /// See [`ACK_QUEUE`] for the bound and why it drops from this end.
    fn push_ack(&self, state: &mut State, id: InputId, status: InputStatus) {
        if state.acks.len() >= ACK_QUEUE {
            state.acks.pop_front();
            self.dropped_acks.fetch_add(1, Ordering::Relaxed);
        }
        state.acks.push_back((id, status));
    }

    /// Retire the generation of the connection being served, so anything
    /// still queued from it is discarded rather than delivered.
    ///
    /// [`reader_loop`] does this as it exits, which covers every connection
    /// that reached the point of having a reader; [`serve_client`] calls it
    /// for the ones that failed before that.
    fn retire_generation(&self) {
        self.lock_state().generation += 1;
    }

    /// Forget every queued ack. An ack answers the client that sent the
    /// input, so one outliving its connection would reach a client that never
    /// sent it. Ids do not repeat for the lifetime of a bridge process (see
    /// [`InputId`]), so the next client would not mistake it for its own, but
    /// there is nothing useful to do with it either.
    fn clear_acks(&self) {
        self.lock_state().acks.clear();
    }

    /// Hand `snapshot` to the writer as the latest, stamped with the newest
    /// input the app has dequeued on the live connection.
    ///
    /// The stamp is read in the same hold of the lock that publishes, so it
    /// names an input dequeued before this publish and on the connection the
    /// writer will send it to: a connection that starts or ends takes the
    /// same lock.
    fn publish(&self, mut snapshot: Snapshot) {
        let mut guard = self.lock_state();
        let state = &mut *guard;
        let stamp = state.stamp();
        snapshot.after_input = stamp.map(|(_, id)| id);
        state.latest_stamp = stamp;
        state.latest = Some(snapshot);
        state.epoch += 1;
        drop(guard);
        self.cv.notify_all();
    }

    /// Send the latest snapshot again, under the same `seq`, if the app has
    /// dequeued an input since it went out; otherwise do nothing.
    ///
    /// For a frame whose tree is the one already published. Skipping it
    /// would leave the bridge with no tree stamped at that input, and a
    /// bridge waiting on one could not tell an input that changed nothing
    /// from one the app is still working on. The tree is not cut or measured
    /// again: it is the snapshot already sent, with only `after_input`
    /// moved, so a bridge that reads no stamp sees the same snapshot twice.
    fn restamp(&self) {
        let mut guard = self.lock_state();
        let state = &mut *guard;
        let stamp = state.stamp();
        if stamp.is_none() || stamp == state.latest_stamp {
            return;
        }
        let Some(latest) = state.latest.as_mut() else {
            return;
        };
        latest.after_input = stamp.map(|(_, id)| id);
        state.latest_stamp = stamp;
        state.epoch += 1;
        drop(guard);
        self.cv.notify_all();
    }

    /// Answer an input the app just dequeued, or refuse it as stale.
    ///
    /// Returns true after queueing the [`Delivered`](InputStatus::Delivered)
    /// ack for an input whose connection is still live. Returns false for an
    /// input from a retired generation: the caller must discard it instead of
    /// acting on it, because it was aimed at a bridge session that no longer
    /// exists. A discard is counted (see [`LayerStats::stale_inputs`]) and
    /// deliberately not acked, since the peer that would read the ack is the
    /// one that is gone.
    fn deliver(&self, generation: u64, id: InputId) -> bool {
        let mut state = self.lock_state();
        if state.generation != generation {
            drop(state);
            // An atomic add, not a load and store: the listener thread counts
            // its pre-handshake discards into the same counter (see
            // `start_connection`), possibly at this very moment.
            self.stale_inputs.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // Remembered before the ack so that an app which refines this ack
        // straight away, inside the same drain closure, is answering an id
        // the layer already knows it handed over.
        state.record_delivered(generation, id);
        self.push_ack(&mut state, id, InputStatus::Delivered);
        drop(state);
        self.cv.notify_all();
        true
    }
}

impl State {
    /// Remember `id` as one the app may answer again with
    /// [`TariaLayer::ack`], for as long as its connection lives.
    fn record_delivered(&mut self, generation: u64, id: InputId) {
        if self.delivered_generation != generation {
            self.delivered.clear();
            self.delivered_generation = generation;
        }
        if self.delivered.len() >= ACKABLE_INPUTS {
            self.delivered.pop_front();
        }
        self.delivered.push_back(id);
    }

    /// Was `id` handed to the app by the connection that is live now?
    ///
    /// False between connections and for anything the previous one delivered,
    /// which is what keeps a late ack from answering a fresh bridge's input
    /// that happens to carry the same id.
    fn was_delivered(&self, id: InputId) -> bool {
        self.delivered_generation == self.generation && self.delivered.contains(&id)
    }

    /// The stamp a snapshot published now carries: the newest input the app
    /// dequeued on the live connection, with that connection's generation,
    /// or `None` when it has dequeued none there.
    ///
    /// Read from what [`record_delivered`](Self::record_delivered) keeps, so
    /// only an input [`deliver`](Shared::deliver) handed over can stamp: one
    /// dropped for want of room, one of a kind this build cannot read, one
    /// discarded as stale, and one written before the hello never reach it.
    fn stamp(&self) -> Option<(u64, InputId)> {
        if self.delivered_generation != self.generation {
            return None;
        }
        self.delivered.back().map(|&id| (self.generation, id))
    }
}

/// Is any node below `root` focused?
///
/// Walks with a stack of sibling iterators rather than by recursing, for the
/// reason [`Node::check_depth`] does: it runs on the tree as built, before
/// the depth cut, and a recursive walk would overflow on a tree deeper than
/// the call stack. The stack holds one iterator per level, so a wide tree
/// costs no more memory than a narrow one.
fn focused_below(root: &Node) -> bool {
    let mut pending = vec![root.children.iter()];
    while let Some(siblings) = pending.last_mut() {
        match siblings.next() {
            Some(node) if node.focused => return true,
            Some(node) => pending.push(node.children.iter()),
            None => {
                pending.pop();
            }
        }
    }
    false
}

/// Take every action off each node marked disabled, so the tree never
/// advertises an action the app has said it will not take.
///
/// Iterative, like [`focused_below`], because it runs on the tree as built,
/// which can be deeper than the call stack.
fn strip_disabled_actions(root: &mut Node) {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.disabled == Some(true) {
            node.actions.clear();
        }
        pending.extend(node.children.iter_mut());
    }
}

/// Are `a` and `b` the same tree?
///
/// What `==` on [`Node`] answers, without its recursion, because one side is
/// the tree as built and can be deeper than the call stack. Each pair of
/// nodes is compared with the children set aside, which still compares every
/// field a node has, the ones a later taria adds included, and the children
/// are then compared as pairs of their own. The `&mut` is only for setting
/// the children aside; both trees come back as they were.
fn same_tree(a: &mut Node, b: &mut Node) -> bool {
    if !same_node(a, b) {
        return false;
    }
    let mut pending = vec![a.children.iter_mut().zip(b.children.iter_mut())];
    while let Some(pairs) = pending.last_mut() {
        match pairs.next() {
            Some((a, b)) => {
                if !same_node(a, b) {
                    return false;
                }
                pending.push(a.children.iter_mut().zip(b.children.iter_mut()));
            }
            None => {
                pending.pop();
            }
        }
    }
    true
}

/// Are `a` and `b` equal apart from their children, and do they have as
/// many?
fn same_node(a: &mut Node, b: &mut Node) -> bool {
    if a.children.len() != b.children.len() {
        return false;
    }
    let a_children = mem::take(&mut a.children);
    let b_children = mem::take(&mut b.children);
    let same = *a == *b;
    a.children = a_children;
    b.children = b_children;
    same
}

/// Free a tree one node at a time.
///
/// The drop glue [`Node`] derives frees a node's children before the node,
/// by recursing, so a tree deeper than the call stack aborts the process
/// when it is dropped: not a panic that unwinds, a stack overflow. Every
/// tree this crate lets go of before the depth cut goes through here
/// instead. Each node is freed with its children moved out to a list of
/// their own, so a chain holds one node there at a time. A child with no
/// children of its own is freed where it stands instead, which recurses no
/// further, so a frame of fifty thousand rows drawn unchanged frees them
/// at the cost of the derived glue rather than queueing every one.
pub(crate) fn drop_iteratively(root: Node) {
    let mut pending = vec![root];
    while let Some(mut node) = pending.pop() {
        pending.extend(
            node.children
                .drain(..)
                .filter(|child| !child.children.is_empty()),
        );
    }
}

/// `node` without its children, copied without copying them.
fn shallow_copy(node: &mut Node) -> Node {
    let children = mem::take(&mut node.children);
    let copy = node.clone();
    node.children = children;
    copy
}

/// A copy of `root` cut to [`MAX_NODE_DEPTH`]: every node at the limit is
/// copied without its children, and nothing below it is copied at all.
///
/// A copy, because the tree as built is kept to compare the next frame
/// against; cut while it is copied, so the part cut away costs nothing. Built
/// with a stack rather than by recursing, because the tree it reads is the
/// one too deep for that. Only the branches that run over lose anything; the
/// rest of the tree keeps every node it had.
fn copy_within_depth(root: &mut Node) -> Node {
    // A copy under construction, beside the children of its original still
    // to copy into it. The stack's height is the level its top is at,
    // counting the root as 1.
    let mut stack = vec![(shallow_copy(root), root.children.iter_mut())];
    while let Some((_, children)) = stack.last_mut() {
        let next = children.next();
        let level = stack.len() + 1;
        match next {
            Some(child) if level >= MAX_NODE_DEPTH => {
                let copy = shallow_copy(child);
                if let Some((parent, _)) = stack.last_mut() {
                    parent.children.push(copy);
                }
            }
            Some(child) => {
                let copy = shallow_copy(child);
                stack.push((copy, child.children.iter_mut()));
            }
            // The root stays on the stack once its last child is in, so the
            // loop ends with it there.
            None if stack.len() == 1 => break,
            None => {
                if let Some((done, _)) = stack.pop()
                    && let Some((parent, _)) = stack.last_mut()
                {
                    parent.children.push(done);
                }
            }
        }
    }
    let copied = stack.pop().map(|(copy, _)| copy);
    drop(stack);
    // The loop never empties the stack, so the fallback is never taken; it
    // is the root alone, which is still a tree within the limit.
    copied.unwrap_or_else(|| shallow_copy(root))
}

/// Accept loop: serves one bridge client at a time until shutdown.
///
/// The transport's [`Listener::accept`] returns once the shutdown flag is
/// set, without waiting for a connection to wake it, and says whether each
/// bridge it hands over waited behind the last one, which is what decides if
/// [`start_connection`] discards what that client wrote before its hello. The
/// listener is dropped as this returns, which unlinks the socket file.
fn accept_loop(mut listener: Listener, shared: Arc<Shared>, input_tx: SyncSender<QueuedInput>) {
    while let Some(Accepted { connection, waited }) = listener.accept(|| shared.is_shutdown()) {
        // Errors just drop this client; the loop keeps accepting.
        let _ = serve_client(connection, &shared, &input_tx, waited);
    }
}

/// Serve one connected bridge: handshake, then reader thread + writer loop
/// until the client disconnects or the layer shuts down.
///
/// `waited` says the client was in the listen backlog when the previous
/// session ended; see [`Listener::accept`] and [`start_connection`].
fn serve_client(
    connection: Connection,
    shared: &Arc<Shared>,
    input_tx: &SyncSender<QueuedInput>,
    waited: bool,
) -> io::Result<()> {
    // Every line is bounded by its own deadline (see `write_line`); this only
    // means the connection never starts out with no write timeout at all.
    connection.set_write_timeout(WRITE_TIMEOUT)?;

    // Capture the snapshot and epoch atomically so the writer loop neither
    // misses nor duplicates a publish that races the handshake. Acks left
    // over from the previous connection go here: they name ids this client
    // never used. The generation taken here tags every input this connection
    // queues, so an input that outlives it is discarded rather than applied
    // to the app on behalf of a bridge that is gone.
    //
    // The snapshot goes out unstamped. Its stamp, if it has one, names an
    // input of the connection that was live when it was published, and ids
    // restart with each bridge process, so on this one it could name an
    // input this bridge has not sent yet.
    let (initial_snapshot, initial_epoch, generation) = {
        let mut state = shared.lock_state();
        state.acks.clear();
        state.generation += 1;
        let initial = state.latest.clone().map(|mut snapshot| {
            snapshot.after_input = None;
            snapshot
        });
        (initial, state.epoch, state.generation)
    };
    let alive = Arc::new(AtomicBool::new(true));
    let (mut writer, reader) = match start_connection(
        connection,
        shared,
        input_tx,
        initial_snapshot,
        &alive,
        generation,
        waited,
    ) {
        Ok(started) => started,
        Err(err) => {
            // The reader thread normally retires this generation as it exits,
            // and here it never started (or never got the handshake). Retire
            // it by hand so the invariant holds through a failed connection
            // too: between connections the counter matches no queued input.
            shared.retire_generation();
            shared.clear_acks();
            return Err(err);
        }
    };

    writer_loop(&mut writer, shared, &alive, initial_epoch);

    // Unblock the reader (it is parked in a blocking read) and reap it before
    // going back to accept the next client.
    let _ = writer.shutdown();
    let _ = reader.join();
    // Whatever is still queued can no longer be delivered, and means nothing
    // to the next client. Queued inputs need no sweep here: the reader
    // retired their generation, so the app discards them as it dequeues.
    shared.clear_acks();
    Ok(())
}

/// Discard what a client that waited its turn wrote before its handshake,
/// send the handshake (and the snapshot a new client is owed) and start the
/// connection's reader thread, so every way of failing to get a connection
/// off the ground funnels through one `Err` for the caller to clean up after.
/// On success the connection's writer comes back beside the reader thread.
///
/// # Input written before the hello
///
/// The accept loop serves one client to completion before it accepts the
/// next, so a client that connects meanwhile sits in the listen backlog with
/// a socket that works, and whatever it writes there waits in the kernel's
/// buffer: a probe measured 180 KB accepted before a write would block, all
/// of it readable once the connection is finally served. Read as the new
/// session's first lines, that is input the app applies on behalf of a client
/// that wrote it blind, to a tree it never saw, and the hello is the boundary
/// that tells the two apart: nothing written before it can have been written
/// in reply to it. See [`LayerStats::stale_inputs`], which counts what is
/// thrown away here.
///
/// Only a client that `waited` is drained: one the accept loop took from the
/// backlog after a session ended, before it last found the backlog empty
/// (see [`Listener::accept`]). A client that connects to an idle app is
/// accepted within one poll of the listener, and one that writes at once
/// without reading the hello, a script piping a line into the socket, waited
/// behind nobody.
/// What it wrote is input for the session it is about to get, as it was in
/// 0.3.0, and discarding it would lose it unacked, with nothing to tell the
/// client. The protocol leaves the choice to the app; this keeps the discard
/// to the client it exists for.
///
/// For a client that waited, the stream is read without blocking, right
/// before the hello is written and not a step earlier, and everything already
/// there is discarded. What arrives between this read and the hello's write
/// slips through, and only a client writing blind can land in that gap. A
/// client that waits for the hello loses nothing either way.
///
/// The bytes may stop partway through a line, whose remainder then arrives
/// as the session's first bytes. That remainder belongs to a line written
/// blind, and its own tail can parse as a whole input (a line with leading
/// whitespace, cut inside it, is one), so the reader is told to skip through
/// the first newline it reads before it treats anything as input.
///
/// More than [`MAX_LINE_BYTES`] waiting is an error, like a line that long:
/// past that it is a flood, not a client that wrote a little too early, and
/// reading it all off would let a client that never stops writing hold the
/// listener here forever.
///
/// The latest snapshot is skipped, not written, once the layer is shutting
/// down, for the reason the writer skips one (see [`writer_loop`]).
fn start_connection(
    connection: Connection,
    shared: &Arc<Shared>,
    input_tx: &SyncSender<QueuedInput>,
    initial_snapshot: Option<Snapshot>,
    alive: &Arc<AtomicBool>,
    generation: u64,
    waited: bool,
) -> io::Result<(Writer, JoinHandle<()>)> {
    let (mut reader, mut writer) = connection.split()?;
    let starts_mid_line = if waited {
        discard_unsolicited(&mut reader, shared)?
    } else {
        false
    };

    write_line(
        &mut writer,
        &AppToBridge::hello(shared.app_label.clone(), PROTOCOL_VERSION).stamps(true),
    )?;
    if let Some(snapshot) = initial_snapshot
        && !shared.is_shutdown()
    {
        write_line(&mut writer, &AppToBridge::Snapshot(snapshot))?;
    }
    let reader = thread::Builder::new().name("taria-reader".into()).spawn({
        let alive = Arc::clone(alive);
        let shared = Arc::clone(shared);
        let input_tx = input_tx.clone();
        move || reader_loop(reader, input_tx, alive, shared, generation, starts_mid_line)
    })?;
    Ok((writer, reader))
}

/// Read off and count what a client that waited its turn wrote before its
/// hello, for [`start_connection`]. Returns whether it stopped partway through
/// a line, whose remainder the reader is then told to skip.
fn discard_unsolicited(reader: &mut Reader, shared: &Shared) -> io::Result<bool> {
    // Only what is already there is read: the reads stop rather than wait,
    // and wait again before anything is written, since the hello and every
    // later write rely on blocking until their deadline.
    let mut unsolicited = Vec::new();
    let read = reader.with_nonblocking_reads(|ready| {
        let read = read_unsolicited(ready, &mut unsolicited);
        // Counted whatever the read ended in: an input read off here was
        // discarded either way, including when the next byte overflows the
        // cap.
        shared
            .stale_inputs
            .fetch_add(count_input_lines(&unsolicited), Ordering::Relaxed);
        read
    })?;
    read?;
    Ok(unsolicited.last().is_some_and(|&byte| byte != b'\n'))
}

/// Append everything `reader` has ready to `pending`, stopping when a read
/// would block or the peer has closed its end.
///
/// For the non-blocking pass [`start_connection`] makes before the hello.
/// More than [`MAX_LINE_BYTES`] in all is an
/// [`InvalidData`](io::ErrorKind::InvalidData) error, with `pending` left
/// holding exactly the first [`MAX_LINE_BYTES`] bytes: the read that ran
/// over is kept up to the cap, so every complete line before it can still be
/// counted as an input thrown away. Any other error ends the read the same
/// way: the connection is broken, and `pending` holds what came before.
fn read_unsolicited(mut reader: impl Read, pending: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let Some(read) = chunk.get(..n) else {
                    return Err(io::Error::other(
                        "a read reported more bytes than it was given",
                    ));
                };
                let room = MAX_LINE_BYTES.saturating_sub(pending.len());
                if n > room {
                    // Up to the cap, so a complete line that fits before it
                    // is counted even though the read it came in ran over.
                    pending.extend_from_slice(read.get(..room).unwrap_or_default());
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "the client wrote more than {MAX_LINE_BYTES} bytes before its \
                             handshake"
                        ),
                    ));
                }
                pending.extend_from_slice(read);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err),
        }
    }
}

/// How many complete lines of `bytes` are agent inputs, of any kind.
///
/// A line that does not end in `\n` is not complete, and neither is it
/// counted: whether it would have parsed is unknowable. Lines that do not
/// parse at all are passed over in silence, as the reader passes them over on
/// a live connection. An input of a kind this build cannot read
/// ([`AgentInput::Unknown`]) counts like any other, because it was discarded
/// like any other; on a live connection it would have been answered, but
/// before the hello there is nobody to answer.
fn count_input_lines(bytes: &[u8]) -> u64 {
    let inputs = bytes
        .split_inclusive(|&byte| byte == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .filter(|line| {
            matches!(
                serde_json::from_slice::<BridgeToApp>(line),
                Ok(BridgeToApp::Input { .. })
            )
        })
        .count();
    u64::try_from(inputs).unwrap_or(u64::MAX)
}

/// Reader half of a connection: parse `BridgeToApp` lines and forward agent
/// inputs to the app. Malformed lines are ignored. A line longer than
/// [`MAX_LINE_BYTES`] marks the connection broken (the loop exits and the
/// client is disconnected) instead of buffering it. When the input queue is
/// full the newest input is dropped, acked [`Dropped`](InputStatus::Dropped)
/// so the agent learns of it, and counted for the app (see
/// [`LayerStats::dropped_inputs`]); a flood of inputs can never block this
/// thread.
///
/// Every forwarded input carries `generation`, which this loop retires when
/// the connection ends so anything still queued from it is discarded instead
/// of delivered.
///
/// `starts_mid_line` says the bytes the client wrote before its hello, which
/// [`start_connection`] discarded, stopped partway through a line. Everything
/// up to and including the first newline is that line's remainder, and is
/// skipped without being parsed, counted or acked; see [`start_connection`].
/// It is still a line, so a remainder over [`MAX_LINE_BYTES`] breaks the
/// connection like any other.
fn reader_loop(
    stream: Reader,
    input_tx: SyncSender<QueuedInput>,
    alive: Arc<AtomicBool>,
    shared: Arc<Shared>,
    generation: u64,
    mut starts_mid_line: bool,
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
        if std::mem::take(&mut starts_mid_line) {
            // The rest of a line the client began before its hello. Its tail
            // can parse on its own, and it was still written blind.
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<BridgeToApp>(&buf) else {
            continue;
        };
        let BridgeToApp::Input { id, input, .. } = msg else {
            // A message variant added to the protocol after this adapter was
            // written. Skipped like a line that failed to parse, and for the
            // same reason: it asks for something this build cannot do. It
            // cannot be acked either, because an ack answers an input id and
            // this message is not an input.
            continue;
        };
        if matches!(input, AgentInput::Unknown) {
            // An input whose `kind` this build cannot read, which the parse
            // kept only because the id lives on the message around it.
            // Answered here rather than handed to the app: the variant
            // carries nothing to act on, so every app would need an arm for a
            // value that is unactionable by construction. Delivering it would
            // also ack `Delivered` at the dequeue, a claim the app took the
            // input, and leave the true answer to whether each app author
            // remembers to refine it. `Ignored` is both true and already
            // known here, so the agent learns in one round trip.
            shared.unknown_inputs.fetch_add(1, Ordering::Relaxed);
            shared.queue_ack(id, InputStatus::Ignored);
            continue;
        }
        let queued = QueuedInput {
            generation,
            id,
            input,
        };
        match input_tx.try_send(queued) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                shared.dropped_inputs.fetch_add(1, Ordering::Relaxed);
                shared.queue_ack(id, InputStatus::Dropped);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    let mut state = shared.lock_state();
    // Retire this connection's generation before anyone can observe the
    // disconnect: an input still queued from it must already be stale by the
    // time the peer is gone, because no ack could reach that peer any more.
    // Only one connection is served at a time and `serve_client` joins this
    // thread before accepting the next, so this always retires the generation
    // that just ended.
    state.generation += 1;
    alive.store(false, Ordering::SeqCst);
    // Notify while holding the lock so the writer cannot miss the wakeup
    // between checking `alive` and parking on the condvar.
    shared.cv.notify_all();
    drop(state);
}

/// Writer half of a connection: send queued acks, then each newly published
/// snapshot, as lines. Acks go first in every pass, so an ack always reaches
/// the client before the snapshot published after it; an agent that saw the
/// snapshot first could not tell whether it reflects its input yet.
///
/// Returns when the layer shuts down, the reader reports the client gone, or
/// a write fails. A shutdown first writes every ack still queued, the rest of
/// the batch in hand included, within [`SHUTDOWN_FLUSH`] in all, and skips the
/// snapshot that would have followed them; a client already gone gets
/// nothing, since nobody is left to read it.
///
/// The shutdown flag is checked before every line, the snapshot's included,
/// so a shutdown that comes while a line is being written waits for that line
/// only, and [`WRITE_TIMEOUT`] bounds the whole of it. That is the bound
/// [`Drop`] promises, because it waits for this thread.
///
/// Every ack that flush can write belongs to this connection. The queue is
/// emptied under the lock as each connection starts, in the same step that
/// moves the generation on, so nothing queued for an earlier one survives
/// into it. While this connection's reader runs, `alive` is true and the
/// generation is this connection's, and the reader changes both together,
/// under the lock, as it exits, so an `alive` read under the lock vouches
/// for every ack taken in the same hold. Every shutdown check takes the acks
/// in the same hold that read `alive`.
fn writer_loop(stream: &mut Writer, shared: &Shared, alive: &AtomicBool, mut last_epoch: u64) {
    loop {
        let (acks, snapshot) = {
            let mut state = shared.lock_state();
            loop {
                if !alive.load(Ordering::SeqCst) {
                    return;
                }
                if state.shutdown {
                    let queued = std::mem::take(&mut state.acks);
                    drop(state);
                    flush_acks(stream, queued);
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
        let mut acks = acks.into_iter();
        while let Some((id, status)) = acks.next() {
            // Rechecked per ack rather than once per pass: a full batch
            // against a peer that stopped reading is a whole queue of write
            // timeouts long, and `Drop` waits for this thread, so an exit
            // during the batch would sit through every one of them. The acks
            // not written yet go out under the flush's bound instead, this
            // one first and anything queued since the batch was taken last.
            let unwritten = std::iter::once((id, status)).chain(&mut acks);
            if stop_writing(stream, shared, alive, unwritten) {
                return;
            }
            if write_line(stream, &AppToBridge::ack(id, status)).is_err() {
                return;
            }
        }
        let Some(snapshot) = snapshot else {
            continue;
        };
        // Checked again here, or a shutdown that came while the batch's last
        // ack was being written would be noticed only after the snapshot:
        // the one line a shutdown skips, and the longest.
        if stop_writing(stream, shared, alive, std::iter::empty()) {
            return;
        }
        if write_line(stream, &AppToBridge::Snapshot(snapshot)).is_err() {
            return;
        }
    }
}

/// The check [`writer_loop`] makes before each line of a batch: is the
/// client gone, or is the layer shutting down?
///
/// Returns true when the writer must stop: at once when the client is gone,
/// and when the layer is shutting down, once `unwritten` and then every ack
/// queued since the batch was taken have been flushed. `unwritten` is
/// consumed only in that case, so the caller can go on with it otherwise. The
/// queued acks are taken in the same hold of the lock that read `alive`,
/// which is what vouches for them; see [`writer_loop`].
fn stop_writing(
    stream: &mut Writer,
    shared: &Shared,
    alive: &AtomicBool,
    unwritten: impl IntoIterator<Item = (InputId, InputStatus)>,
) -> bool {
    let mut state = shared.lock_state();
    if !alive.load(Ordering::SeqCst) {
        return true;
    }
    if !state.shutdown {
        return false;
    }
    let queued = std::mem::take(&mut state.acks);
    drop(state);
    flush_acks(stream, unwritten.into_iter().chain(queued));
    true
}

/// Serialize one message as a newline-terminated JSON frame and write it,
/// the whole line within [`WRITE_TIMEOUT`] of starting or not at all.
///
/// The deadline is on the line, not on each `write` it takes; see
/// [`WRITE_TIMEOUT`] for why. A line that misses it may be written in part,
/// and every caller then drops the connection, which is the only thing that
/// can follow a partial line.
fn write_line(stream: &mut Writer, msg: &AppToBridge) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
    line.push(b'\n');
    let deadline = Instant::now()
        .checked_add(WRITE_TIMEOUT)
        .ok_or_else(|| io::Error::other("the write deadline is past what the clock can hold"))?;
    stream.write_all_before(&line, deadline)
}

/// Write the acks a shutdown found queued, oldest first, giving up once
/// [`SHUTDOWN_FLUSH`] has passed or a write fails.
///
/// Best effort by design: the caller returns either way, and an ack that
/// misses the deadline costs its waiter the timeout it would have waited out
/// with no flush at all. The connection is closed straight after, so the
/// write timeout this leaves on the socket is never used again.
fn flush_acks(stream: &mut Writer, acks: impl IntoIterator<Item = (InputId, InputStatus)>) {
    let Some(deadline) = Instant::now().checked_add(SHUTDOWN_FLUSH) else {
        return;
    };
    // One buffer and one deadline for the lot, rather than a line at a time,
    // so the bound is on the flush and not on each of its writes.
    let mut lines = Vec::new();
    for (id, status) in acks {
        let Ok(line) = serde_json::to_vec(&AppToBridge::ack(id, status)) else {
            break;
        };
        lines.extend_from_slice(&line);
        lines.push(b'\n');
    }
    let _ = stream.write_all_before(&lines, deadline);
}

/// Resolve the default socket path for `app_label` from the environment.
///
/// The rule itself lives in [`taria::socket::resolve_path`], shared with the
/// bridge so the two sides cannot look for the socket in different places,
/// including its refusal of a label that is not a plain file name. Reading the
/// environment stays here, where the process actually is.
#[cfg(not(windows))]
fn resolve_socket_path(app_label: &str) -> Result<PathBuf, taria::socket::InvalidAppLabel> {
    taria::socket::resolve_path(
        env::var_os("TARIA_SOCK"),
        env::var_os("XDG_RUNTIME_DIR"),
        &env::temp_dir(),
        &transport::user_identity(),
        app_label,
    )
}

/// A refused app label as the `io::Error` every bind path already reports.
///
/// `InvalidInput` because the label is one: the same kind
/// [`bind_at`](TariaLayer::bind_at) gives an over-long path, and the message
/// is the label error's own, which names the label and what a label may be.
#[cfg(not(windows))]
fn invalid_label(err: taria::socket::InvalidAppLabel) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
}

/// Resolve the default pipe name for `app_label` from the environment: the
/// Windows counterpart of the function above, by
/// [`taria::socket::resolve_pipe_name`], shared with the bridge, with the
/// user's SID from the process token.
///
/// A refused label or `$TARIA_SOCK` is `InvalidInput`, with the refusal's own
/// message. The SID is needed only when `$TARIA_SOCK` does not name the pipe,
/// so a token that cannot be read is reported only then, in place of the
/// refusal of the empty user it stands in for.
#[cfg(windows)]
fn resolve_socket_path(app_label: &str) -> io::Result<PathBuf> {
    let user = transport::user_identity();
    let resolved = taria::socket::resolve_pipe_name(
        env::var_os("TARIA_SOCK"),
        user.as_deref().unwrap_or_default(),
        app_label,
    );
    match (resolved, user) {
        (Ok(name), _) => Ok(PathBuf::from(name)),
        (Err(taria::socket::InvalidPipeName::User(_)), Err(err)) => Err(err),
        (Err(err), _) => Err(io::Error::new(io::ErrorKind::InvalidInput, err.to_string())),
    }
}

/// On Windows the resolution's error is an `io::Error` already.
#[cfg(windows)]
fn invalid_label(err: io::Error) -> io::Error {
    err
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    #[cfg(unix)]
    use std::os::unix::net::{UnixListener, UnixStream};

    use super::*;
    use crate::test_util::bind_test_layer;
    #[cfg(unix)]
    use crate::transport::{absolutize, ensure_private_dir};

    /// Unique, absent scratch path under the system temp dir.
    #[cfg(unix)]
    fn scratch_path(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("taria-dirtest-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        let _ = fs::remove_file(&path);
        path
    }

    /// A private (0700) temp dir that removes itself when the test ends, so
    /// nothing is left in the system temp dir even on panic.
    fn private_temp_dir(prefix: &str) -> tempfile::TempDir {
        let mut builder = tempfile::Builder::new();
        builder.prefix(prefix);
        #[cfg(unix)]
        builder.permissions(fs::Permissions::from_mode(0o700));
        builder.tempdir().expect("create test dir")
    }

    /// The shared state a connection's threads work on, without a socket, for
    /// tests that drive those parts directly.
    fn test_shared(app_label: &str) -> Arc<Shared> {
        Arc::new(Shared {
            app_label: app_label.to_string(),
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            dropped_inputs: AtomicU64::new(0),
            stale_inputs: AtomicU64::new(0),
            unknown_inputs: AtomicU64::new(0),
            dropped_acks: AtomicU64::new(0),
        })
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_world_accessible_dir() {
        let dir = scratch_path("world-writable");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

        let err = ensure_private_dir(&dir).expect_err("0777 dir must be rejected");
        assert!(err.to_string().contains("group/world"), "err: {err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    #[test]
    fn bind_at_refuses_an_over_long_socket_path() {
        let dir = private_temp_dir("taria-longpath-");
        let path = dir.path().join(format!("{}.sock", "x".repeat(120)));

        let Err(err) = TariaLayer::bind_at("toolong", &path) else {
            panic!("over-long path must be refused, not bound");
        };
        let message = err.to_string();
        assert!(
            message.contains(&format!("{} byte", taria::socket::MAX_SOCKET_PATH_BYTES)),
            "err: {message}"
        );
        assert!(message.contains("TARIA_SOCK"), "err: {message}");
        assert!(!path.exists(), "no socket file may be left behind");
    }

    /// The label names the socket file, and this layer binds and unlinks what
    /// the resolution returns. A label that is a path instead of a file name
    /// must therefore be refused before any of that, on both entry points:
    /// `bind` cannot resolve one, and `bind_or_disabled`, which never fails,
    /// has to come back disabled rather than pointed somewhere else.
    #[test]
    fn a_label_that_is_not_a_file_name_binds_nothing() {
        for label in ["/etc/cron.d/evil", "../../../tmp/pwn", "sub/dir"] {
            let Err(err) = TariaLayer::bind(label) else {
                panic!("bind accepted the label {label:?}");
            };
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "label: {label}");
            assert!(err.to_string().contains(label), "err: {err}");

            let layer = TariaLayer::bind_or_disabled(label);
            assert!(!layer.is_enabled(), "label: {label}");
            assert_eq!(layer.app_label(), label, "the label is reported as given");
            assert_eq!(
                layer.socket_path(),
                Path::new(""),
                "a refused label resolves to no path"
            );
            let message = layer
                .bind_error()
                .expect("a disabled layer says why")
                .to_string();
            assert!(message.contains(label), "err: {message}");
        }
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
        assert_eq!(layer.stats(), LayerStats::default());

        assert_eq!(layer.try_recv(), None);
        assert_eq!(layer.recv_timeout(Duration::from_millis(1)), None);
        layer.ack(InputId::new(0), InputStatus::Ignored);

        let mut drained = 0;
        layer.drain(|_| {
            drained += 1;
            InputStatus::Ignored
        });
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

    /// A chain of `depth` nodes, one child each: `n1` down to `n{depth}`.
    fn chain(depth: usize) -> Node {
        let mut node = Node::new(format!("n{depth}"), Role::TreeItem);
        for level in (1..depth).rev() {
            node = Node::new(format!("n{level}"), Role::TreeItem).child(node);
        }
        node
    }

    /// The auto root counts towards the depth, so a chain of
    /// `MAX_NODE_DEPTH - 1` is exactly at the limit and nothing is cut.
    /// Checked on its own, because it is the boundary the cut must not creep
    /// past into trees that were always fine.
    #[test]
    fn a_tree_at_the_depth_limit_is_published_untouched() {
        let mut layer = bind_test_layer("taria-layer-", "atlimit");
        layer.publish([chain(MAX_NODE_DEPTH - 1)]);

        let root = layer
            .latest_snapshot()
            .expect("a tree at the limit publishes")
            .root;
        assert_eq!(root.depth(), MAX_NODE_DEPTH);
        assert!(root.check_depth().is_ok());
        assert_eq!(layer.stats().truncated_snapshots, 0);
        assert_eq!(layer.stats().last_truncation, None);
    }

    /// The failure this enforcement exists for: past the limit a snapshot
    /// does not arrive at the peer at all, and nothing anywhere says so. So
    /// the tree still goes out, cut to fit, rather than being held back (the
    /// agent would keep reading a stale tree, which is the same failure) or
    /// sent whole (which is the failure itself).
    #[test]
    fn a_tree_over_the_depth_limit_is_cut_rather_than_dropped() {
        let mut layer = bind_test_layer("taria-layer-", "deep");
        assert!(layer.latest_snapshot().is_none(), "nothing published yet");

        // One node over the limit once the auto root is counted.
        layer.publish([chain(MAX_NODE_DEPTH)]);
        let snapshot = layer
            .latest_snapshot()
            .expect("a tree over the limit is published cut, never withheld");
        assert_eq!(snapshot.seq, 1);
        assert_eq!(snapshot.root.depth(), MAX_NODE_DEPTH);
        assert!(snapshot.root.check_depth().is_ok());

        // Everything above the cut is untouched: only the deepest node lost
        // its children.
        let mut node = &snapshot.root;
        for level in 1..MAX_NODE_DEPTH {
            assert_eq!(node.children.len(), 1, "level {level} lost a sibling");
            node = &node.children[0];
        }
        assert_eq!(node.id.0, format!("n{}", MAX_NODE_DEPTH - 1));
        assert!(node.children.is_empty(), "the node at the limit is a leaf");

        // And the app can find out, with the branch named.
        assert_eq!(layer.stats().truncated_snapshots, 1);
        let too_deep = layer.stats().last_truncation.expect("the cut is recorded");
        assert_eq!(too_deep.depth(), MAX_NODE_DEPTH + 1);
        assert_eq!(too_deep.deepest().0, format!("n{MAX_NODE_DEPTH}"));
    }

    /// Focus is decided after the cut, not before: a snapshot has to carry
    /// focus somewhere, and the only focused node can be in the part removed.
    #[test]
    fn a_cut_that_removes_the_only_focused_node_leaves_focus_on_the_root() {
        let mut layer = bind_test_layer("taria-layer-", "deepfocus");
        let mut deep = Node::new("leaf", Role::TreeItem).focused(true);
        for level in (1..MAX_NODE_DEPTH).rev() {
            deep = Node::new(format!("n{level}"), Role::TreeItem).child(deep);
        }
        layer.publish([deep]);

        let root = layer.latest_snapshot().expect("published").root;
        assert_eq!(layer.stats().truncated_snapshots, 1);
        assert!(
            root.focused,
            "the focused node was cut, so the root has to carry focus"
        );
        assert!(!subtree_has_focus_below_root(&root));
    }

    /// Focus anywhere under the root, which is what the root's own focus flag
    /// is the negation of.
    fn subtree_has_focus_below_root(root: &Node) -> bool {
        focused_below(root)
    }

    /// The tree as built is compared, searched for focus and copied before
    /// the depth cut, so none of the three may recurse: the tree the cut
    /// exists for can be deeper than the call stack.
    #[test]
    fn a_tree_deeper_than_the_call_stack_is_compared_and_cut_without_recursing() {
        const DEEP: usize = 100_000;
        let mut deep = chain(DEEP);
        let mut same = chain(DEEP);
        // Different only at the very bottom, so the comparison has to walk
        // the whole way down to see it.
        let mut other = Node::new(format!("n{DEEP}"), Role::TreeItem).label("changed");
        for level in (1..DEEP).rev() {
            other = Node::new(format!("n{level}"), Role::TreeItem).child(other);
        }

        assert!(!focused_below(&deep));
        assert!(same_tree(&mut deep, &mut same));
        assert!(!same_tree(&mut deep, &mut other));

        let cut = copy_within_depth(&mut deep);
        assert_eq!(cut.depth(), MAX_NODE_DEPTH);
        assert!(cut.check_depth().is_ok());
        assert_eq!(deep.depth(), DEEP, "the tree as built is left whole");
        for tree in [deep, same, other] {
            drop_iteratively(tree);
        }
    }

    /// Run `test` on a thread with a stack far smaller than a recursive drop
    /// of a 100k-deep tree needs, so a recursion anywhere in it overflows
    /// however large the test thread's own stack happens to be. A stack
    /// overflow aborts the process rather than failing the test, so this is
    /// what makes such a regression fail every time instead of now and then.
    fn on_a_small_stack(test: impl FnOnce() + Send + 'static) {
        let outcome = thread::Builder::new()
            .name("small-stack".into())
            .stack_size(512 * 1024)
            .spawn(test)
            .expect("spawn a test thread")
            .join();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// The derived drop glue of [`Node`] recurses, so every tree the layer
    /// lets go of before the depth cut is freed one node at a time: the
    /// frame drawn unchanged, the tree as built that a changed frame
    /// replaces, the one the layer holds when it is dropped, and whatever a
    /// disabled layer or an unpublished frame is handed. Publishing a tree
    /// deeper than the call stack succeeded before this, and the next
    /// publish aborted the process.
    #[test]
    fn a_tree_deeper_than_the_call_stack_is_freed_without_recursing() {
        const DEEP: usize = 100_000;
        on_a_small_stack(|| {
            let mut layer = bind_test_layer("taria-layer-", "deepfree");
            layer.publish([chain(DEEP)]);
            // Drawn again unchanged: the frame's own tree is freed.
            layer.publish([chain(DEEP)]);
            assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
            // A different one, which the depth cut makes the snapshot already
            // sent: the tree as built that it replaces is freed all the same.
            layer.publish([chain(DEEP + 1)]);
            assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
            // And one that is a new snapshot.
            layer.publish([chain(DEEP), Node::new("new", Role::Text)]);
            assert_eq!(layer.latest_snapshot().expect("published").seq, 2);
            assert_eq!(layer.stats().truncated_snapshots, 2);
            // A frame recorded and never published is freed with it.
            layer.frame().push(chain(DEEP));
            // And the layer frees the one it still holds.
            drop(layer);

            let mut disabled =
                TariaLayer::disabled("deepfree", PathBuf::new(), io::Error::other("test"));
            disabled.publish([chain(DEEP)]);
        });
    }

    /// Only the branches that run over are cut. A wide tree with one runaway
    /// branch keeps everything else, which is what makes cutting worth more
    /// than refusing the publish.
    #[test]
    fn only_the_branch_that_runs_over_is_cut() {
        let mut layer = bind_test_layer("taria-layer-", "deepwide");
        layer.publish([
            Node::new("shallow", Role::Text).label("kept"),
            chain(MAX_NODE_DEPTH * 4),
        ]);

        let root = layer.latest_snapshot().expect("published").root;
        assert_eq!(root.depth(), MAX_NODE_DEPTH);
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].id.0, "shallow");
        assert_eq!(root.children[0].label.as_deref(), Some("kept"));
        assert_eq!(layer.stats().truncated_snapshots, 1);
    }

    /// The counter counts snapshots the agent was sent, not frames the app
    /// drew. A deep tree redrawn unchanged is the snapshot already published
    /// once it is cut, so it is not sent again and must not count again: at
    /// sixty frames a second it would otherwise add sixty a second.
    #[test]
    fn the_same_over_deep_tree_published_twice_counts_once() {
        let mut layer = bind_test_layer("taria-layer-", "deeptwice");
        layer.publish([chain(MAX_NODE_DEPTH * 2)]);
        layer.publish([chain(MAX_NODE_DEPTH * 2)]);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
        assert_eq!(layer.stats().truncated_snapshots, 1);

        // A deep tree that changed is a new snapshot, cut again.
        layer.publish([chain(MAX_NODE_DEPTH * 2), Node::new("new", Role::Text)]);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 2);
        assert_eq!(layer.stats().truncated_snapshots, 2);
    }

    /// One focused text input holding `value`, as a draft would be published.
    fn draft(value: String) -> Node {
        Node::new("draft", Role::TextInput)
            .value(value)
            .focused(true)
    }

    /// The root the layer builds around [`draft`], for sizing values against
    /// and for the line it would have been sent as.
    fn root_around_draft(label: &str, value: String) -> Node {
        Node::new("app", Role::App).label(label).child(draft(value))
    }

    fn line_len(snapshot: Snapshot) -> usize {
        serde_json::to_vec(&AppToBridge::Snapshot(snapshot))
            .expect("serialize a snapshot")
            .len()
    }

    /// The size boundary, through the layer: a root exactly at the budget is
    /// published as built and counted nowhere; one byte more is cut, counted,
    /// and still fits the line.
    #[test]
    fn a_tree_at_the_size_budget_is_published_untouched_and_one_byte_over_is_cut() {
        let mut layer = bind_test_layer("taria-layer-", "atsize");
        let base = serde_json::to_vec(&root_around_draft("atsize", String::new()))
            .expect("serialize a root")
            .len();
        let at = "x".repeat(ROOT_BUDGET - base);

        layer.publish([draft(at.clone())]);
        let snapshot = layer
            .latest_snapshot()
            .expect("a tree at the budget publishes");
        assert_eq!(snapshot.root, root_around_draft("atsize", at.clone()));
        assert!(line_len(snapshot) <= MAX_LINE_BYTES);
        assert_eq!(layer.stats().oversized_snapshots, 0);
        assert_eq!(layer.stats().last_oversize, None);

        let over = format!("{at}x");
        layer.publish([draft(over.clone())]);
        let snapshot = layer
            .latest_snapshot()
            .expect("a tree over is published cut");
        assert_eq!(snapshot.seq, 2);
        let value = snapshot.root.children[0].value.clone().expect("a value");
        assert!(value.ends_with('\u{2026}'), "the cut is marked");
        assert!(over.starts_with(value.trim_end_matches('\u{2026}')));
        assert!(snapshot.root.children[0].focused, "focus is untouched");
        assert!(line_len(snapshot) <= MAX_LINE_BYTES);

        assert_eq!(layer.stats().oversized_snapshots, 1);
        let too_large = layer.stats().last_oversize.expect("the cut is recorded");
        // The line it would have been, counted at the largest seq and stamp
        // the way the bound counts them: one byte over the root budget is one
        // byte over the line.
        let unbounded = Snapshot::new(u64::MAX, root_around_draft("atsize", over))
            .after(InputId::new(u64::MAX));
        assert_eq!(too_large.bytes(), line_len(unbounded));
        assert_eq!(too_large.bytes(), MAX_LINE_BYTES + 1);
        assert_eq!(too_large.limit(), MAX_LINE_BYTES);
        assert_eq!(too_large.strings_trimmed(), 1);
        assert_eq!(too_large.nodes_dropped(), 0);
        assert_eq!(too_large.largest().0, "draft");
        let message = too_large.to_string();
        assert!(message.contains("`draft`"), "{message}");
        assert!(message.contains(&MAX_LINE_BYTES.to_string()), "{message}");
    }

    /// Counted like the depth cut: an oversized draft that sits still while
    /// the app redraws it is one snapshot, and an edit to it is another.
    #[test]
    fn the_same_oversized_tree_published_twice_counts_once() {
        let mut layer = bind_test_layer("taria-layer-", "bigtwice");
        let big = "y".repeat(MAX_LINE_BYTES * 2);
        layer.publish([draft(big.clone())]);
        layer.publish([draft(big.clone())]);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
        assert_eq!(layer.stats().oversized_snapshots, 1);

        // An edit past the cut changes nothing the agent would be sent, so
        // it is not a new snapshot either.
        layer.publish([draft(format!("{big}z"))]);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
        assert_eq!(layer.stats().oversized_snapshots, 1);

        // One the agent would see is.
        layer.publish([draft(format!("z{big}"))]);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 2);
        assert_eq!(layer.stats().oversized_snapshots, 2);
    }

    /// Focus is decided after the bound, as it is after the depth cut: the
    /// rows dropped to fit can include the focused one, and a snapshot has
    /// to carry focus somewhere.
    #[test]
    fn a_size_bound_that_drops_the_only_focused_node_leaves_focus_on_the_root() {
        let mut layer = bind_test_layer("taria-layer-", "bigfocus");
        // The focused row is the last, and a list keeps a run of its first
        // rows, so it is the first row to go.
        let rows = (0..60_000).map(|n| {
            Node::new(format!("row-{n:05}"), Role::ListItem)
                .label(format!("Task {n:05}"))
                .focused(n == 59_999)
        });
        layer.publish([Node::new("list", Role::List).children(rows)]);

        let root = layer.latest_snapshot().expect("published").root;
        assert_eq!(layer.stats().oversized_snapshots, 1);
        let too_large = layer.stats().last_oversize.expect("recorded");
        assert!(too_large.nodes_dropped() > 0, "the last row went");
        assert!(
            root.focused,
            "the focused row was dropped, so the root has to carry focus"
        );
        assert!(!subtree_has_focus_below_root(&root));
    }

    /// Through the layer, the line a node that could never fit leaves: the
    /// draft beside it goes out whole, in the line it would have had alone
    /// plus the chrome. Dropped only after every string was cut to the
    /// floor, it left a line of 590 bytes with 256 of them the draft.
    #[test]
    fn a_node_that_could_never_fit_leaves_the_draft_beside_it_whole() {
        let mut layer = bind_test_layer("taria-layer-", "unfit");
        let value = "d".repeat(300_000);
        let chrome = [
            Node::new("status", Role::Status).value("1 draft"),
            Node::new("quit", Role::Button)
                .label("Quit")
                .action(taria::Action::Activate),
        ];
        let mut nodes = vec![
            Node::new("I".repeat(2_000_000), Role::Text),
            draft(value.clone()),
        ];
        nodes.extend(chrome.clone());
        layer.publish(nodes);

        let snapshot = layer.latest_snapshot().expect("published");
        let ids: Vec<&str> = snapshot
            .root
            .children
            .iter()
            .map(|child| child.id.0.as_str())
            .collect();
        assert_eq!(ids, ["draft", "status", "quit"]);
        assert_eq!(snapshot.root.children[0], draft(value.clone()));
        assert_eq!(snapshot.root.children[1..], chrome);

        let alone = line_len(Snapshot::new(1, root_around_draft("unfit", value)));
        let line = line_len(snapshot);
        assert!(line > alone && line <= MAX_LINE_BYTES, "{line} of {alone}");
        let too_large = layer.stats().last_oversize.expect("recorded");
        assert_eq!(too_large.strings_trimmed(), 0);
        assert_eq!(too_large.nodes_dropped(), 1);
    }

    /// The cost the comparison before the bound exists for. Deduplicating
    /// after the bound meant a tree too large for the line paid the whole
    /// bound on every frame it was drawn, changed or not, which for a few
    /// megabytes is a hundred milliseconds a frame. Unchanged, a frame is now
    /// compared and nothing else.
    #[test]
    fn an_unchanged_oversized_tree_is_bounded_once_however_often_it_is_drawn() {
        let mut layer = bind_test_layer("taria-layer-", "bigstill");
        let rows: Vec<Node> = (0..5000)
            .map(|n| Node::new(format!("row-{n:05}"), Role::ListItem).label(format!("Task {n:05}")))
            .collect();
        let frame = |text: &str| {
            [
                draft(text.to_string()),
                Node::new("list", Role::List).children(rows.clone()),
            ]
        };
        let big = "d".repeat(MAX_LINE_BYTES * 3);

        let start = bound::runs();
        let nodes = frame(&big);
        let started = Instant::now();
        layer.publish(nodes);
        let first = started.elapsed();
        let once = bound::runs();
        assert_eq!(once.measured - start.measured, 1);
        assert_eq!(once.cut - start.cut, 1);

        let mut redrawn = Duration::ZERO;
        for _ in 1..100 {
            // Built outside the clock: what is timed is the layer's work.
            let nodes = frame(&big);
            let started = Instant::now();
            layer.publish(nodes);
            redrawn += started.elapsed();
        }
        assert_eq!(
            bound::runs(),
            once,
            "an unchanged frame is neither measured nor cut"
        );
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
        assert_eq!(layer.stats().oversized_snapshots, 1);
        let per_frame = redrawn / 99;
        eprintln!("first frame {first:?}, each unchanged frame after it {per_frame:?}");
        assert!(
            per_frame * 4 < first,
            "an unchanged frame took {per_frame:?}, against {first:?} for the one bounded"
        );

        // An edit past the cut is cut once and sends nothing, and the frames
        // that redraw it unchanged after that are not cut again.
        let edited = format!("{big}z");
        for _ in 0..10 {
            layer.publish(frame(&edited));
        }
        let after = bound::runs();
        assert_eq!(after.cut - once.cut, 1);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
        assert_eq!(layer.stats().oversized_snapshots, 1);
    }

    /// The comparison costs a tree that fits nothing extra: it is measured
    /// once, never cut, and held once, as the snapshot it went out as. Only a
    /// tree that had to be cut keeps the tree it was cut from as well.
    #[test]
    fn a_tree_that_fits_is_measured_once_and_held_once() {
        let mut layer = bind_test_layer("taria-layer-", "fitstill");
        let frame = || {
            [Node::new("list", Role::List)
                .children((0..100).map(|n| Node::new(format!("row-{n}"), Role::ListItem)))]
        };
        let start = bound::runs();
        for _ in 0..10 {
            layer.publish(frame());
        }
        let runs = bound::runs();
        assert_eq!(runs.measured - start.measured, 1);
        assert_eq!(runs.cut, start.cut);
        let inner = layer.inner.as_ref().expect("enabled");
        assert!(
            inner.last_raw.is_none(),
            "no second copy of a tree that fits"
        );
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);

        layer.publish([draft("y".repeat(MAX_LINE_BYTES * 2))]);
        let inner = layer.inner.as_ref().expect("enabled");
        assert!(
            inner.last_raw.is_some(),
            "a cut tree keeps what it was cut from"
        );
    }

    /// A deep tree whose runaway branch is the last child at every level: 40
    /// levels, cut to 32 for depth and then to the line for size. Any one
    /// cap on children that keeps the branch keeps everything, so a cap
    /// alone kept the first level's leaves and a few percent of the line.
    /// What the cap dropped is offered back, shallowest first, so the line
    /// is used.
    #[test]
    fn a_deep_tree_cut_for_depth_and_then_for_size_still_fills_the_line() {
        let mut layer = bind_test_layer("taria-layer-", "deepfill");
        let leaves = |level: usize| {
            (0..200).map(move |n| {
                Node::new(format!("leaf-{level}-{n}"), Role::Text).label("x".repeat(250))
            })
        };
        // 39 links under the root, so 40 levels counting it.
        let mut branch = Node::new("n39", Role::TreeItem).children(leaves(39));
        for level in (1..39).rev() {
            branch = Node::new(format!("n{level}"), Role::TreeItem)
                .children(leaves(level))
                .child(branch);
        }
        layer.publish([branch]);

        let snapshot = layer.latest_snapshot().expect("published");
        assert_eq!(layer.stats().truncated_snapshots, 1);
        assert_eq!(layer.stats().oversized_snapshots, 1);
        assert_eq!(
            snapshot.root.children[0].children.len(),
            201,
            "the first level is kept whole, branch and all"
        );
        let len = line_len(snapshot);
        assert!(len <= MAX_LINE_BYTES);
        assert!(
            len * 10 >= MAX_LINE_BYTES * 9,
            "{len} of {MAX_LINE_BYTES} bytes used"
        );
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

    /// A disabled node goes out with no actions, at any depth and whatever
    /// it was built with, and keeps everything else; an enabled sibling keeps
    /// its own. An empty action list alone marks nothing disabled.
    #[test]
    fn a_disabled_node_is_published_with_no_actions() {
        let mut layer = bind_test_layer("taria-layer-", "disabled");
        let save = |disabled: bool| {
            Node::new("save", Role::Button)
                .label("Save")
                .disabled(disabled)
                .focused(true)
                .actions([taria::Action::Activate, taria::Action::Custom("x".into())])
        };
        let nested = Node::new("menu", Role::Menu).disabled(true).child(
            Node::new("open", Role::MenuItem)
                .disabled(true)
                .action(taria::Action::Activate),
        );
        let quit = Node::new("quit", Role::Button).action(taria::Action::Activate);
        let empty = Node::new("idle", Role::Button);
        layer.publish([save(true), nested, quit.clone(), empty.clone()]);

        let snapshot = layer.latest_snapshot().expect("published");
        let root = &snapshot.root;
        let saved = &root.children[0];
        assert!(saved.actions.is_empty(), "{saved:?}");
        assert_eq!(saved.disabled, Some(true));
        assert!(saved.focused && saved.label.as_deref() == Some("Save"));
        assert!(root.children[1].actions.is_empty());
        assert!(root.children[1].children[0].actions.is_empty());
        assert_eq!(root.children[2], quit);
        assert_eq!(root.children[3], empty);
        assert_eq!(root.children[3].disabled, None);

        // The same frame again is the snapshot already sent, and enabling
        // the node brings its actions back.
        layer.publish([
            save(true),
            Node::new("menu", Role::Menu).disabled(true).child(
                Node::new("open", Role::MenuItem)
                    .disabled(true)
                    .action(taria::Action::Activate),
            ),
            quit.clone(),
            empty.clone(),
        ]);
        assert_eq!(layer.latest_snapshot().expect("published").seq, 1);
        layer.publish([save(false)]);
        let snapshot = layer.latest_snapshot().expect("published");
        assert_eq!(snapshot.seq, 2);
        assert_eq!(snapshot.root.children[0], save(false));
    }

    /// A number that is not finite cannot be stored on a node, so it never
    /// reaches the line, which could not spell it, and never leaves a tree
    /// unequal to itself, which would have defeated the comparison that skips
    /// an unchanged frame: every frame would have gone out again.
    #[test]
    fn a_number_that_is_not_finite_never_ships_or_breaks_the_dedupe() {
        let mut layer = bind_test_layer("taria-layer-", "nan");
        let bar = || {
            let mut bar = Node::new("bar", Role::ProgressBar).range(f64::NAN, 0.0, f64::INFINITY);
            bar.value_max = taria::Number::new(f64::NEG_INFINITY);
            bar
        };
        layer.publish([bar()]);
        layer.publish([bar()]);
        layer.publish([bar()]);
        let snapshot = layer.latest_snapshot().expect("published");
        assert_eq!(snapshot.seq, 1, "the same frame drawn again is not resent");
        let published = &snapshot.root.children[0];
        assert_eq!(
            (published.value_now, published.value_max),
            (None, None),
            "{published:?}"
        );
        assert_eq!(published.value_min.map(taria::Number::get), Some(0.0));
        let line = serde_json::to_string(&snapshot).expect("serializes");
        assert!(
            !line.contains("NaN") && !line.contains("inf") && !line.contains("null"),
            "{line}"
        );
    }

    /// The ordering [`writer_loop`] promises, exercised where it actually
    /// bites: an ack and a newer snapshot pending in the *same* pass. Driving
    /// the loop directly over a socket pair is the only way to make that
    /// overlap certain; through a live connection the writer wakes on the
    /// first of the two and usually drains it before the second exists, so
    /// the socket tests only ever see the two in separate passes.
    #[cfg(unix)]
    #[test]
    fn writer_sends_pending_acks_before_a_pending_snapshot() {
        let (server, client) = UnixStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");

        let shared = test_shared("ackorder");
        {
            // Both are waiting before the loop runs, so it has to choose an
            // order: the app acked an input and published a new tree in
            // response to it.
            let mut state = shared.lock_state();
            state.acks.push_back((7.into(), InputStatus::Delivered));
            state.latest = Some(Snapshot::new(1, Node::new("app", Role::App)));
            state.epoch = 1;
        }

        let alive = Arc::new(AtomicBool::new(true));
        let writer = thread::spawn({
            let shared = Arc::clone(&shared);
            let alive = Arc::clone(&alive);
            move || {
                let mut server = Writer::from(server);
                writer_loop(&mut server, &shared, &alive, 0);
            }
        });

        let mut reader = BufReader::new(client);
        let mut read_message = || {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read a message");
            assert!(n > 0, "writer closed the stream");
            serde_json::from_str::<AppToBridge>(&line).expect("parse a message")
        };
        assert_eq!(
            read_message(),
            AppToBridge::ack(InputId::new(7), InputStatus::Delivered),
            "an agent that saw the snapshot first could not tell whether it \
             already reflects its input"
        );
        let AppToBridge::Snapshot(snapshot) = read_message() else {
            panic!("expected the snapshot after the ack");
        };
        assert_eq!(snapshot.seq, 1);

        shared.lock_state().shutdown = true;
        shared.cv.notify_all();
        writer.join().expect("writer thread");
    }

    /// Write junk into `stream` until the peer's side is full, so the next
    /// write blocks: a bridge that has stopped reading. Returns how many
    /// bytes it took, for a test that wants to read past them later.
    #[cfg(unix)]
    fn fill_until_blocked(stream: &UnixStream) -> usize {
        stream.set_nonblocking(true).expect("non-blocking");
        let chunk = [b'x'; 4096];
        let mut filled = 0;
        loop {
            match (&*stream).write(&chunk) {
                Ok(n) => filled += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("filling the socket failed: {err}"),
            }
        }
        stream.set_nonblocking(false).expect("blocking again");
        filled
    }

    /// Run [`writer_loop`] on its own thread and report when it returns, so
    /// a writer that never does fails the test instead of hanging the suite.
    /// The stream is dropped as the loop returns, which is the EOF a test
    /// reads to know nothing else was written.
    #[cfg(unix)]
    fn spawn_writer(server: UnixStream, shared: &Arc<Shared>) -> Receiver<()> {
        let (tx, rx) = mpsc::channel();
        thread::spawn({
            let shared = Arc::clone(shared);
            move || {
                let mut server = Writer::from(server);
                let alive = AtomicBool::new(true);
                writer_loop(&mut server, &shared, &alive, 0);
                drop(server);
                let _ = tx.send(());
            }
        });
        rx
    }

    /// The exit the flush exists for: the app dequeued an input, refined its
    /// ack and dropped the layer, all before the writer ran again. Both acks
    /// are still queued, and so is the snapshot the app published last.
    #[cfg(unix)]
    #[test]
    fn a_shutdown_writes_the_queued_acks_and_not_the_snapshot() {
        let (server, client) = UnixStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");

        let shared = test_shared("exitflush");
        {
            let mut state = shared.lock_state();
            state.acks.push_back((4.into(), InputStatus::Delivered));
            state.acks.push_back((4.into(), InputStatus::Ignored));
            state.latest = Some(Snapshot::new(1, Node::new("app", Role::App)));
            state.epoch = 1;
            state.shutdown = true;
        }
        let done = spawn_writer(server, &shared);

        let mut reader = BufReader::new(client);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("read a message") == 0 {
                break;
            }
            lines.push(serde_json::from_str::<AppToBridge>(&line).expect("parse a message"));
        }
        assert_eq!(
            lines,
            [
                AppToBridge::ack(InputId::new(4), InputStatus::Delivered),
                AppToBridge::ack(InputId::new(4), InputStatus::Ignored),
            ],
            "every queued ack goes out, in order, and the snapshot behind them does not"
        );
        assert!(
            done.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the writer returns once the acks are out"
        );
        assert!(shared.lock_state().acks.is_empty(), "the queue was taken");
    }

    /// The other place the writer notices a shutdown: partway through a
    /// batch, blocked on a slow bridge. What it has not written of that batch
    /// goes out under the flush's bound, followed by anything queued since,
    /// and the snapshot that was due after the batch is skipped.
    #[cfg(unix)]
    #[test]
    fn a_shutdown_mid_batch_writes_the_rest_of_it_and_what_queued_since() {
        let (server, client) = UnixStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let junk = fill_until_blocked(&server);

        let shared = test_shared("midbatch");
        {
            let mut state = shared.lock_state();
            state.acks.push_back((1.into(), InputStatus::Delivered));
            state.acks.push_back((2.into(), InputStatus::Delivered));
            state.latest = Some(Snapshot::new(1, Node::new("app", Role::App)));
            state.epoch = 1;
        }
        let done = spawn_writer(server, &shared);

        // The writer has the batch once the queue is empty. From there it is
        // blocked writing the first ack into the full socket, or about to be,
        // and either way the next time it looks at the shutdown flag is
        // inside that batch, before one of its acks.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.lock_state().acks.is_empty() {
            assert!(Instant::now() < deadline, "the writer never took the batch");
            thread::sleep(Duration::from_millis(1));
        }
        {
            let mut state = shared.lock_state();
            state.acks.push_back((3.into(), InputStatus::Ignored));
            state.shutdown = true;
            shared.cv.notify_all();
        }

        // Reading frees the socket, which lets the blocked write finish and
        // the writer reach its next check.
        let mut reader = BufReader::new(client);
        let mut skipped = vec![0; junk];
        reader.read_exact(&mut skipped).expect("read past the junk");
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("read a message") == 0 {
                break;
            }
            lines.push(serde_json::from_str::<AppToBridge>(&line).expect("parse a message"));
        }
        assert_eq!(
            lines,
            [
                AppToBridge::ack(InputId::new(1), InputStatus::Delivered),
                AppToBridge::ack(InputId::new(2), InputStatus::Delivered),
                AppToBridge::ack(InputId::new(3), InputStatus::Ignored),
            ],
            "the batch in hand first, then what was queued behind it, and no snapshot"
        );
        assert!(done.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    /// The gap the per-ack check leaves: a shutdown that comes while the
    /// batch's last ack is being written. That ack's check has passed, and
    /// there is no ack after it whose check could notice, so the writer used
    /// to go straight on to the snapshot, the one line a shutdown skips and
    /// the one that can hold `Drop` for a whole `WRITE_TIMEOUT`.
    #[cfg(unix)]
    #[test]
    fn a_shutdown_while_the_last_ack_is_written_never_writes_the_snapshot() {
        let (server, client) = UnixStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let junk = fill_until_blocked(&server);

        let shared = test_shared("lastack");
        {
            let mut state = shared.lock_state();
            state.acks.push_back((1.into(), InputStatus::Delivered));
            state.latest = Some(Snapshot::new(1, Node::new("app", Role::App)));
            state.epoch = 1;
        }
        let done = spawn_writer(server, &shared);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !shared.lock_state().acks.is_empty() {
            assert!(Instant::now() < deadline, "the writer never took the batch");
            thread::sleep(Duration::from_millis(1));
        }
        // Time for the writer to pass the check before its only ack and
        // stall in that ack's write, against the full socket. Should it still
        // be short of the check, that check sees the shutdown instead, and
        // what is asserted below holds all the same.
        thread::sleep(Duration::from_millis(100));
        {
            let mut state = shared.lock_state();
            state.shutdown = true;
            shared.cv.notify_all();
        }

        // Reading frees the socket, which lets the stalled ack finish.
        let mut reader = BufReader::new(client);
        let mut skipped = vec![0; junk];
        reader.read_exact(&mut skipped).expect("read past the junk");
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("read a message") == 0 {
                break;
            }
            lines.push(serde_json::from_str::<AppToBridge>(&line).expect("parse a message"));
        }
        assert_eq!(
            lines,
            [AppToBridge::ack(InputId::new(1), InputStatus::Delivered)],
            "the ack in hand goes out, and the snapshot behind it does not"
        );
        assert!(done.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    /// The flush is bounded in total, not per ack: against a bridge that has
    /// stopped reading, a queue of acks each given the time left would still
    /// be one bound long, and a queue each given `WRITE_TIMEOUT` would hold
    /// the app's exit for minutes.
    #[cfg(unix)]
    #[test]
    fn the_shutdown_flush_gives_up_on_a_bridge_that_stopped_reading() {
        let (server, client) = UnixStream::pair().expect("socket pair");
        fill_until_blocked(&server);
        // As on a live connection, where every write already has this
        // timeout: the flush has to beat it, not lean on it.
        server
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .expect("write timeout");

        let shared = test_shared("stuckflush");
        {
            let mut state = shared.lock_state();
            for id in 0..20 {
                state
                    .acks
                    .push_back((InputId::new(id), InputStatus::Delivered));
            }
            state.shutdown = true;
        }
        let done = spawn_writer(server, &shared);

        // Well short of one `WRITE_TIMEOUT`, so a flush that leaned on it
        // fails here rather than passing slowly.
        let margin = Duration::from_secs(2);
        done.recv_timeout(SHUTDOWN_FLUSH + margin)
            .expect("the flush must give up on a stuck bridge within its bound");
        // Held to here, so the bridge was stuck rather than gone throughout.
        drop(client);
    }

    /// A reader that hands out a scripted sequence of results, for the read
    /// a socket can only produce by timing: an interrupted call, then a
    /// would-block once everything waiting has been read.
    struct Scripted(VecDeque<io::Result<&'static [u8]>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let bytes = self
                .0
                .pop_front()
                .expect("read again after it would have blocked")?;
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    /// What the pre-handshake pass reads and what it counts: every complete
    /// line that is an input, of any kind, and nothing else.
    #[test]
    fn input_written_before_the_hello_is_read_off_and_counted_by_the_line() {
        let script = [
            Ok(&br#"{"type":"input","id":1,"input":{"kind":"key","key":"q"}}"#[..]),
            Err(io::ErrorKind::Interrupted.into()),
            Ok(&b"\nnot json\n"[..]),
            // A kind this build cannot read is still an input, discarded
            // like the rest.
            Ok(&br#"{"type":"input","id":2,"input":{"kind":"paste"}}"#[..]),
            Ok(&b"\n{\"type\":\"bye\"}\n"[..]),
            // Cut off: the reader skips its remainder rather than this
            // counting it.
            Ok(&br#"{"type":"input","id":3,"#[..]),
            Err(io::ErrorKind::WouldBlock.into()),
        ];
        let mut pending = Vec::new();
        read_unsolicited(Scripted(script.into()), &mut pending)
            .expect("an interrupted read is retried, and a would-block ends the pass");
        assert!(
            pending.ends_with(br#""id":3,"#),
            "everything ready was read"
        );
        assert_eq!(count_input_lines(&pending), 2);

        // A peer that closed its end is done too.
        let mut pending = Vec::new();
        read_unsolicited(&b"{}\n"[..], &mut pending).expect("EOF ends the pass");
        assert_eq!(pending, b"{}\n");

        // Up to a line's worth waiting is fine; past it is an oversized line.
        let line = vec![b' '; MAX_LINE_BYTES];
        read_unsolicited(line.as_slice(), &mut Vec::new()).expect("exactly the cap is accepted");
        let flood = vec![b'\n'; MAX_LINE_BYTES + 1];
        let mut pending = Vec::new();
        let err = read_unsolicited(flood.as_slice(), &mut pending)
            .expect_err("more than a line's worth before the hello is refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            pending.len() <= MAX_LINE_BYTES,
            "and nothing past the cap is kept"
        );
    }

    /// The read that runs over the cap is kept up to it, so every complete
    /// line before the cap is counted. Dropping that whole read undercounted
    /// by as many lines as it held whenever the reads did not land on the cap
    /// exactly, which a socket's reads need not.
    #[test]
    fn every_complete_line_before_the_cap_is_counted() {
        let line = b"{\"type\":\"input\",\"id\":1,\"input\":{\"kind\":\"key\",\"key\":\"q\"}}\n";
        let fit = MAX_LINE_BYTES / line.len();
        assert_ne!(
            MAX_LINE_BYTES % line.len(),
            0,
            "the cap falls inside a line"
        );
        // Past the cap by more than one read's worth.
        let flood = line.repeat(fit + 400);
        // A short first read, so that no later one ends on the cap.
        let (first, rest) = flood.split_at(100);
        let mut pending = Vec::new();
        let err = read_unsolicited(first.chain(rest), &mut pending)
            .expect_err("more than a line's worth before the hello is refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(pending.len(), MAX_LINE_BYTES, "kept up to the cap");
        assert_eq!(
            count_input_lines(&pending),
            u64::try_from(fit).expect("fits"),
            "every input that ends before the cap is counted, and the one it cuts is not"
        );
    }

    /// Exit must not depend on the wake-up connection succeeding: with the
    /// socket file gone, nothing can return the listener from `accept()`, and
    /// a join would hang the app forever.
    #[cfg(unix)]
    #[test]
    fn drop_returns_even_when_the_listener_cannot_be_woken() {
        let layer = bind_test_layer("taria-drophang-", "unwakeable");
        fs::remove_file(layer.socket_path()).expect("remove the socket file");

        // Drop off-thread so a regression fails this test instead of hanging
        // the whole suite.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            drop(layer);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "drop must not join a listener that can no longer be woken"
        );
    }

    /// The case the wake-up-by-connecting exit never covered: not the socket
    /// being gone, but a *different* socket at the same path. Running the app
    /// twice does exactly that, and the old exit connected to the path, which
    /// landed on the second instance's listener, counted as having woken this
    /// one, and then joined a thread parked in `accept()` on a socket nothing
    /// could reach. The app hung after restoring the terminal.
    ///
    /// Dropping must also leave that socket alone: it is the other instance's
    /// way of being reached.
    #[cfg(unix)]
    #[test]
    fn drop_neither_hangs_nor_unlinks_when_the_socket_was_replaced() {
        let dir = private_temp_dir("taria-replaced-");
        let path = dir.path().join("replaced.sock");
        let layer = TariaLayer::bind_at("replaced", &path).expect("bind the first layer");

        // What a second instance used to do, done by hand because binding
        // over a live socket is now refused.
        fs::remove_file(&path).expect("remove this layer's socket");
        let usurper = UnixListener::bind(&path).expect("bind another listener at the path");

        // Off-thread so a regression fails this test instead of hanging the
        // whole suite.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            drop(layer);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "drop must not wait on a listener no connection can reach any more"
        );
        assert!(
            path.exists(),
            "the socket at the path belongs to another listener; drop must leave it"
        );
        assert!(
            UnixStream::connect(&path).is_ok(),
            "and it must still be served"
        );
        drop(usurper);
    }

    /// `$TARIA_SOCK` is used verbatim, and a private directory passes the
    /// vetting whatever else is inside it, so a typo in it (`~/.ssh/config`
    /// is 0700 and user-owned) used to mean startup silently deleted a file
    /// the user meant to keep.
    #[cfg(unix)]
    #[test]
    fn binding_never_deletes_something_that_is_not_a_socket() {
        let dir = private_temp_dir("taria-notasocket-");
        let path = dir.path().join("config");
        let content = b"Host example\n  User me\n";
        fs::write(&path, content).expect("create the file that must survive");

        let Err(err) = TariaLayer::bind_at("notasocket", &path) else {
            panic!("binding over a regular file must be refused");
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let message = err.to_string();
        assert!(message.contains("not a socket"), "err: {message}");
        assert!(message.contains("a regular file"), "err: {message}");
        assert_eq!(
            fs::read(&path).expect("the file is still there").as_slice(),
            content,
            "the file at the socket path must be untouched"
        );

        // A symlink is judged as itself, not as whatever it points at.
        let link = dir.path().join("config-link");
        std::os::unix::fs::symlink(&path, &link).expect("create the symlink");
        let Err(err) = TariaLayer::bind_at("notasocket", &link) else {
            panic!("binding over a symlink must be refused");
        };
        assert!(err.to_string().contains("a symlink"), "err: {err}");
        assert!(link.exists(), "the symlink must be left in place");
    }

    /// The case the pre-bind unlink exists for: a socket left behind by a run
    /// that was killed before `Drop` could run. Nothing is listening on it,
    /// and refusing it would leave the app unable to start until somebody
    /// deleted the file by hand.
    #[cfg(unix)]
    #[test]
    fn a_socket_nothing_is_listening_on_is_replaced() {
        let dir = private_temp_dir("taria-stalesock-");
        let path = dir.path().join("stale.sock");
        // Closing a listener does not unlink its socket, which is exactly
        // what SIGKILL leaves behind.
        drop(UnixListener::bind(&path).expect("bind a listener to abandon"));
        assert!(path.exists(), "the abandoned socket file stays behind");

        let layer = TariaLayer::bind_at("stale", &path).expect("a dead socket is replaced");
        assert!(layer.is_enabled());
        assert!(
            UnixStream::connect(&path).is_ok(),
            "the socket bound over the stale one must serve"
        );
    }

    /// Two instances of the same app, which is an ordinary thing to do while
    /// testing a TUI. The second used to unlink the first's socket and bind
    /// its own there, leaving the first running, reporting itself enabled,
    /// and reachable by nobody.
    #[cfg(unix)]
    #[test]
    fn binding_over_a_live_socket_is_refused_and_leaves_it_serving() {
        let dir = private_temp_dir("taria-twice-");
        let path = dir.path().join("twice.sock");
        let first = TariaLayer::bind_at("twice", &path).expect("the first instance binds");

        let Err(err) = TariaLayer::bind_at("twice", &path) else {
            panic!("a second instance must not take over the first's socket");
        };
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        let message = err.to_string();
        assert!(message.contains("already serving"), "err: {message}");
        assert!(
            message.contains("TARIA_SOCK"),
            "the error must say how to run a second instance: {message}"
        );

        // `bind_or_disabled` never fails, so it has to come back disabled
        // rather than pointed at a socket it does not own.
        let second = TariaLayer::bind_or_disabled_at("twice", path.clone());
        assert!(!second.is_enabled(), "the second instance must not be live");
        assert_eq!(
            second.bind_error().map(io::Error::kind),
            Some(io::ErrorKind::AddrInUse),
            "and it must say why"
        );
        drop(second);

        // The first instance is untouched throughout: still bound, still
        // serving, and still the owner of the socket file.
        assert!(first.is_enabled());
        assert!(
            UnixStream::connect(&path).is_ok(),
            "the first instance must still be reachable"
        );
        drop(first);
        assert!(!path.exists(), "the first instance still owns its socket");
    }

    /// The one queue that could grow without bound: inputs are capped and
    /// snapshots coalesce into one latest, but a bridge that reads steadily
    /// and slowly never stalls long enough for the write timeout to
    /// disconnect it while answers pile up behind it.
    #[test]
    fn the_ack_queue_is_bounded_and_drops_the_oldest_answers() {
        // A bound layer rather than bare shared state, so the count is read
        // the way an app reads it, through `stats()`. Nothing connects, so
        // the writer never takes an ack off the queue.
        let layer = bind_test_layer("taria-layer-", "ackbound");
        let shared = &layer.inner.as_ref().expect("a bound layer").shared;
        let overflow = 10;
        for id in 0..(ACK_QUEUE + overflow) as u64 {
            shared.queue_ack(InputId::new(id), InputStatus::Dropped);
        }

        let state = shared.lock_state();
        assert_eq!(state.acks.len(), ACK_QUEUE, "the queue must stay bounded");
        assert_eq!(
            state.acks.front().map(|(id, _)| *id),
            Some(InputId::new(overflow as u64)),
            "the oldest answers are the ones given up; a waiter on those has \
             long since timed out"
        );
        assert_eq!(
            state.acks.back().map(|(id, _)| *id),
            Some(InputId::new((ACK_QUEUE + overflow - 1) as u64)),
            "the newest answer is the one an agent is still waiting on"
        );
        drop(state);
        assert_eq!(
            layer.stats().dropped_acks,
            overflow as u64,
            "each dropped answer must be counted once"
        );
    }

    /// An id names an input only within the bridge process that sent it, and
    /// a fresh bridge counts from the start again. So an ack the app makes
    /// after a reconnect must not be sent: it would resolve the new bridge's
    /// waiter on its own live id with a verdict from a session that never saw
    /// it.
    #[test]
    fn an_app_ack_is_dropped_unless_its_input_belongs_to_the_live_connection() {
        let shared = test_shared("appack");
        shared.lock_state().generation = 7;
        assert!(
            shared.deliver(7, InputId::new(5)),
            "the live connection's input delivers"
        );
        shared.lock_state().acks.clear();

        shared.queue_app_ack(InputId::new(5), InputStatus::Ignored);
        assert_eq!(
            shared.lock_state().acks.pop_front(),
            Some((InputId::new(5), InputStatus::Ignored)),
            "refining the ack of a live input is the whole point of `ack`"
        );

        // The connection ends and the next one starts; its ids start over.
        shared.retire_generation();
        shared.lock_state().generation += 1;
        shared.queue_app_ack(InputId::new(5), InputStatus::Ignored);
        assert!(
            shared.lock_state().acks.is_empty(),
            "an id from a bridge that is gone must answer nobody"
        );

        // And an id falls out of reach once a queue's worth of newer inputs
        // has been delivered past it, which costs a late ack rather than a
        // wrong one.
        // One delivery past what the layer remembers, so exactly the oldest
        // id falls out.
        let generation = shared.lock_state().generation;
        for id in 100..=(100 + ACKABLE_INPUTS as u64) {
            assert!(shared.deliver(generation, InputId::new(id)));
        }
        shared.lock_state().acks.clear();
        shared.queue_app_ack(InputId::new(100), InputStatus::Ignored);
        assert!(shared.lock_state().acks.is_empty(), "id 100 was forgotten");
        shared.queue_app_ack(InputId::new(101), InputStatus::Ignored);
        assert_eq!(
            shared.lock_state().acks.pop_front(),
            Some((InputId::new(101), InputStatus::Ignored)),
            "everything still remembered stays answerable"
        );
    }

    /// A disabled layer must not park an app forever. The enabled path turns
    /// a deadline no clock can hold into a wait for as long as the queue can
    /// deliver; a disabled layer's queue can never deliver, so that wait has
    /// no end, and an app pacing its loop on this call would run bound and
    /// freeze unbound.
    #[test]
    fn a_disabled_layer_does_not_block_forever_on_an_unholdable_timeout() {
        let dir = private_temp_dir("taria-disabledwait-");
        let blocker = dir.path().join("not-a-directory");
        fs::write(&blocker, b"").expect("create blocker file");
        let layer = TariaLayer::bind_or_disabled_at("disabledwait", blocker.join("app.sock"));
        assert!(!layer.is_enabled());

        let started = Instant::now();
        assert_eq!(layer.recv_timeout(Duration::MAX), None);
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(1),
            "a disabled layer waited {waited:?} on a timeout it can never serve"
        );

        // A timeout the clock can hold is still waited out, so an app that
        // paces its loop on this call keeps its timing.
        let started = Instant::now();
        assert_eq!(layer.recv_timeout(Duration::from_millis(50)), None);
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn focus_detection_reaches_every_level_below_the_root() {
        let unfocused = Node::new("a", Role::Pane)
            .focused(true)
            .child(Node::new("b", Role::Text));
        assert!(
            !focused_below(&unfocused),
            "the root's own flag is not below it"
        );

        let deep_focus = Node::new("a", Role::Pane)
            .child(Node::new("b", Role::List).child(Node::new("c", Role::ListItem).focused(true)));
        assert!(focused_below(&deep_focus));
    }
}
