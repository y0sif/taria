//! The [`TariaLayer`]: socket lifecycle, background threads, snapshot
//! publishing, and input acknowledgement for a ratatui app.

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{AgentInput, MAX_NODE_DEPTH, Node, PROTOCOL_VERSION, Role, Snapshot, TreeTooDeep};

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
/// long since timed out. Drops are counted (see [`TariaLayer::dropped_acks`]).
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

/// How long the listener naps between accept attempts.
///
/// The listener socket is non-blocking, so the thread is never parked in
/// `accept()` and shutdown is noticed without anything connecting: [`Drop`]
/// sets the flag and the thread returns within one nap. Waking a blocking
/// `accept()` by connecting to the socket path cannot do that, because the
/// path may by then hold a socket another process bound, whose listener would
/// take the wake-up while this one stayed parked forever.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// How long to wait for the "is anything listening there?" probe that runs
/// before an existing socket file is replaced.
///
/// `connect` is the only liveness probe the standard library offers, and it
/// blocks with no timeout: a socket whose owner stopped accepting with a full
/// backlog would park the app inside `bind`, during startup, which is the one
/// thing taria promises never to do. So the probe runs on a thread of its own
/// and its answer is waited for only this long; no answer in time means
/// "cannot tell", which refuses the bind rather than unlinking a socket that
/// may still be serving.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// The embeddable taria endpoint for a ratatui app.
///
/// Binding a layer opens a Unix domain socket and spawns a listener thread
/// that serves one bridge client at a time: on connect it sends an
/// [`AppToBridge::Hello`] handshake plus the latest snapshot, then streams
/// every newly published snapshot and forwards incoming [`AgentInput`]
/// messages to the app via [`try_recv`](Self::try_recv) /
/// [`recv_timeout`](Self::recv_timeout). Every forwarded input is answered
/// with an [`AppToBridge::Ack`], so an agent can tell an input the app acted
/// on from one it never saw. An input belongs to the connection it arrived
/// on: one whose bridge disconnected before the app dequeued it is discarded
/// rather than applied, because it was aimed at a session that is gone (see
/// [`stale_inputs`](Self::stale_inputs)).
///
/// A layer may also be *disabled*: [`bind_or_disabled`](Self::bind_or_disabled)
/// hands back an inert layer rather than an error, so taria failing to bind
/// can never stop an app from starting. Every method stays callable on a
/// disabled layer, which is what lets an app keep one code path.
///
/// Dropping the layer shuts the threads down and removes the socket file, but
/// only while that file is still the one this layer bound: a second instance
/// that took the path over is serving a socket of its own there, and unlinking
/// it would leave that instance running and unreachable.
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
    input_rx: Receiver<QueuedInput>,
    listener: Option<JoinHandle<()>>,
    /// Device and inode of the socket file as bound, so [`Drop`] can tell it
    /// from a file that replaced it. `None` when it could not be read, which
    /// makes every comparison fail and leaves the path alone.
    socket_ident: Option<(u64, u64)>,
    seq: u64,
    last_root: Option<Node>,
    /// Snapshots published with a branch cut for depth, and the failure
    /// behind the newest cut. Owned here rather than in [`Shared`] because
    /// only the app thread publishes and only the app thread reads these.
    truncated: u64,
    last_truncation: Option<TreeTooDeep>,
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
        // Free the path if a dead socket is holding it, and refuse the bind
        // if anything else is.
        clear_socket_path(&socket_path)?;

        let listener = UnixListener::bind(&socket_path)?;
        // Polled rather than parked in `accept()`, so shutdown needs no
        // connection to be noticed; see `ACCEPT_POLL`.
        if let Err(err) = listener.set_nonblocking(true) {
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            return Err(err);
        }
        // Identify the socket just bound, before anyone else can replace it.
        let socket_ident = socket_ident(&socket_path);
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
                socket_ident,
                seq: 0,
                last_root: None,
                truncated: 0,
                last_truncation: None,
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

    /// [`bind_or_disabled`](Self::bind_or_disabled) at an explicit path, so
    /// the disabled branch can be exercised without mutating the process
    /// environment (which no test can do safely while other tests run).
    fn bind_or_disabled_at(app_label: &str, socket_path: PathBuf) -> Self {
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
    /// Inputs left over from a bridge connection that has since ended are
    /// discarded here rather than handed over; see
    /// [`stale_inputs`](Self::stale_inputs).
    ///
    /// [`AgentInput::Unknown`] never arrives here either. An input whose
    /// `kind` this build cannot read carries nothing to apply, so the layer
    /// acks it [`Ignored`](InputStatus::Ignored) where it is parsed and never
    /// queues it; see [`unknown_inputs`](Self::unknown_inputs).
    ///
    /// Always `None` on a disabled layer.
    pub fn try_recv(&self) -> Option<AgentInput> {
        self.try_recv_with_id().map(|(_, input)| input)
    }

    /// Wait up to `timeout` for the next agent input.
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
        loop {
            let queued = inner.input_rx.try_recv().ok()?;
            if inner.shared.deliver(queued.generation, queued.id) {
                return Some((queued.id, queued.input));
            }
        }
    }

    /// [`recv_timeout`](Self::recv_timeout), keeping the [`InputId`] so the
    /// app can answer the input again later with [`ack`](Self::ack).
    pub fn recv_timeout_with_id(&self, timeout: Duration) -> Option<(InputId, AgentInput)> {
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
    ///
    /// Reach for [`drain_acking`](Self::drain_acking) first: an app that only
    /// wants to refine an ack needs the id for nothing else, and every
    /// integration that wrote this loop by hand wrote the same four lines
    /// around it.
    pub fn drain_with_ids(&self, mut f: impl FnMut(InputId, AgentInput)) {
        while let Some((id, input)) = self.try_recv_with_id() {
            f(id, input);
        }
    }

    /// [`drain`](Self::drain) where the handler returns each input's status
    /// and the layer sends the ones that say something new.
    ///
    /// Answering an input the app looked at and deliberately did nothing with
    /// is the whole reason [`drain_with_ids`](Self::drain_with_ids) hands out
    /// an [`InputId`], and two independent apps wrote the same
    /// dequeue-compare-[`ack`](Self::ack) loop around it, each with its own
    /// two-variant verdict enum in front. Returning
    /// [`Ignored`](InputStatus::Ignored) says the same thing with no id in the
    /// app's hands and no enum of its own.
    ///
    /// Return [`Delivered`](InputStatus::Delivered) for an input the app
    /// acted on. It sends nothing: the dequeue already acked it, and
    /// re-sending would double this layer's ack traffic, which on a bridge
    /// reading slower than the app answers costs the refinements queued
    /// behind it (see [`dropped_acks`](Self::dropped_acks)).
    ///
    /// `drain_with_ids` stays for an app that needs the id for something
    /// else, such as an input it can only answer after a later frame.
    pub fn drain_acking(&self, mut f: impl FnMut(AgentInput) -> InputStatus) {
        while let Some((id, input)) = self.try_recv_with_id() {
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
    /// The ack travels the same path as snapshots, so it can never overtake
    /// the snapshot published after it. Does nothing on a disabled layer.
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

    /// How many agent inputs have been discarded so far because the bridge
    /// connection they arrived on ended before the app dequeued them. Always
    /// 0 on a disabled layer.
    ///
    /// Such an input was aimed at a bridge session that no longer exists:
    /// applying it would act on an agent's intent minutes or milliseconds
    /// after the agent that formed it is gone, with nobody left to receive
    /// the result. A `key q` that arrives just before the bridge restarts
    /// would otherwise quit the app on the next frame. The bridge drops its
    /// own queue on reconnect for the mirror-image reason, so both sides
    /// agree that an input belongs to one connection.
    ///
    /// Monotonic across reconnects, and read like
    /// [`dropped_inputs`](Self::dropped_inputs): after the terminal is
    /// restored, never while the app owns the alternate screen.
    pub fn stale_inputs(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.shared.stale_inputs.load(Ordering::Relaxed))
    }

    /// How many agent inputs have been answered without reaching the app
    /// because their `kind` is one this build cannot read. Always 0 on a
    /// disabled layer.
    ///
    /// An input like that parses as [`AgentInput::Unknown`], which carries
    /// neither the kind it arrived under nor what it asked for, so there is
    /// nothing for the app to apply. The layer acks it
    /// [`Ignored`](InputStatus::Ignored) itself and never queues it; see
    /// [`try_recv`](Self::try_recv), which is why an app never has to think
    /// about the variant.
    ///
    /// A nonzero value means the bridge on the other end is built against a
    /// newer taria than this app, and the agent is asking for things this app
    /// cannot do yet. Raising the app's taria dependency is the fix. Read it
    /// like [`dropped_inputs`](Self::dropped_inputs): after the terminal is
    /// restored, never while the app owns the alternate screen.
    pub fn unknown_inputs(&self) -> u64 {
        self.inner.as_ref().map_or(0, |inner| {
            inner.shared.unknown_inputs.load(Ordering::Relaxed)
        })
    }

    /// How many acks have been dropped so far because more answers were
    /// waiting for the bridge than the queue holds. Always 0 on a disabled
    /// layer.
    ///
    /// Acks queue for the writer thread, and a bridge that reads steadily but
    /// far slower than an agent sends input never stalls long enough for the
    /// write timeout to disconnect it. The queue is bounded so the app cannot
    /// be grown out of memory by such a peer; the oldest answers go first,
    /// because the ones an agent is still waiting on are the newest.
    ///
    /// A nonzero value means some agent requests were answered nowhere and
    /// their callers waited out a timeout instead. The slow end is the bridge,
    /// not the app. Monotonic across reconnects, and read like
    /// [`dropped_inputs`](Self::dropped_inputs): after the terminal is
    /// restored, never while the app owns the alternate screen.
    pub fn dropped_acks(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.shared.dropped_acks.load(Ordering::Relaxed))
    }

    /// How many published snapshots have had a branch cut for being deeper
    /// than [`taria::MAX_NODE_DEPTH`]. Always 0 on a disabled layer.
    ///
    /// See [`publish`](Self::publish) for what the cut does and why.
    /// [`last_truncation`](Self::last_truncation) names the branch. Monotonic
    /// for the lifetime of the layer, and read like
    /// [`dropped_inputs`](Self::dropped_inputs): after the terminal is
    /// restored, never while the app owns the alternate screen.
    pub fn truncated_snapshots(&self) -> u64 {
        self.inner.as_ref().map_or(0, |inner| inner.truncated)
    }

    /// The depth failure behind the most recent truncation, or `None` if no
    /// published tree has been cut.
    ///
    /// [`TreeTooDeep`](taria::TreeTooDeep) carries the depth measured and the
    /// id of a node found at it. An app whose tree is generated from data has
    /// no other way to tell which branch ran away, which is the whole reason
    /// the counter alone is not enough here.
    pub fn last_truncation(&self) -> Option<TreeTooDeep> {
        self.inner.as_ref()?.last_truncation.clone()
    }

    /// Publish `nodes` as a snapshot, without going through a
    /// [`FrameRecorder`].
    ///
    /// For apps that build their node list separately from drawing, where the
    /// recorder's frame, push-loop and publish triplet is three lines saying
    /// one thing. The nodes are wrapped in the same auto-generated root, so
    /// both paths produce the same tree.
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
    /// that can fix it: see
    /// [`truncated_snapshots`](Self::truncated_snapshots) for the count and
    /// [`last_truncation`](Self::last_truncation) for the branch. The fix is
    /// almost always to publish what the widget draws rather than the data
    /// behind it, which for a deep tree view is the expanded path and the
    /// rows currently on screen.
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
    ///
    /// A tree over [`MAX_NODE_DEPTH`] is cut before any of that; see
    /// [`publish`](Self::publish).
    pub(crate) fn publish_nodes(&mut self, nodes: Vec<Node>) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        // The root added here counts towards the depth, so the check has to
        // run on the assembled tree rather than on the caller's nodes, and
        // before focus is decided: cutting a branch can remove the only
        // focused node, and a snapshot has to carry focus somewhere.
        let mut root = Node::new("app", Role::App)
            .label(inner.shared.app_label.clone())
            .children(nodes);
        if let Err(too_deep) = root.check_depth() {
            truncate_to_max_depth(&mut root);
            inner.truncated += 1;
            inner.last_truncation = Some(too_deep);
        }
        let focus_recorded = root.children.iter().any(subtree_has_focus);
        let root = root.focused(!focus_recorded);

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
    ///
    /// How long this takes depends only on what the threads are doing, never
    /// on what is at the socket path. The listener polls the shutdown flag
    /// rather than waiting to be woken by a connection, so it returns whether
    /// the socket file was replaced by a second instance, removed, or is
    /// being served by some other process; the worst case is one
    /// `ACCEPT_POLL` nap, plus one `WRITE_TIMEOUT` if a connected bridge
    /// has stopped reading mid-write.
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        {
            let mut state = inner.shared.lock_state();
            state.shutdown = true;
            inner.shared.cv.notify_all();
        }
        if let Some(handle) = inner.listener.take() {
            let _ = handle.join();
        }
        remove_own_socket(&self.socket_path, inner.socket_ident);
    }
}

/// Unlink the socket file, but only while it is still the one this layer
/// bound.
///
/// A second instance that started later has a socket of its own at the same
/// path, and unlinking that on the way out would leave it running with no way
/// for a bridge to reach it (the mirror of the takeover `clear_socket_path`
/// refuses). Device and inode are what separate the file this layer created
/// from a file that merely has the same name.
fn remove_own_socket(path: &Path, bound: Option<(u64, u64)>) {
    if bound.is_some() && socket_ident(path) == bound {
        let _ = fs::remove_file(path);
    }
}

/// Identify the file at `path` by device and inode, so a later look can tell
/// the same file from a different one that took its place. `None` when it
/// cannot be read, which fails every comparison and so leaves the path alone.
fn socket_ident(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::symlink_metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// State shared between the app thread and the connection threads.
struct Shared {
    app_label: String,
    state: Mutex<State>,
    cv: Condvar,
    /// Agent inputs dropped because the input queue was full. Written by
    /// reader threads, read via [`TariaLayer::dropped_inputs`].
    dropped_inputs: AtomicU64,
    /// Agent inputs discarded at dequeue because the connection they arrived
    /// on had ended. Written by the app thread, read via
    /// [`TariaLayer::stale_inputs`].
    stale_inputs: AtomicU64,
    /// Agent inputs answered [`Ignored`](InputStatus::Ignored) by the reader
    /// without reaching the app, because their `kind` did not parse. Written
    /// by reader threads, read via [`TariaLayer::unknown_inputs`].
    unknown_inputs: AtomicU64,
    /// Acks dropped because [`ACK_QUEUE`] answers were already waiting for a
    /// bridge that is not reading them fast enough. Written by whichever
    /// thread queues an ack, read via [`TariaLayer::dropped_acks`].
    dropped_acks: AtomicU64,
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

    /// Answer an input the app just dequeued, or refuse it as stale.
    ///
    /// Returns true after queueing the [`Delivered`](InputStatus::Delivered)
    /// ack for an input whose connection is still live. Returns false for an
    /// input from a retired generation: the caller must discard it instead of
    /// acting on it, because it was aimed at a bridge session that no longer
    /// exists. A discard is counted (see [`TariaLayer::stale_inputs`]) and
    /// deliberately not acked, since the peer that would read the ack is the
    /// one that is gone.
    fn deliver(&self, generation: u64, id: InputId) -> bool {
        let mut state = self.lock_state();
        if state.generation != generation {
            drop(state);
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
}

/// Does `node` or any of its descendants have focus?
fn subtree_has_focus(node: &Node) -> bool {
    node.focused || node.children.iter().any(subtree_has_focus)
}

/// Drop the children of every node at [`MAX_NODE_DEPTH`], so no node of the
/// tree rooted at `root` is deeper than the limit.
///
/// Walks with a vector for the reason [`Node::check_depth`] does: the input
/// this exists for is a tree deeper than the call stack, and a recursive walk
/// would overflow on exactly that. Only the branches that run over are
/// touched; the rest of the tree keeps every node it had.
fn truncate_to_max_depth(root: &mut Node) {
    let mut pending = vec![(root, 1usize)];
    while let Some((node, level)) = pending.pop() {
        if level >= MAX_NODE_DEPTH {
            node.children.clear();
            continue;
        }
        pending.extend(node.children.iter_mut().map(|child| (child, level + 1)));
    }
}

/// Accept loop: serves one bridge client at a time until shutdown.
///
/// The listener is non-blocking, so this thread is never parked in `accept()`
/// and the shutdown flag is checked on its own schedule; see [`ACCEPT_POLL`]
/// for why exit cannot be left to depend on a connection arriving.
fn accept_loop(listener: UnixListener, shared: Arc<Shared>, input_tx: SyncSender<QueuedInput>) {
    loop {
        if shared.is_shutdown() {
            return;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                if shared.is_shutdown() {
                    return;
                }
                // Both halves of a session are read and written as blocking
                // streams, and an accepted socket inherits the listener's
                // non-blocking flag on some platforms. Say it outright rather
                // than rely on which one this is.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                // Errors just drop this client; the loop keeps accepting.
                let _ = serve_client(stream, &shared, &input_tx);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                // Nothing waiting to be accepted: nap instead of spinning,
                // then back to the shutdown check at the top.
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => {
                if shared.is_shutdown() {
                    return;
                }
                // Avoid a hot loop on a persistently failing listener.
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

/// Serve one connected bridge: handshake, then reader thread + writer loop
/// until the client disconnects or the layer shuts down.
fn serve_client(
    stream: UnixStream,
    shared: &Arc<Shared>,
    input_tx: &SyncSender<QueuedInput>,
) -> io::Result<()> {
    // SO_SNDTIMEO is shared across the duplicated fds; only writes block long
    // enough to need it.
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut write_stream = stream.try_clone()?;

    // Capture the snapshot and epoch atomically so the writer loop neither
    // misses nor duplicates a publish that races the handshake. Acks left
    // over from the previous connection go here: they name ids this client
    // never used. The generation taken here tags every input this connection
    // queues, so an input that outlives it is discarded rather than applied
    // to the app on behalf of a bridge that is gone.
    let (initial_snapshot, initial_epoch, generation) = {
        let mut state = shared.lock_state();
        state.acks.clear();
        state.generation += 1;
        (state.latest.clone(), state.epoch, state.generation)
    };
    let alive = Arc::new(AtomicBool::new(true));
    let reader = match start_connection(
        &mut write_stream,
        stream,
        shared,
        input_tx,
        initial_snapshot,
        &alive,
        generation,
    ) {
        Ok(reader) => reader,
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

    writer_loop(&mut write_stream, shared, &alive, initial_epoch);

    // Unblock the reader (it is parked in a blocking read) and reap it before
    // going back to accept the next client.
    let _ = write_stream.shutdown(Shutdown::Both);
    let _ = reader.join();
    // Whatever is still queued can no longer be delivered, and means nothing
    // to the next client. Queued inputs need no sweep here: the reader
    // retired their generation, so the app discards them as it dequeues.
    shared.clear_acks();
    Ok(())
}

/// Send the handshake (and the snapshot a new client is owed) and start the
/// connection's reader thread, so every way of failing to get a connection
/// off the ground funnels through one `Err` for the caller to clean up after.
fn start_connection(
    write_stream: &mut UnixStream,
    stream: UnixStream,
    shared: &Arc<Shared>,
    input_tx: &SyncSender<QueuedInput>,
    initial_snapshot: Option<Snapshot>,
    alive: &Arc<AtomicBool>,
    generation: u64,
) -> io::Result<JoinHandle<()>> {
    write_line(
        write_stream,
        &AppToBridge::hello(shared.app_label.clone(), PROTOCOL_VERSION),
    )?;
    if let Some(snapshot) = initial_snapshot {
        write_line(write_stream, &AppToBridge::Snapshot(snapshot))?;
    }
    thread::Builder::new().name("taria-reader".into()).spawn({
        let alive = Arc::clone(alive);
        let shared = Arc::clone(shared);
        let input_tx = input_tx.clone();
        move || reader_loop(stream, input_tx, alive, shared, generation)
    })
}

/// Reader half of a connection: parse `BridgeToApp` lines and forward agent
/// inputs to the app. Malformed lines are ignored. A line longer than
/// [`MAX_LINE_BYTES`] marks the connection broken (the loop exits and the
/// client is disconnected) instead of buffering it. When the input queue is
/// full the newest input is dropped, acked [`Dropped`](InputStatus::Dropped)
/// so the agent learns of it, and counted for the app (see
/// [`TariaLayer::dropped_inputs`]); a flood of inputs can never block this
/// thread.
///
/// Every forwarded input carries `generation`, which this loop retires when
/// the connection ends so anything still queued from it is discarded instead
/// of delivered.
fn reader_loop(
    stream: UnixStream,
    input_tx: SyncSender<QueuedInput>,
    alive: Arc<AtomicBool>,
    shared: Arc<Shared>,
    generation: u64,
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
            // Rechecked per ack rather than once per pass: a full batch
            // against a peer that stopped reading is a whole queue of write
            // timeouts long, and `Drop` waits for this thread, so an exit
            // during the flush would sit through every one of them.
            if shared.is_shutdown() || !alive.load(Ordering::SeqCst) {
                return;
            }
            if write_line(stream, &AppToBridge::ack(id, status)).is_err() {
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

/// Make `path` free for `bind`, or say why it cannot be.
///
/// Binding needs the path to be absent, so a socket left behind by a run that
/// was killed before it could clean up has to be unlinked first. That unlink
/// has to be earned. The path can come verbatim from `$TARIA_SOCK`, and a
/// private directory passes the vetting whatever else is in it (`~/.ssh` is
/// 0700 and user-owned), so removing whatever happens to be there would let a
/// typo delete a file the user meant to keep. A socket another instance is
/// still serving must survive too: unlinking it and binding a fresh one in its
/// place leaves that instance running, believing it is reachable, while every
/// bridge connects to the newcomer instead.
///
/// So: only a socket is ever removed, and only once a connection to it has
/// been refused.
fn clear_socket_path(path: &Path) -> io::Result<()> {
    // symlink_metadata so a symlink is seen as itself: following one would
    // decide about a socket somewhere else and unlink the link.
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("cannot inspect {}: {err}", path.display()),
            ));
        }
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists and is not a socket (it is {}); refusing to delete it to \
                 bind there. Point $TARIA_SOCK at a path taria may create.",
                path.display(),
                describe_file_type(&meta)
            ),
        ));
    }
    match probe_socket(path) {
        // Nothing is listening: the socket outlived whatever bound it.
        SocketProbe::Stale => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            // Someone else cleaned it up in the meantime, which is the state
            // this call wanted anyway.
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io::Error::new(
                err.kind(),
                format!("cannot remove the stale socket {}: {err}", path.display()),
            )),
        },
        SocketProbe::Live => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another instance is already serving the taria socket {}; refusing to take it \
                 over, because that would leave it running with no way for a bridge to reach \
                 it. To run a second instance alongside it, give it a socket of its own: set \
                 TARIA_SOCK to a different path.",
                path.display()
            ),
        )),
        SocketProbe::Unknown(why) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "cannot tell whether another instance is serving the taria socket {} ({why}); \
                 refusing to remove a socket that may still be in use. Set TARIA_SOCK to a \
                 different path to bind elsewhere.",
                path.display()
            ),
        )),
    }
}

/// What is at a socket path that is not a socket, for the error that refuses
/// to delete it.
fn describe_file_type(meta: &fs::Metadata) -> &'static str {
    let kind = meta.file_type();
    if kind.is_symlink() {
        "a symlink"
    } else if kind.is_dir() {
        "a directory"
    } else if kind.is_file() {
        "a regular file"
    } else {
        "another kind of file"
    }
}

/// The answer to "is anything serving the socket at this path?".
enum SocketProbe {
    /// The connection was accepted: something is listening.
    Live,
    /// The connection was refused, or the file went away: nothing is.
    Stale,
    /// The probe could not answer, carrying the reason for the error that
    /// reports it.
    Unknown(String),
}

/// Try to connect to the socket at `path`, briefly, to find out whether
/// anything is listening on it.
///
/// The connect runs on a thread of its own because it blocks with no timeout;
/// see [`SOCKET_PROBE_TIMEOUT`]. An answer that does not arrive in time is
/// [`Unknown`](SocketProbe::Unknown), never a guess: the whole point is to
/// avoid unlinking a socket that is still being served.
fn probe_socket(path: &Path) -> SocketProbe {
    let (tx, rx) = mpsc::channel();
    let probe_path = path.to_path_buf();
    let spawned = thread::Builder::new()
        .name("taria-socket-probe".into())
        .spawn(move || {
            let outcome = UnixStream::connect(&probe_path).map(|stream| {
                // Nothing is ever sent on it. A peer serving this path sees a
                // connection that opens and closes, which is what it does
                // with any client that goes away.
                let _ = stream.shutdown(Shutdown::Both);
            });
            // The receiver is gone once the wait above times out; that is the
            // `Unknown` case, and there is nothing to report to.
            let _ = tx.send(outcome);
        });
    if let Err(err) = spawned {
        return SocketProbe::Unknown(format!("the probe thread could not start: {err}"));
    }
    match rx.recv_timeout(SOCKET_PROBE_TIMEOUT) {
        Ok(Ok(())) => SocketProbe::Live,
        // Refused is what a socket with no listener answers; a socket that
        // vanished between the look and the connect leaves the path free too.
        Ok(Err(err))
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            SocketProbe::Stale
        }
        Ok(Err(err)) => SocketProbe::Unknown(format!("connecting to it failed: {err}")),
        Err(_) => SocketProbe::Unknown(format!(
            "connecting to it did not finish within {SOCKET_PROBE_TIMEOUT:?}"
        )),
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
/// bridge so the two sides cannot look for the socket in different places,
/// including its refusal of a label that is not a plain file name. Reading the
/// environment stays here, where the process actually is.
fn resolve_socket_path(app_label: &str) -> Result<PathBuf, taria::socket::InvalidAppLabel> {
    taria::socket::resolve_path(
        env::var_os("TARIA_SOCK"),
        env::var_os("XDG_RUNTIME_DIR"),
        &env::temp_dir(),
        &user_identity(),
        app_label,
    )
}

/// A refused app label as the `io::Error` every bind path already reports.
///
/// `InvalidInput` because the label is one: the same kind
/// [`bind_at`](TariaLayer::bind_at) gives an over-long path, and the message
/// is the label error's own, which names the label and what a label may be.
fn invalid_label(err: taria::socket::InvalidAppLabel) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
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
        assert_eq!(layer.dropped_inputs(), 0);
        assert_eq!(layer.stale_inputs(), 0);
        assert_eq!(layer.unknown_inputs(), 0);
        assert_eq!(layer.truncated_snapshots(), 0);
        assert_eq!(layer.last_truncation(), None);

        assert_eq!(layer.try_recv(), None);
        assert_eq!(layer.try_recv_with_id(), None);
        assert_eq!(layer.recv_timeout(Duration::from_millis(1)), None);
        assert_eq!(layer.recv_timeout_with_id(Duration::from_millis(1)), None);
        layer.ack(0, InputStatus::Ignored);

        let mut drained = 0;
        layer.drain(|_| drained += 1);
        layer.drain_with_ids(|_, _| drained += 1);
        layer.drain_acking(|_| {
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
        assert_eq!(layer.truncated_snapshots(), 0);
        assert_eq!(layer.last_truncation(), None);
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
        assert_eq!(layer.truncated_snapshots(), 1);
        let too_deep = layer.last_truncation().expect("the cut is recorded");
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
        assert_eq!(layer.truncated_snapshots(), 1);
        assert!(
            root.focused,
            "the focused node was cut, so the root has to carry focus"
        );
        assert!(!subtree_has_focus_below_root(&root));
    }

    /// Focus anywhere under the root, which is what the root's own focus flag
    /// is the negation of.
    fn subtree_has_focus_below_root(root: &Node) -> bool {
        root.children.iter().any(subtree_has_focus)
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
        assert_eq!(layer.truncated_snapshots(), 1);
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

    /// The ordering [`writer_loop`] promises, exercised where it actually
    /// bites: an ack and a newer snapshot pending in the *same* pass. Driving
    /// the loop directly over a socket pair is the only way to make that
    /// overlap certain; through a live connection the writer wakes on the
    /// first of the two and usually drains it before the second exists, so
    /// the socket tests only ever see the two in separate passes.
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
            state.acks.push_back((7, InputStatus::Delivered));
            state.latest = Some(Snapshot::new(1, Node::new("app", Role::App)));
            state.epoch = 1;
        }

        let alive = Arc::new(AtomicBool::new(true));
        let writer = thread::spawn({
            let shared = Arc::clone(&shared);
            let alive = Arc::clone(&alive);
            move || {
                let mut server = server;
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
            AppToBridge::ack(7, InputStatus::Delivered),
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

    /// Exit must not depend on the wake-up connection succeeding: with the
    /// socket file gone, nothing can return the listener from `accept()`, and
    /// a join would hang the app forever.
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
        let shared = test_shared("ackbound");
        let overflow = 10;
        for id in 0..(ACK_QUEUE + overflow) as InputId {
            shared.queue_ack(id, InputStatus::Dropped);
        }

        let state = shared.lock_state();
        assert_eq!(state.acks.len(), ACK_QUEUE, "the queue must stay bounded");
        assert_eq!(
            state.acks.front().map(|(id, _)| *id),
            Some(overflow as InputId),
            "the oldest answers are the ones given up; a waiter on those has \
             long since timed out"
        );
        assert_eq!(
            state.acks.back().map(|(id, _)| *id),
            Some((ACK_QUEUE + overflow - 1) as InputId),
            "the newest answer is the one an agent is still waiting on"
        );
        drop(state);
        assert_eq!(
            shared.dropped_acks.load(Ordering::Relaxed),
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
        assert!(shared.deliver(7, 5), "the live connection's input delivers");
        shared.lock_state().acks.clear();

        shared.queue_app_ack(5, InputStatus::Ignored);
        assert_eq!(
            shared.lock_state().acks.pop_front(),
            Some((5, InputStatus::Ignored)),
            "refining the ack of a live input is the whole point of `ack`"
        );

        // The connection ends and the next one starts; its ids start over.
        shared.retire_generation();
        shared.lock_state().generation += 1;
        shared.queue_app_ack(5, InputStatus::Ignored);
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
        for id in 100..=(100 + ACKABLE_INPUTS as InputId) {
            assert!(shared.deliver(generation, id));
        }
        shared.lock_state().acks.clear();
        shared.queue_app_ack(100, InputStatus::Ignored);
        assert!(shared.lock_state().acks.is_empty(), "id 100 was forgotten");
        shared.queue_app_ack(101, InputStatus::Ignored);
        assert_eq!(
            shared.lock_state().acks.pop_front(),
            Some((101, InputStatus::Ignored)),
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
        assert_eq!(layer.recv_timeout_with_id(Duration::MAX), None);
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
    fn focus_detection_is_recursive() {
        let unfocused = Node::new("a", Role::Pane).child(Node::new("b", Role::Text));
        assert!(!subtree_has_focus(&unfocused));

        let deep_focus = Node::new("a", Role::Pane)
            .child(Node::new("b", Role::List).child(Node::new("c", Role::ListItem).focused(true)));
        assert!(subtree_has_focus(&deep_focus));
    }
}
