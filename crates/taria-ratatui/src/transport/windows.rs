//! The Windows transport: one named pipe instance, served to one bridge at a
//! time and reused from one session to the next.
//!
//! # The pipe
//!
//! [`Listener::bind`] creates the pipe with nothing but wide-character (`W`)
//! calls, which take the name exactly as
//! [`check_pipe_name`](taria::socket::check_pipe_name) vetted it: an `A` call
//! would pass it through the ANSI code page first, whose best-fit mapping can
//! turn some Unicode characters into a `\` that check never saw. The pipe is
//! created as the first and only instance of its name
//! (`FILE_FLAG_FIRST_PIPE_INSTANCE`, one instance at most), refuses clients on
//! other machines (`PIPE_REJECT_REMOTE_CLIENTS`), is a byte stream both ways,
//! and is owned by the current user with a DACL that lets only that user in
//! (see [`security::PipeSecurity`]). A second app binding the same name is
//! refused by the system, which is what makes binding over a live endpoint
//! fail as it does on Unix.
//!
//! # Handles and overlapped operations
//!
//! The pipe is opened for overlapped I/O, because the session's reader and
//! writer use the one handle at the same time, each from its own thread, and a
//! handle opened for synchronous I/O lets only one call at a time through. The
//! handle is shared through an [`Arc`] by the listener and by everything a
//! session is made of, and closed once, when the last of them is dropped.
//!
//! Every operation on it runs through an [`Op`]: an OVERLAPPED, the event it
//! signals and the buffer the kernel reads from or writes into, boxed so they
//! never move. An operation is started and finished inside one call on the
//! `Op`'s owner, so none is ever in flight once that call has returned.
//! Finishing means the kernel has said so: a wait that gives up (a deadline,
//! a shutdown) cancels the operation and then waits for it to finish, and
//! only then is its memory used again or freed. An `Op` dropped with an
//! operation still in flight, which only a panic could cause, does the same,
//! and leaks its memory rather than free it if even that fails.
//!
//! # Sessions
//!
//! A [`Session`] is one bridge: the [`Connection`] the listener hands over,
//! and the [`Reader`] and [`Writer`] it splits into. They share a closed
//! event, set once by [`Writer::shutdown`] and never reset. Every read and
//! every write checks it before it starts and waits on it beside the
//! operation's own event, so a shutdown wakes a blocked reader at once and a
//! read started after it ends at once, which cancelling the read alone would
//! not do: a cancel only reaches the operation in flight when it is made.
//!
//! The next [`Listener::accept`] retires the session: it sets the closed
//! event, waits until nothing of the session is left, and only then
//! disconnects the client (`DisconnectNamedPipe`) and listens again. Data the
//! client has not read is discarded by that disconnect, and that is the only
//! place it happens. At shutdown nothing disconnects: the handle is closed,
//! which leaves what the writer already wrote, the acks flushed at shutdown
//! included, readable by the bridge until it has read it all.

use std::cell::UnsafeCell;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{Ordering, fence};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, STATUS_PENDING, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, PeekNamedPipe,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

#[path = "windows_security.rs"]
mod security;

use super::Accepted;
use security::{PipeSecurity, Sid};

/// How long the listener waits for a client in one go before it looks at the
/// layer's shutdown flag again, the counterpart of the Unix transport's
/// `ACCEPT_POLL`. A layer dropped with no bridge connected waits at most this
/// long for the listener (plus the system timer's resolution, about 16 ms by
/// default on Windows).
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// The size the pipe's buffers are asked for, each way. Advisory: the system
/// may round it. A write that does not fit waits for the bridge to read,
/// which is what the write deadline bounds.
const PIPE_BUFFER: u32 = 64 * 1024;

/// The most one read asks for, and the size of the reader's own buffer. The
/// layer reads through a `BufReader` of the same size, so one read fills it.
const READ_CHUNK: usize = 8 * 1024;

/// The most one write hands the kernel. Larger than any line the layer
/// writes, a snapshot of `MAX_LINE_BYTES` included, so a line is one write;
/// the cap only keeps the length within what `WriteFile` takes and bounds the
/// writer's copy of it.
const MAX_WRITE: usize = 4 * 1024 * 1024;

/// A kernel handle this module owns: the pipe, an event, a token. Closed
/// exactly once, when dropped.
struct Handle(HANDLE);

// SAFETY: a kernel handle is an index into the process's handle table, not
// tied to the thread that opened it. The pipe is used from the listener, the
// reader and the writer threads, each operation with an OVERLAPPED of its own,
// which overlapped I/O is made for; an event may be set and waited on from any
// thread.
unsafe impl Send for Handle {}
// SAFETY: as above. Every call made through a shared `Handle` (I/O with an
// OVERLAPPED of its caller's, `SetEvent`, waits) is safe to make from several
// threads at once.
unsafe impl Sync for Handle {}

impl Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value, valid, and closed only
        // here. A pipe handle is closed only once no operation is in flight on
        // it: every one runs inside a call on an owner that holds the handle's
        // `Arc` (see `Op`).
        unsafe { CloseHandle(self.0) };
    }
}

/// The pipe instance, shared by the listener and the session it serves.
type Pipe = Arc<Handle>;

/// A manual-reset event, unset, with no name and default security.
fn event() -> io::Result<Handle> {
    // SAFETY: no attributes and no name; the handle returned is owned by the
    // `Handle` it is put in.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(event))
}

/// The kinds of operation an [`Op`] starts.
#[derive(Clone, Copy)]
enum Kind {
    /// `ConnectNamedPipe`: wait for a client.
    Connect,
    /// `ReadFile` into the op's buffer.
    Read,
    /// `WriteFile` from the op's buffer.
    Write,
}

/// What a wait on an [`Op`] woke for.
enum Woke {
    /// The operation finished; [`Op::finish`] collects its result.
    Finished,
    /// The session's closed event was set first.
    Closed,
    /// The time ran out first.
    TimedOut,
}

/// The kernel-facing memory of an [`Op`].
struct OpState {
    /// Written by the kernel while an operation is in flight, so it lives in
    /// a cell and is only ever reached through a raw pointer while it is.
    overlapped: UnsafeCell<OVERLAPPED>,
    /// The event the operation signals when it finishes. Manual-reset, and
    /// reset before every start.
    event: Handle,
    /// What a read reads into, or a write writes from. Not touched while an
    /// operation is in flight, and its length is never changed then.
    buf: Vec<u8>,
}

/// One overlapped operation at a time on the pipe.
///
/// # Invariants
///
/// - The OVERLAPPED, the event and the buffer are boxed in an [`OpState`], so
///   the addresses the kernel holds stay put however the `Op` moves.
/// - `in_flight` is set from a start the kernel took (pending, or finished at
///   once) until [`finish`](Self::finish) has seen the operation finish.
///   While it is set the state is only reached through shared references and
///   raw pointers, and nothing in it is freed, reused or rewritten.
/// - Every caller finishes or cancels what it starts before it returns, so an
///   operation never outlives the call that started it.
/// - Dropped with an operation in flight, it cancels it and waits for it to
///   finish; if even that fails, the state is leaked, never freed.
struct Op {
    pipe: Pipe,
    /// `None` only once the state has been leaked, after which every
    /// operation fails.
    state: Option<Box<OpState>>,
    in_flight: bool,
}

// SAFETY: an `Op` is used by one thread at a time (its owner's) and moved
// between threads only with nothing in flight, or with an operation that the
// thread it moves to then finishes. The OVERLAPPED holds integers and an event
// handle, neither tied to a thread, and overlapped operations started through
// it are not cancelled when the starting thread exits, since every one is
// finished before the call that started it returns.
unsafe impl Send for Op {}

impl Op {
    /// An idle op on `pipe` with a buffer of `capacity` bytes.
    fn new(pipe: Pipe, capacity: usize) -> io::Result<Self> {
        Ok(Self {
            pipe,
            state: Some(Box::new(OpState {
                // SAFETY: an all-zero OVERLAPPED is plain data, integers and a
                // null event; every start fills it in afresh.
                overlapped: UnsafeCell::new(unsafe { std::mem::zeroed() }),
                event: event()?,
                buf: vec![0; capacity],
            })),
            in_flight: false,
        })
    }

    /// The state, for a caller that may rewrite it: only with nothing in
    /// flight.
    fn idle_state(&mut self) -> io::Result<&mut OpState> {
        if self.in_flight {
            return Err(io::Error::other(
                "an operation on the taria pipe is still in flight",
            ));
        }
        self.state.as_deref_mut().ok_or_else(leaked)
    }

    /// Put `bytes` in the buffer, for the write that follows.
    fn fill(&mut self, bytes: &[u8]) -> io::Result<()> {
        let state = self.idle_state()?;
        state.buf.clear();
        state.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// The first `n` bytes of the buffer, once a read has finished.
    fn filled(&self, n: usize) -> &[u8] {
        match &self.state {
            Some(state) if !self.in_flight => state.buf.get(..n).unwrap_or_default(),
            _ => &[],
        }
    }

    /// Start an operation of `kind` on at most `len` bytes of the buffer.
    ///
    /// `Ok` means the kernel took it, and a wait and then
    /// [`finish`](Self::finish) collect its result, at once if it finished
    /// straight away. An error means it never started, and nothing the
    /// kernel holds refers to this op.
    fn start(&mut self, kind: Kind, len: usize) -> io::Result<()> {
        let pipe = self.pipe.raw();
        let state = self.idle_state()?;
        let len = u32::try_from(len.min(state.buf.len())).unwrap_or(u32::MAX);
        let event = state.event.raw();
        // SAFETY: resets this op's own event, which nothing waits on with
        // nothing in flight.
        if unsafe { ResetEvent(event) } == 0 {
            return Err(io::Error::last_os_error());
        }
        *state.overlapped.get_mut() = OVERLAPPED {
            // Pending until the kernel writes the final status, which is what
            // `still_pending` reads. The calls below set it too; setting it
            // here keeps that reading true whatever they do.
            Internal: STATUS_PENDING as u32 as usize,
            hEvent: event,
            // SAFETY: the rest of an OVERLAPPED is integers, zero for a pipe,
            // which has no file offset.
            ..unsafe { std::mem::zeroed() }
        };
        let overlapped = state.overlapped.get();
        let buf = state.buf.as_mut_ptr();
        // SAFETY: each call starts one operation on the pipe this op belongs
        // to, with this op's own OVERLAPPED and event and at most `len` bytes
        // of its own buffer, all boxed, so they stay where they are. They are
        // not touched or freed until `finish` has seen the operation finish,
        // or are leaked if it never does (see `Drop`).
        let ok = unsafe {
            match kind {
                Kind::Connect => ConnectNamedPipe(pipe, overlapped),
                Kind::Read => ReadFile(pipe, buf, len, ptr::null_mut(), overlapped),
                Kind::Write => WriteFile(pipe, buf.cast_const(), len, ptr::null_mut(), overlapped),
            }
        };
        if ok == 0 {
            // SAFETY: reads this thread's last error, set by the call above.
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                // Failed outright: the kernel completes nothing for an
                // operation it refused, so nothing is in flight.
                return Err(os_error(err));
            }
        }
        // Pending, or finished at once, which on a handle opened for
        // overlapped I/O still reports through the OVERLAPPED and the event.
        self.in_flight = true;
        Ok(())
    }

    /// Wait up to `timeout_ms` for the operation in flight to finish, or for
    /// `closed` to be set, whichever comes first. A finished operation wins a
    /// tie, so a read that has its data returns it.
    fn wait(&self, closed: Option<&Handle>, timeout_ms: u32) -> io::Result<Woke> {
        let state = self.state.as_deref().ok_or_else(leaked)?;
        // The status the kernel wrote is the word on whether it finished, and
        // is written before the event is set, so an operation that finished
        // at once is collected even if the event were never set for it.
        if self.in_flight && !still_pending(state) {
            return Ok(Woke::Finished);
        }
        let handles = [
            state.event.raw(),
            closed.map_or(ptr::null_mut(), Handle::raw),
        ];
        let count = if closed.is_some() { 2 } else { 1 };
        // SAFETY: waits on `count` handles, this op's event and the session's
        // closed event, both owned by values borrowed for the call.
        let woke = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, timeout_ms) };
        match woke {
            WAIT_OBJECT_0 => Ok(Woke::Finished),
            WAIT_TIMEOUT => Ok(Woke::TimedOut),
            woke if woke == WAIT_OBJECT_0 + 1 => Ok(Woke::Closed),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Wait for the operation in flight to finish, and return what it did:
    /// the bytes it moved, or its error.
    ///
    /// Returns at once once a wait has seen it finish. If the wait itself
    /// fails, the operation is still in flight and stays so, which keeps its
    /// memory alive.
    fn finish(&mut self) -> io::Result<u32> {
        if !self.in_flight {
            return Err(io::Error::other(
                "no operation on the taria pipe is in flight",
            ));
        }
        let state = self.state.as_deref().ok_or_else(leaked)?;
        let mut moved = 0u32;
        // SAFETY: the OVERLAPPED is the one the operation in flight was
        // started with, alive in its box; waiting writes only `moved`.
        let ok =
            unsafe { GetOverlappedResult(self.pipe.raw(), state.overlapped.get(), &mut moved, 1) };
        // SAFETY: reads this thread's last error, set by the call above.
        let err = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        if still_pending(state) {
            return Err(if ok == 0 {
                os_error(err)
            } else {
                io::Error::other("the taria pipe reported an unfinished operation as done")
            });
        }
        self.in_flight = false;
        if ok == 0 {
            Err(os_error(err))
        } else {
            Ok(moved)
        }
    }

    /// Cancel the operation in flight and wait for it to finish, which a
    /// cancelled operation does at once. Returns what it did: aborted, or
    /// its own result if it finished before the cancel reached it.
    fn cancel(&mut self) -> io::Result<u32> {
        if !self.in_flight {
            return Err(io::Error::other(
                "no operation on the taria pipe is in flight",
            ));
        }
        if let Some(state) = self.state.as_deref() {
            // SAFETY: cancels only the operation started with this
            // OVERLAPPED on this pipe, and frees nothing. ERROR_NOT_FOUND
            // means it had already finished, which `finish` collects.
            unsafe { CancelIoEx(self.pipe.raw(), state.overlapped.get()) };
        }
        self.finish()
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if self.in_flight {
            // Only a panic between a start and its finish gets here. The
            // kernel may still write to the state, so it is not freed until
            // the operation is over.
            let _ = self.cancel();
        }
        if self.in_flight
            && let Some(state) = self.state.take()
        {
            // The operation could not be seen to finish: the kernel may still
            // write into the state, so it is leaked, never freed.
            std::mem::forget(state);
        }
    }
}

/// Is the operation started with this state's OVERLAPPED still in flight?
/// The kernel replaces `STATUS_PENDING` with the final status as it finishes
/// it, before it sets the event.
fn still_pending(state: &OpState) -> bool {
    // SAFETY: reads one integer of the OVERLAPPED through the cell, volatile
    // because the kernel may be writing it.
    let internal = unsafe { ptr::read_volatile(&raw const (*state.overlapped.get()).Internal) };
    internal == STATUS_PENDING as u32 as usize
}

/// The error every operation returns once an [`Op`]'s state is leaked.
fn leaked() -> io::Error {
    io::Error::other("an operation on the taria pipe never finished, so the pipe is unusable")
}

/// What a session's [`Connection`], [`Reader`] and [`Writer`] share: the
/// event that says it is over.
struct Session {
    /// Manual-reset, set once by [`Writer::shutdown`] (or by the listener
    /// retiring the session), and never reset.
    closed: Handle,
}

impl Session {
    fn new() -> io::Result<Self> {
        Ok(Self { closed: event()? })
    }

    /// End the session: every read and write on it, in flight or to come,
    /// returns at once.
    fn close(&self) -> io::Result<()> {
        // SAFETY: sets this session's own event.
        if unsafe { SetEvent(self.closed.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn is_closed(&self) -> bool {
        // SAFETY: a zero-timeout wait on this session's own event.
        unsafe { WaitForSingleObject(self.closed.raw(), 0) == WAIT_OBJECT_0 }
    }
}

/// The bound pipe, listening for bridges.
///
/// Dropping it closes the listener's hold on the pipe, and the pipe goes, and
/// its name with it, once nothing else holds it either: no session, and no
/// bridge still holding its end.
pub(crate) struct Listener {
    path: PathBuf,
    pipe: Pipe,
    /// The listener's own op, for `ConnectNamedPipe`.
    connect: Op,
    /// The session handed out last, until the next accept retires it.
    last: Option<Arc<Session>>,
}

/// How one listen ended.
enum Connect {
    Connected,
    Shutdown,
    /// A client connected and left before the listen: listen again at once.
    Again,
    /// The listen failed: nap, then listen again.
    Failed,
}

impl Listener {
    /// Create the pipe `path` names, which must pass
    /// [`check_pipe_name`](taria::socket::check_pipe_name): a pipe on this
    /// machine, `\\.\pipe\<name>`, with one name after the prefix.
    ///
    /// A name another instance is serving is refused with
    /// [`AddrInUse`](io::ErrorKind::AddrInUse), as on Unix. The system
    /// decides that, not a probe: the pipe is created as the first instance
    /// of its name, which fails while any instance of it exists.
    pub(crate) fn bind(path: PathBuf) -> io::Result<Self> {
        let Some(name) = path.to_str() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not valid Unicode, so it cannot name a pipe",
                    path.display()
                ),
            ));
        };
        taria::socket::check_pipe_name(name)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        let wide: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(iter::once(0))
            .collect();

        let user = Sid::current_user()?;
        let security = PipeSecurity::owned_by(&user)?;
        let attributes = security.attributes();
        // SAFETY: `wide` is NUL-terminated and `attributes` points at a
        // descriptor `security` keeps alive, both for the length of the call,
        // which copies what it needs. The handle returned is owned below.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: reads this thread's last error, set by the call above.
            return Err(create_error(name, unsafe { GetLastError() }));
        }
        let pipe = Arc::new(Handle(handle));
        let connect = Op::new(Arc::clone(&pipe), 0)?;
        Ok(Self {
            path,
            pipe,
            connect,
            last: None,
        })
    }

    /// The pipe's name, as given.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Wait for the next bridge to connect, or for `shutdown` to say stop:
    /// `None` once it does.
    ///
    /// The session handed out last is retired first (see the module docs).
    /// The listen is an overlapped `ConnectNamedPipe`, waited on in slices of
    /// [`ACCEPT_POLL`] with `shutdown` asked between them, and cancelled
    /// (and waited for) once it says stop.
    ///
    /// No bridge ever [`waited`](Accepted::waited): a pipe has no backlog. A
    /// client that tries while another is served is refused with
    /// `ERROR_PIPE_BUSY` and gets nothing to write into, so there is never
    /// input written before a hello to throw away.
    pub(crate) fn accept(&mut self, shutdown: impl Fn() -> bool) -> Option<Accepted> {
        loop {
            if shutdown() {
                return None;
            }
            if !self.retire_last(&shutdown) {
                return None;
            }
            // Made before the listen, so a client that connects is never left
            // connected with no session to serve it.
            let Ok(session) = Session::new() else {
                thread::sleep(ACCEPT_POLL);
                continue;
            };
            match self.listen(&shutdown) {
                Connect::Connected => {
                    if shutdown() {
                        return None;
                    }
                    let session = Arc::new(session);
                    self.last = Some(Arc::clone(&session));
                    return Some(Accepted {
                        connection: Connection {
                            pipe: Arc::clone(&self.pipe),
                            session,
                        },
                        waited: false,
                    });
                }
                Connect::Shutdown => return None,
                Connect::Again => {}
                Connect::Failed => thread::sleep(ACCEPT_POLL),
            }
        }
    }

    /// Retire the session handed out last: close it, wait until nothing of
    /// it is left, then disconnect its client so the instance can be listened
    /// on again. False if `shutdown` said stop first.
    ///
    /// Waiting for the session to be gone is what keeps a reader or writer
    /// of the last session from ever reaching the next one's client. The
    /// layer joins both before it accepts again, so in practice nothing is
    /// left by the time this runs.
    fn retire_last(&mut self, shutdown: &impl Fn() -> bool) -> bool {
        let Some(last) = self.last.take() else {
            return true;
        };
        let _ = last.close();
        while Arc::strong_count(&last) > 1 {
            if shutdown() {
                return false;
            }
            thread::sleep(ACCEPT_POLL);
        }
        // Pairs with the release in the drops that brought the count to one,
        // so everything those owners did is done before the disconnect.
        fence(Ordering::Acquire);
        drop(last);
        // A pipe already disconnected says so, which is the state wanted.
        self.disconnect();
        true
    }

    /// Disconnect whichever client the instance has, discarding what it has
    /// not read, so the instance can be listened on again. False if the pipe
    /// refused.
    ///
    /// Only ever between sessions, never at shutdown: see the module docs.
    fn disconnect(&self) -> bool {
        // SAFETY: a plain call on the pipe handle, which the listener holds
        // open; it takes no memory of ours, so nothing in flight can be
        // affected but by having its operation end.
        unsafe { DisconnectNamedPipe(self.pipe.raw()) != 0 }
    }

    /// Listen for one client, until one connects or `shutdown` says stop.
    fn listen(&mut self, shutdown: &impl Fn() -> bool) -> Connect {
        if let Err(err) = self.connect.start(Kind::Connect, 0) {
            return self.listened(Err(err));
        }
        let slice = millis_ceil(ACCEPT_POLL);
        loop {
            match self.connect.wait(None, slice) {
                Ok(Woke::Finished) => {
                    let result = self.connect.finish();
                    return self.listened(result);
                }
                Ok(Woke::TimedOut | Woke::Closed) => {
                    if shutdown() {
                        // Whatever the listen did, the listener is going.
                        let _ = self.connect.cancel();
                        return Connect::Shutdown;
                    }
                }
                Err(_) => {
                    // The wait itself failed: give the listen up, and keep
                    // what it did if it finished first.
                    let result = self.connect.cancel();
                    return self.listened(result);
                }
            }
        }
    }

    /// What a listen's result means for the accept loop.
    fn listened(&mut self, result: io::Result<u32>) -> Connect {
        match result.map_err(|err| err.raw_os_error()) {
            Ok(_) => Connect::Connected,
            // A client got in before the listen started, which is a client
            // connected all the same.
            Err(Some(code)) if code == ERROR_PIPE_CONNECTED as i32 => Connect::Connected,
            // A client got in and left before the listen started.
            Err(Some(code)) if code == ERROR_NO_DATA as i32 => {
                if self.disconnect() {
                    Connect::Again
                } else {
                    Connect::Failed
                }
            }
            Err(_) => {
                self.disconnect();
                Connect::Failed
            }
        }
    }
}

/// The error a failed `CreateNamedPipeW` is reported as.
///
/// Access denied is what the first-instance flag answers when the name is
/// taken, and pipe busy what a pipe with no instance to spare answers: both
/// mean the name is in use, which is [`AddrInUse`](io::ErrorKind::AddrInUse)
/// as on Unix. Anything else keeps its own kind, with the name added.
fn create_error(name: &str, code: u32) -> io::Error {
    if code == ERROR_ACCESS_DENIED || code == ERROR_PIPE_BUSY {
        return io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another instance is already serving the taria pipe {name}, or something else \
                 holds that name (a bridge still connected to an instance that has exited keeps \
                 it until it lets go); refusing to take it over, because that would leave it \
                 running with no way for a bridge to reach it. To run a second instance \
                 alongside it, give it a pipe of its own: set TARIA_SOCK to a different name of \
                 the form {}<name>.",
                taria::socket::LOCAL_PIPE_PREFIX
            ),
        );
    }
    let err = os_error(code);
    io::Error::new(
        err.kind(),
        format!("cannot create the taria pipe {name}: {err}"),
    )
}

/// One accepted bridge: the pipe, connected to it, and its session.
pub(crate) struct Connection {
    pipe: Pipe,
    session: Arc<Session>,
}

impl Connection {
    /// Accepted for the seam's sake, and nothing to set: a pipe has no write
    /// timeout of its own, and every write is bounded by the deadline
    /// [`Writer::write_all_before`] is given. Zero is refused, as `std`
    /// refuses it for a socket, so both transports take the same values.
    pub(crate) fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot set a 0 duration timeout",
            ));
        }
        Ok(())
    }

    /// Split into the reader thread's half and the writer loop's, each with
    /// an op of its own on the one pipe handle.
    pub(crate) fn split(self) -> io::Result<(Reader, Writer)> {
        let reader = Reader {
            op: Op::new(Arc::clone(&self.pipe), READ_CHUNK)?,
            session: Arc::clone(&self.session),
        };
        let writer = Writer {
            op: Op::new(self.pipe, 0)?,
            session: self.session,
        };
        Ok((reader, writer))
    }
}

/// The reading half of a [`Connection`].
pub(crate) struct Reader {
    op: Op,
    session: Arc<Session>,
}

impl Read for Reader {
    /// Read what the bridge sent, waiting for it if nothing is there yet.
    ///
    /// End of stream (`Ok(0)`) once the bridge has gone, and once the
    /// session is shut down, at once and for good: a read in flight when
    /// [`Writer::shutdown`] is called is cancelled, and one started after it
    /// never starts.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_up_to(buf, buf.len())
    }
}

impl Reader {
    /// Run `f` on this connection with reads that fail with
    /// [`WouldBlock`](io::ErrorKind::WouldBlock) rather than wait once what
    /// is already there has been read.
    ///
    /// Nothing about the pipe changes: each read first asks the pipe how much
    /// is waiting (`PeekNamedPipe`) and reads no more than that, which a read
    /// then finds at once. The layer never calls this on Windows, since no
    /// bridge ever [`waited`](Accepted::waited), but it keeps the seam whole.
    pub(crate) fn with_nonblocking_reads<R>(
        &mut self,
        f: impl FnOnce(&mut dyn Read) -> R,
    ) -> io::Result<R> {
        Ok(f(&mut ReadyOnly(self)))
    }

    /// Read at most `len` bytes into `buf`.
    fn read_up_to(&mut self, buf: &mut [u8], len: usize) -> io::Result<usize> {
        let len = len.min(buf.len()).min(READ_CHUNK);
        if len == 0 || self.session.is_closed() {
            return Ok(0);
        }
        match self.op.start(Kind::Read, len) {
            Ok(()) => {}
            Err(err) if is_disconnect(&err) => return Ok(0),
            Err(err) => return Err(err),
        }
        let read = loop {
            match self.op.wait(Some(&self.session.closed), INFINITE) {
                Ok(Woke::Finished) => break self.op.finish(),
                Ok(Woke::Closed) => {
                    break match self.op.cancel() {
                        Err(err) if err.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => {
                            Ok(0)
                        }
                        result => result,
                    };
                }
                Ok(Woke::TimedOut) => {}
                Err(err) => {
                    let _ = self.op.cancel();
                    return Err(err);
                }
            }
        };
        match read {
            Ok(n) => {
                let data = self.op.filled(n as usize);
                let Some(into) = buf.get_mut(..data.len()) else {
                    return Err(io::Error::other(
                        "the taria pipe read more than it was asked for",
                    ));
                };
                into.copy_from_slice(data);
                Ok(data.len())
            }
            Err(err) if is_disconnect(&err) => Ok(0),
            Err(err) => Err(err),
        }
    }

    /// How many bytes the bridge has sent that are waiting to be read, or
    /// `None` once it has gone.
    fn waiting(&self) -> io::Result<Option<u32>> {
        let mut waiting = 0u32;
        // SAFETY: peeks no data (no buffer, length 0), only the count, into
        // a local.
        let ok = unsafe {
            PeekNamedPipe(
                self.op.pipe.raw(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut waiting,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            return if is_disconnect(&err) {
                Ok(None)
            } else {
                Err(err)
            };
        }
        Ok(Some(waiting))
    }
}

/// A [`Reader`] that reads only what is already waiting, for
/// [`Reader::with_nonblocking_reads`].
struct ReadyOnly<'a>(&'a mut Reader);

impl Read for ReadyOnly<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.0.session.is_closed() {
            return Ok(0);
        }
        match self.0.waiting()? {
            None => Ok(0),
            Some(0) => Err(io::ErrorKind::WouldBlock.into()),
            Some(waiting) => self.0.read_up_to(buf, waiting as usize),
        }
    }
}

/// The writing half of a [`Connection`].
pub(crate) struct Writer {
    op: Op,
    session: Arc<Session>,
}

impl Writer {
    /// `write_all`, finished by `deadline` or failed with
    /// [`TimedOut`](io::ErrorKind::TimedOut).
    ///
    /// A line is one overlapped `WriteFile`, which finishes once the pipe has
    /// taken all of it, and is waited on for what is left of the deadline,
    /// and on the session's closed event beside it. At the deadline the
    /// write is cancelled and waited for, so its buffer is never freed under
    /// the kernel, and it is `TimedOut` unless it finished in full first.
    /// That bounds the whole line, however the bridge reads, which is the
    /// layer's `WRITE_TIMEOUT`, and the whole of the acks flushed at
    /// shutdown, which is its `SHUTDOWN_FLUSH`.
    ///
    /// Nothing more is needed for those acks to arrive: a write that
    /// finished left them in the pipe, and what is in the pipe stays readable
    /// by the bridge after the app closes its end. `FlushFileBuffers`, which
    /// waits for the bridge to read it all and can wait forever, is never
    /// called, and neither is `DisconnectNamedPipe`, which would throw away
    /// what the bridge has not read yet, at shutdown.
    ///
    /// A write cut short may have written part of the line, and every caller
    /// then drops the connection, as on Unix. Once the session is shut down a
    /// write fails at once with [`NotConnected`](io::ErrorKind::NotConnected).
    pub(crate) fn write_all_before(
        &mut self,
        mut bytes: &[u8],
        deadline: Instant,
    ) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.session.is_closed() {
                return Err(shut_down());
            }
            if deadline <= Instant::now() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let chunk = bytes.get(..MAX_WRITE).unwrap_or(bytes);
            match self.write_before(chunk, deadline)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => bytes = bytes.get(n..).unwrap_or_default(),
            }
        }
        Ok(())
    }

    /// One `WriteFile` of `chunk`, finished by `deadline` or cancelled.
    fn write_before(&mut self, chunk: &[u8], deadline: Instant) -> io::Result<usize> {
        self.op.fill(chunk)?;
        self.op.start(Kind::Write, chunk.len())?;
        let whole = |moved: u32| moved as usize == chunk.len();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let woke = if left.is_zero() {
                Ok(Woke::TimedOut)
            } else {
                self.op.wait(Some(&self.session.closed), millis_ceil(left))
            };
            match woke {
                Ok(Woke::Finished) => return self.op.finish().map(|moved| moved as usize),
                // A timer that fires a little early is waited again.
                Ok(Woke::TimedOut) if Instant::now() < deadline => {}
                Ok(Woke::TimedOut) => {
                    return match self.op.cancel() {
                        Ok(moved) if whole(moved) => Ok(moved as usize),
                        _ => Err(io::ErrorKind::TimedOut.into()),
                    };
                }
                Ok(Woke::Closed) => {
                    return match self.op.cancel() {
                        Ok(moved) if whole(moved) => Ok(moved as usize),
                        _ => Err(shut_down()),
                    };
                }
                Err(err) => {
                    let _ = self.op.cancel();
                    return Err(err);
                }
            }
        }
    }

    /// Shut the session down, which wakes the [`Reader`] blocked on it.
    ///
    /// Sets the session's closed event, for good: a read in flight is
    /// cancelled and returns end of stream, and every later read and write on
    /// the session returns at once. What was written stays in the pipe for
    /// the bridge to read; see [`write_all_before`](Self::write_all_before).
    /// The client is disconnected by the next accept, or let go when the
    /// listener closes the pipe.
    ///
    /// The closed event rather than `CancelIoEx` alone, because a cancel
    /// reaches only the read in flight when it is made: a reader between
    /// reads would start its next one after it and wait forever. And not
    /// `DisconnectNamedPipe`, which would also end it, because it throws away
    /// what the bridge has not read yet, the acks flushed at shutdown among
    /// it.
    pub(crate) fn shutdown(&self) -> io::Result<()> {
        self.session.close()
    }
}

/// The error a write on a session that was shut down fails with.
fn shut_down() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "the taria pipe's session was shut down",
    )
}

/// Does `err` say the bridge has gone? Read as end of stream, as `std` reads
/// a broken pipe.
fn is_disconnect(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error().map(|code| code as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    )
}

/// `duration` in whole milliseconds, rounded up, for a wait: never
/// `INFINITE`, and never zero for a duration that is not.
fn millis_ceil(duration: Duration) -> u32 {
    let millis = duration.as_nanos().div_ceil(1_000_000);
    u32::try_from(millis)
        .unwrap_or(INFINITE - 1)
        .min(INFINITE - 1)
}

/// A Win32 error code as the `io::Error` it is.
fn os_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// The current user's SID string, `S-1-5-21-...`, from the process token,
/// which names the default pipe (see [`taria::socket::pipe_name`]).
///
/// An error, rather than a fallback, when the token cannot be read: the name
/// is what keeps one user's pipes apart from another's, and a guessed one
/// would only put the app where its bridge does not look.
pub(crate) fn user_identity() -> io::Result<String> {
    Sid::current_user()
        .and_then(|sid| sid.to_string_sid())
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "cannot read this user's SID from the process token, which names the default \
                     taria pipe: {err}; set TARIA_SOCK to a pipe name of the form {}<name> to \
                     bind without it",
                    taria::socket::LOCAL_PIPE_PREFIX
                ),
            )
        })
}

#[cfg(all(test, windows))]
mod tests {
    use std::env;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::io::FromRawHandle;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

    use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
    use taria::{AgentInput, PROTOCOL_VERSION};
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        OWNER_SECURITY_INFORMATION, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
    };

    use super::security::LocalMemory;
    use super::*;
    use crate::TariaLayer;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// `ACCESS_ALLOWED_ACE_TYPE`, which lives in a `windows-sys` feature this
    /// crate does not otherwise need.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    /// A pipe name no other test, run or process uses.
    fn unique_name(label: &str) -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        format!(
            r"\\.\pipe\taria-transport-test-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(iter::once(0)).collect()
    }

    fn bind(label: &str) -> Listener {
        Listener::bind(PathBuf::from(unique_name(label))).expect("bind a test pipe")
    }

    fn name_of(listener: &Listener) -> String {
        listener
            .path()
            .to_str()
            .expect("a test name is Unicode")
            .to_owned()
    }

    /// Open the pipe as a bridge does: both ways, synchronous, through the W
    /// call.
    fn open_client(name: &str) -> io::Result<File> {
        let wide = wide(name);
        // SAFETY: `wide` is NUL-terminated; the handle returned is owned by
        // the `File` below.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle this function owns, handed to the `File`.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    /// Open the pipe, retrying while it is busy, as it is for the moment
    /// between one session and the next.
    fn connect_client(name: &str) -> File {
        let until = Instant::now() + TIMEOUT;
        loop {
            match open_client(name) {
                Ok(file) => return file,
                Err(err)
                    if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && Instant::now() < until =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("cannot open {name}: {err}"),
            }
        }
    }

    /// Accept on a thread of its own, handing the listener back with what
    /// it accepted.
    fn accept_async(
        mut listener: Listener,
        stop: Arc<AtomicBool>,
    ) -> mpsc::Receiver<(Listener, Option<Accepted>)> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let accepted = listener.accept(|| stop.load(Ordering::SeqCst));
            let _ = tx.send((listener, accepted));
        });
        rx
    }

    /// Accept the next client on `listener`, connecting one to it.
    fn next_session(listener: Listener) -> (Listener, Connection, File) {
        let name = name_of(&listener);
        let accepting = accept_async(listener, Arc::new(AtomicBool::new(false)));
        let client = connect_client(&name);
        let (listener, accepted) = accepting
            .recv_timeout(TIMEOUT)
            .expect("accept returns once a client connects");
        let accepted = accepted.expect("the client is accepted");
        assert!(!accepted.waited, "a pipe has no backlog to wait in");
        (listener, accepted.connection, client)
    }

    /// A fresh pipe with one client connected and accepted.
    fn session(label: &str) -> (Listener, Connection, File) {
        next_session(bind(label))
    }

    fn read_line(from: &mut impl BufRead) -> String {
        let mut line = String::new();
        from.read_line(&mut line).expect("read a line");
        line
    }

    #[test]
    fn a_line_goes_each_way() {
        let (_listener, connection, client) = session("roundtrip");
        connection
            .set_write_timeout(TIMEOUT)
            .expect("a nonzero timeout");
        let (mut reader, mut writer) = connection.split().expect("split");

        writer
            .write_all_before(b"hello\n", Instant::now() + TIMEOUT)
            .expect("write a line");
        let mut from_app = BufReader::new(client.try_clone().expect("clone the client"));
        assert_eq!(read_line(&mut from_app), "hello\n");

        (&client).write_all(b"world\n").expect("the bridge writes");
        assert_eq!(read_line(&mut BufReader::new(&mut reader)), "world\n");
    }

    #[test]
    fn a_second_bind_of_the_same_name_is_refused_as_in_use() {
        let first = bind("twice");
        let name = name_of(&first);

        let Err(err) = Listener::bind(PathBuf::from(&name)) else {
            panic!("a second instance must not get the first's pipe");
        };
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "err: {err}");
        let message = err.to_string();
        assert!(message.contains("already serving"), "err: {message}");
        assert!(message.contains("TARIA_SOCK"), "err: {message}");

        // The first is untouched, and still the one a bridge reaches.
        let (_first, _connection, _client) = next_session(first);
    }

    #[test]
    fn the_pipe_is_owned_by_its_user_and_lets_nobody_else_in() {
        let listener = bind("dacl");
        let me = Sid::current_user().expect("read the user");
        let everyone = string_sid("S-1-1-0");
        let anonymous = string_sid("S-1-5-7");

        let mut owner: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: queries the pipe's owner and DACL into locals; the
        // descriptor they point into is freed by the guard below.
        let code = unsafe {
            GetSecurityInfo(
                listener.pipe.raw(),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(code, 0, "{}", os_error(code));
        let _descriptor = LocalMemory(descriptor);

        // SAFETY: both SIDs are valid: the system's and this test's own.
        assert_ne!(
            unsafe { EqualSid(owner, me.as_psid()) },
            0,
            "the user owns the pipe, even from an elevated token"
        );
        assert!(!dacl.is_null(), "a null DACL would let everyone in");
        // SAFETY: a valid DACL, inside the descriptor kept alive above.
        let count = unsafe { (*dacl).AceCount };
        assert!(
            count >= 1,
            "a DACL with no entries lets nobody in, the user included"
        );
        for index in 0..count {
            let mut ace = ptr::null_mut();
            // SAFETY: `index` is below the DACL's own count.
            assert_ne!(unsafe { GetAce(dacl, index.into(), &mut ace) }, 0);
            // SAFETY: every ACE starts with a header.
            let header = unsafe { *ace.cast::<ACE_HEADER>() };
            assert_eq!(header.AceType, ACCESS_ALLOWED_ACE_TYPE, "ACE {index}");
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: an access-allowed ACE, whose SID starts at `SidStart`.
            let (sid, mask): (PSID, u32) =
                unsafe { ((&raw mut (*allowed).SidStart).cast(), (*allowed).Mask) };
            // SAFETY: all valid SIDs.
            unsafe {
                assert_eq!(EqualSid(sid, everyone.0), 0, "no Everyone entry");
                assert_eq!(EqualSid(sid, anonymous.0), 0, "no anonymous entry");
                assert_ne!(EqualSid(sid, me.as_psid()), 0, "ACE {index} is the user's");
            }
            let allowed_rights =
                GENERIC_READ | GENERIC_WRITE | FILE_GENERIC_READ | FILE_GENERIC_WRITE;
            assert_eq!(mask & !allowed_rights, 0, "ACE {index} grants {mask:#x}");
        }
    }

    /// A SID from its string form, freed when the guard drops.
    fn string_sid(text: &str) -> LocalMemory {
        let wide = wide(text);
        let mut sid = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; the SID is freed by the guard.
        assert_ne!(
            unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) },
            0
        );
        LocalMemory(sid)
    }

    #[test]
    fn a_second_client_is_told_the_pipe_is_busy() {
        let (listener, _connection, _client) = session("busy");
        let err = open_client(&name_of(&listener)).expect_err("one bridge at a time");
        assert_eq!(
            err.raw_os_error(),
            Some(ERROR_PIPE_BUSY as i32),
            "err: {err}"
        );
    }

    #[test]
    fn the_write_deadline_fires_against_a_bridge_that_never_reads() {
        let (_listener, connection, _client) = session("deadline");
        let (_reader, mut writer) = connection.split().expect("split");
        // Far more than the pipe holds, so the write waits for a reader.
        let line = vec![b'x'; 4 * 1024 * 1024];
        let budget = Duration::from_millis(300);

        let started = Instant::now();
        let err = writer
            .write_all_before(&line, started + budget)
            .expect_err("a bridge that reads nothing cannot take 4 MiB");
        let took = started.elapsed();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "err: {err}");
        assert!(took >= budget, "gave up early, after {took:?}");
        assert!(
            took < budget + Duration::from_millis(200),
            "gave up {:?} past the deadline",
            took - budget
        );
        assert!(
            !writer.op.in_flight && writer.op.state.is_some(),
            "the cancelled write was waited for, and its buffer kept, not leaked"
        );
    }

    #[test]
    fn shutdown_wakes_a_blocked_reader_and_every_later_read_returns_at_once() {
        let (_listener, connection, _client) = session("wake");
        let (mut reader, writer) = connection.split().expect("split");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 64];
            let first = reader.read(&mut buf).map_err(|err| err.kind());
            let woke = Instant::now();
            // The read the coordinator's race is about: one started after
            // the shutdown, which a cancel alone would leave blocked.
            let again = reader.read(&mut buf).map_err(|err| err.kind());
            let again_took = woke.elapsed();
            let _ = tx.send((first, woke, again, again_took));
        });
        // Long enough for the reader to be parked in its read.
        thread::sleep(Duration::from_millis(200));

        let shut = Instant::now();
        writer.shutdown().expect("shut down");
        let (first, woke, again, again_took) = rx
            .recv_timeout(TIMEOUT)
            .expect("the shutdown wakes the reader");

        let latency = woke.duration_since(shut);
        assert_eq!(first, Ok(0), "end of stream");
        assert!(
            latency < Duration::from_millis(100),
            "woke after {latency:?}"
        );
        assert_eq!(again, Ok(0), "a read after the shutdown is end of stream");
        assert!(
            again_took < Duration::from_millis(100),
            "a read after the shutdown took {again_took:?}"
        );
    }

    #[test]
    fn a_write_after_shutdown_fails_at_once() {
        let (_listener, connection, _client) = session("shutwrite");
        let (_reader, mut writer) = connection.split().expect("split");
        writer.shutdown().expect("shut down");
        let started = Instant::now();
        let err = writer
            .write_all_before(b"late\n", started + TIMEOUT)
            .expect_err("the session is over");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected, "err: {err}");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn shutdown_wakes_a_blocked_accept() {
        let listener = bind("wakeaccept");
        let stop = Arc::new(AtomicBool::new(false));
        let accepting = accept_async(listener, Arc::clone(&stop));
        // Long enough for the listener to be parked in its listen.
        thread::sleep(Duration::from_millis(200));

        let stopped = Instant::now();
        stop.store(true, Ordering::SeqCst);
        let (_listener, accepted) = accepting
            .recv_timeout(TIMEOUT)
            .expect("the flag wakes the listener");
        let latency = stopped.elapsed();

        assert!(accepted.is_none(), "nothing was accepted");
        assert!(
            latency < Duration::from_millis(100),
            "woke after {latency:?}"
        );
    }

    #[test]
    fn the_instance_is_reused_session_after_session() {
        let mut listener = bind("reuse");
        let handle = listener.pipe.raw();
        for round in 0..3 {
            let (next, connection, client) = next_session(listener);
            listener = next;
            assert_eq!(listener.pipe.raw(), handle, "the one instance, every time");
            let (mut reader, mut writer) = connection.split().expect("split");
            let deadline = Instant::now() + TIMEOUT;

            let mut from_app = BufReader::new(client.try_clone().expect("clone the client"));
            writer
                .write_all_before(format!("round {round}\n").as_bytes(), deadline)
                .expect("write");
            assert_eq!(
                read_line(&mut from_app),
                format!("round {round}\n"),
                "this session's own line, and nothing the last one left unread"
            );
            // Left unread when the bridge goes: the next session must never
            // see it.
            writer
                .write_all_before(b"left behind\n", deadline)
                .expect("write");
            drop(from_app);
            drop(client);

            let mut buf = [0u8; 16];
            assert_eq!(
                reader.read(&mut buf).expect("read"),
                0,
                "end of stream once the bridge has gone"
            );
            writer.shutdown().expect("shut down");
        }
    }

    #[test]
    fn what_was_written_before_shutdown_survives_the_pipe_closing() {
        let (listener, connection, client) = session("flush");
        let (reader, mut writer) = connection.split().expect("split");
        // The acks a shutdown flushes: written, not yet read.
        writer
            .write_all_before(
                b"ack 1\nack 2\n",
                Instant::now() + Duration::from_millis(250),
            )
            .expect("flush");
        // As the layer's drop does it: shut down, reap, close. No disconnect.
        writer.shutdown().expect("shut down");
        drop((reader, writer, listener));

        let mut from_app = BufReader::new(client);
        assert_eq!(read_line(&mut from_app), "ack 1\n");
        assert_eq!(read_line(&mut from_app), "ack 2\n");
        assert_eq!(read_line(&mut from_app), "", "then end of stream");
    }

    #[test]
    fn nonblocking_reads_take_what_is_there_and_then_would_block() {
        let (_listener, connection, client) = session("ready");
        let (mut reader, _writer) = connection.split().expect("split");
        (&client).write_all(b"early\n").expect("the bridge writes");

        let (got, last) = reader
            .with_nonblocking_reads(|ready| {
                let mut got = Vec::new();
                let mut buf = [0u8; 4];
                loop {
                    match ready.read(&mut buf) {
                        Ok(0) => return (got, None),
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                        Err(err) => return (got, Some(err.kind())),
                    }
                }
            })
            .expect("the reads run");
        assert_eq!(got, b"early\n");
        assert_eq!(last, Some(io::ErrorKind::WouldBlock));
    }

    #[test]
    fn names_that_are_not_a_local_pipe_are_refused() {
        for name in [
            r"\\host\pipe\taria",
            r"\\.\pipe\..\UNC\host\share",
            r"\\.\pipe\a/b",
            r"\\?\pipe\taria",
            r"C:\Users\someone\app.sock",
            "app.sock",
        ] {
            let Err(err) = Listener::bind(PathBuf::from(name)) else {
                panic!("{name} was bound");
            };
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}: {err}");
        }
        let Err(err) = Listener::bind(PathBuf::from(r"\\host\pipe\taria")) else {
            panic!("a remote pipe was bound");
        };
        assert!(err.to_string().contains("SMB"), "err: {err}");
    }

    #[test]
    fn the_user_identity_is_a_sid_that_names_a_pipe() {
        let sid = user_identity().expect("read the SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
        let name = taria::socket::pipe_name("demo", &sid).expect("a SID names a pipe");
        assert_eq!(name, format!(r"\\.\pipe\taria-{sid}-demo"));
    }

    #[test]
    fn the_default_endpoint_is_the_users_own_pipe() {
        if env::var_os("TARIA_SOCK").is_some_and(|name| !name.is_empty()) {
            return;
        }
        let label = format!("default-{}", std::process::id());
        let sid = user_identity().expect("read the SID");
        let layer = TariaLayer::bind_or_disabled(&label);
        assert!(layer.is_enabled(), "{:?}", layer.bind_error());
        assert_eq!(
            layer.socket_path(),
            Path::new(&format!(r"\\.\pipe\taria-{sid}-{label}"))
        );
    }

    /// The layer end to end over the pipe, and the case the shutdown flush is
    /// for: an app that takes an input and exits in the same breath still
    /// gets its ack to the bridge.
    #[test]
    fn a_layer_serves_a_bridge_and_its_last_ack_survives_exit() {
        let name = unique_name("layer");
        let layer = TariaLayer::bind_at("pipe-e2e", &name).expect("bind the layer");
        assert_eq!(layer.socket_path(), Path::new(&name));

        let client = connect_client(&name);
        let mut from_app = BufReader::new(client.try_clone().expect("clone the client"));
        let hello: AppToBridge =
            serde_json::from_str(&read_line(&mut from_app)).expect("a message");
        assert_eq!(
            hello,
            AppToBridge::hello("pipe-e2e", PROTOCOL_VERSION).stamps(true)
        );

        let mut line = serde_json::to_string(&BridgeToApp::input(
            InputId::new(7),
            AgentInput::key("enter"),
        ))
        .expect("serialize");
        line.push('\n');
        (&client).write_all(line.as_bytes()).expect("send an input");
        let (id, _input) = layer.recv_timeout(TIMEOUT).expect("the input arrives");
        assert_eq!(id, InputId::new(7));

        let dropped = Instant::now();
        drop(layer);
        assert!(
            dropped.elapsed() < Duration::from_secs(1),
            "drop took {:?}",
            dropped.elapsed()
        );

        let mut messages = Vec::new();
        loop {
            let line = read_line(&mut from_app);
            if line.is_empty() {
                break;
            }
            messages.push(serde_json::from_str::<AppToBridge>(&line).expect("a message"));
        }
        assert!(
            messages.contains(&AppToBridge::ack(InputId::new(7), InputStatus::Delivered)),
            "the ack reached the bridge before the pipe closed: {messages:?}"
        );
    }
}
