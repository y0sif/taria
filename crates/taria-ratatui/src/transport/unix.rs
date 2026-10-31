//! The Unix transport: a Unix domain socket in a private directory.
//!
//! Binding vets the socket's directory, clears a socket a killed run left
//! behind and refuses a live one, and the listener unlinks its socket when it
//! is dropped, but only while the file is still the one it bound. The listener
//! is non-blocking and polls the layer's shutdown flag, so exit never waits on
//! a connection. Each accepted connection is one blocking socket, duplicated
//! into a reader and a writer.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::mem;
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::Accepted;

/// The most one `write` call is handed, so that the kernel waits for room in
/// the socket at most once inside it, and the write timeout, which bounds
/// each such wait rather than the call, bounds the call too. See
/// [`Writer::write_all_before`].
///
/// One buffer on Linux, and within the send low-water mark that macOS and the
/// BSDs wait for before they copy anything. A typical line is one or a few
/// calls, and a snapshot at [`MAX_LINE_BYTES`](taria::wire::MAX_LINE_BYTES)
/// about five hundred, measured at under a millisecond more than one call for
/// the lot.
const WRITE_CHUNK: usize = 2 * 1024;

/// The longest one wait for room in the socket runs before the writer looks
/// at the clock again. See [`Writer::write_all_before`].
///
/// The kernel keeps a socket's write timeout on a timer wheel that files a
/// long timeout coarsely and fires it late: on Linux at 1000 Hz, a 5 s
/// timeout fired up to 430 ms past its time when measured, more than the
/// layer's whole `SHUTDOWN_FLUSH`. A timeout this short is filed to the
/// millisecond, so waiting in slices of it holds a deadline to within a few
/// milliseconds, and a writer stalled on a stuck bridge wakes 20 times a
/// second to find it still stuck.
const WRITE_WAIT: Duration = Duration::from_millis(50);

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

/// The bound socket, listening for bridges.
///
/// Dropping it closes the socket and then unlinks the socket file, but only
/// while that file is still the one this listener bound: a second instance
/// that took the path over is serving a socket of its own there, and
/// unlinking it would leave that instance running and unreachable.
pub(crate) struct Listener {
    /// Declared before `unlink` so it is dropped first: the socket closes,
    /// then its file goes.
    socket: UnixListener,
    unlink: Unlink,
    /// Whether the next connection accepted waited behind the last one; see
    /// [`accept`](Self::accept).
    waited: bool,
}

/// Unlinks the socket file when dropped, if it is still the one bound.
struct Unlink {
    path: PathBuf,
    /// Device and inode of the socket file as bound, so [`Drop`] can tell it
    /// from a file that replaced it. `None` when it could not be read, which
    /// makes every comparison fail and leaves the path alone.
    ident: Option<(u64, u64)>,
}

impl Drop for Unlink {
    fn drop(&mut self) {
        remove_own_socket(&self.path, self.ident);
    }
}

impl Listener {
    /// Bind a socket at `path`, creating its directory if need be.
    ///
    /// A relative `path`, a bare file name included, is resolved against the
    /// current directory first, so the directory that would hold the socket
    /// is always vetted; [`path`](Self::path) is the path as resolved. The
    /// path is checked against the platform's length limit before anything is
    /// created, the directory is created with mode `0700` and refused unless
    /// it is private (see [`ensure_private_dir`]), and a socket already at the
    /// path is cleared only if nothing is listening on it (see
    /// [`clear_socket_path`]).
    pub(crate) fn bind(path: PathBuf) -> io::Result<Self> {
        // Absolutize before looking at the parent: a bare filename like
        // "app.sock" has an empty parent, which must not bypass the privacy
        // check by silently binding in an unvetted current directory.
        let path = absolutize(path)?;

        // Check the length here, before anything is created: over the limit,
        // `bind` below would fail with `InvalidInput: path must be shorter
        // than SUN_LEN`, which names neither the path, its length, the limit,
        // nor a way out. `SocketPathTooLong` names all four.
        taria::socket::check_path_len(&path)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }
        // Free the path if a dead socket is holding it, and refuse the bind
        // if anything else is.
        clear_socket_path(&path)?;

        let socket = UnixListener::bind(&path)?;
        // Polled rather than parked in `accept()`, so shutdown needs no
        // connection to be noticed; see `ACCEPT_POLL`.
        if let Err(err) = socket.set_nonblocking(true) {
            drop(socket);
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        // Identify the socket just bound, before anyone else can replace it.
        let ident = socket_ident(&path);
        Ok(Self {
            socket,
            unlink: Unlink { path, ident },
            waited: false,
        })
    }

    /// The path the socket is bound at, made absolute.
    pub(crate) fn path(&self) -> &Path {
        &self.unlink.path
    }

    /// Wait for the next bridge to connect, or for `shutdown` to say stop:
    /// `None` once it does.
    ///
    /// The listener is non-blocking, so the thread is never parked in
    /// `accept()` and `shutdown` is asked on its own schedule; see
    /// [`ACCEPT_POLL`] for why exit cannot be left to depend on a connection
    /// arriving. An accept that fails for another reason is retried after
    /// the same nap.
    ///
    /// It also tracks whether the connection it hands over waited behind the
    /// last one. A connection accepted after a session ended, before any
    /// accept has found the backlog empty, was waiting in the backlog while
    /// that session ran; a client that connects in the instant between the
    /// session's end and the next accept is taken for one too. One accepted
    /// after an accept found nothing connected after the session had ended
    /// waited behind nobody, and neither did the first connection of the
    /// process.
    pub(crate) fn accept(&mut self, shutdown: impl Fn() -> bool) -> Option<Accepted> {
        loop {
            if shutdown() {
                return None;
            }
            match self.socket.accept() {
                Ok((stream, _addr)) => {
                    if shutdown() {
                        return None;
                    }
                    // Both halves of a session are read and written as
                    // blocking streams, and an accepted socket inherits the
                    // listener's non-blocking flag on some platforms. Say it
                    // outright rather than rely on which one this is.
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    // Whoever is in the backlog once this client's session
                    // ends connected while it was served, so it waited behind
                    // it, however that ends.
                    let waited = mem::replace(&mut self.waited, true);
                    return Some(Accepted {
                        connection: Connection { stream },
                        waited,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    // Nobody is waiting, so whoever connects next waited
                    // behind nobody.
                    self.waited = false;
                    // Nap instead of spinning, then back to the shutdown
                    // check at the top.
                    thread::sleep(ACCEPT_POLL);
                }
                Err(_) => {
                    if shutdown() {
                        return None;
                    }
                    // Avoid a hot loop on a persistently failing listener.
                    thread::sleep(ACCEPT_POLL);
                }
            }
        }
    }
}

/// One accepted bridge: a blocking Unix stream.
pub(crate) struct Connection {
    stream: UnixStream,
}

impl Connection {
    /// Bound how long any one write may block.
    ///
    /// `SO_SNDTIMEO` is shared across the duplicated fds that
    /// [`split`](Self::split) makes; only writes block long enough to need
    /// it. Every line sets it again before each of its writes, from its own
    /// deadline (see [`Writer::write_all_before`]), so after the first line
    /// it stays at [`WRITE_WAIT`] or less, and the value set here governs no
    /// write: it only means the socket never starts out with no timeout at
    /// all.
    pub(crate) fn set_write_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.stream.set_write_timeout(Some(timeout))
    }

    /// Split into the reader thread's half and the writer loop's: the stream
    /// itself for reading, and a duplicate of its fd for writing.
    pub(crate) fn split(self) -> io::Result<(Reader, Writer)> {
        let writer = self.stream.try_clone()?;
        Ok((
            Reader {
                stream: self.stream,
            },
            Writer { stream: writer },
        ))
    }
}

/// The reading half of a [`Connection`].
pub(crate) struct Reader {
    stream: UnixStream,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Reader {
    /// Run `f` on this connection with reads that fail with
    /// [`WouldBlock`](io::ErrorKind::WouldBlock) rather than wait once what
    /// is already there has been read, then make reads block again.
    ///
    /// The flag belongs to the open socket, not to this handle, so the
    /// [`Writer`]'s duplicate goes non-blocking with it. It is put back
    /// before `f`'s outcome is returned, and so before anything is written:
    /// the hello and every later write rely on blocking until their deadline.
    pub(crate) fn with_nonblocking_reads<R>(
        &mut self,
        f: impl FnOnce(&mut dyn Read) -> R,
    ) -> io::Result<R> {
        self.stream.set_nonblocking(true)?;
        let outcome = f(&mut &self.stream);
        self.stream.set_nonblocking(false)?;
        Ok(outcome)
    }
}

/// The writing half of a [`Connection`].
pub(crate) struct Writer {
    stream: UnixStream,
}

/// A writer over one end of a socket pair, for the layer's tests that drive
/// its writer loop directly.
#[cfg(test)]
impl From<UnixStream> for Writer {
    fn from(stream: UnixStream) -> Self {
        Self { stream }
    }
}

impl Writer {
    /// `write_all`, finished by `deadline` or failed with
    /// [`TimedOut`](io::ErrorKind::TimedOut).
    ///
    /// A socket's write timeout does not bound a `write` call, only each wait
    /// for room inside one, and a call waits again for every piece the kernel
    /// splits it into (Linux allocates a buffer of at most a few tens of KB at
    /// a time, and waits for room before each). So a large line, against a
    /// bridge that frees a little room before every wait can time out, never
    /// times out at all: a 900 KB snapshot to a bridge taking 64 KB every 3 s
    /// held the writer for forty seconds with a 5 s timeout. Setting the
    /// timeout to what is left of a deadline before each call does not help
    /// on its own, because a single call runs past the deadline.
    ///
    /// So no call is handed more than [`WRITE_CHUNK`] bytes, few enough that
    /// the kernel waits for room at most once inside it, and that wait is
    /// given what is left of the deadline, or [`WRITE_WAIT`] if that is less,
    /// set before every call. A wait that times out short of the deadline is
    /// simply waited again. A timeout of zero means none at all to the socket
    /// and is refused, which is why nothing left ends the loop rather than
    /// reaching it.
    pub(crate) fn write_all_before(
        &mut self,
        mut bytes: &[u8],
        deadline: Instant,
    ) -> io::Result<()> {
        while !bytes.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.stream.set_write_timeout(Some(left.min(WRITE_WAIT)))?;
            let chunk = bytes.get(..WRITE_CHUNK).unwrap_or(bytes);
            match self.stream.write(chunk) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                // The socket is blocking, so this is the wait's timeout
                // firing, and the clock at the top of the loop says whether
                // that was the deadline.
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Shut the connection down both ways, which wakes the [`Reader`] parked
    /// in a blocking read on it.
    pub(crate) fn shutdown(&self) -> io::Result<()> {
        self.stream.shutdown(Shutdown::Both)
    }
}

/// Unlink the socket file, but only while it is still the one this listener
/// bound.
///
/// A second instance that started later has a socket of its own at the same
/// path, and unlinking that on the way out would leave it running with no way
/// for a bridge to reach it (the mirror of the takeover `clear_socket_path`
/// refuses). Device and inode are what separate the file this listener
/// created from a file that merely has the same name.
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

/// Make `path` absolute, resolving a relative path (including a bare
/// filename) against the current directory. Ensures the socket path always
/// has a real parent directory for [`ensure_private_dir`] to vet.
pub(crate) fn absolutize(path: PathBuf) -> io::Result<PathBuf> {
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
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
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
/// Unixes, fall back to [`uid_from_probe`] in the system temp dir.
///
/// Learned once per process and kept: only a success is kept, so a failed
/// attempt is made again on the next call. Two threads that call it first at
/// the same time may both learn it, which is harmless, since each probe has a
/// file of its own and both learn the same uid.
fn current_uid() -> io::Result<u32> {
    static UID: OnceLock<u32> = OnceLock::new();
    if let Some(&uid) = UID.get() {
        return Ok(uid);
    }
    let uid = match fs::metadata("/proc/self") {
        Ok(meta) => meta.uid(),
        Err(_) => uid_from_probe(&env::temp_dir())?,
    };
    Ok(*UID.get_or_init(|| uid))
}

/// The effective uid this process creates files with, from the owner of a
/// probe file it creates in `dir` and then removes.
///
/// Every call creates a file of its own, named by the pid and a counter, so
/// threads probing at once never meet on one name. The file is created with
/// `create_new`, so one already at that name (left by a killed run under a
/// reused pid, or planted) is never opened, read or removed: the call fails
/// instead, and the next call tries the next name.
fn uid_from_probe(dir: &Path) -> io::Result<u32> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let probe = dir.join(format!(
        "taria-uid-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let uid = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .and_then(|file| {
            let uid = file.metadata().map(|meta| meta.uid());
            drop(file);
            let _ = fs::remove_file(&probe);
            uid
        });
    uid.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "cannot learn this process's uid from a probe file at {}: {err}",
                probe.display()
            ),
        )
    })
}

/// Uid where available (via `/proc/self` on Linux), else `$USER`/`$LOGNAME`,
/// else a fixed fallback. Only used to namespace the temp-dir fallback path.
pub(crate) fn user_identity() -> String {
    if let Ok(meta) = fs::metadata("/proc/self") {
        return meta.uid().to_string();
    }
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    /// Threads of one process share its pid, and the probe was once named by
    /// the pid alone: binds made at once raced on one file, and one of them
    /// failed with a bare `File exists`. That made a few of the socket tests
    /// fail at random on macOS, where every bind probes. Each round releases
    /// eight probes together into one directory.
    #[test]
    fn probes_made_at_once_all_learn_the_uid() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 50;

        let dir = tempfile::tempdir().expect("create a probe dir");
        let expected = current_uid().expect("learn the uid");
        // A directory this process created is owned by its effective uid.
        let owner = fs::metadata(dir.path()).expect("stat the probe dir").uid();
        assert_eq!(expected, owner);
        if let Ok(meta) = fs::metadata("/proc/self") {
            assert_eq!(expected, meta.uid());
        }

        for round in 0..ROUNDS {
            let barrier = Barrier::new(THREADS);
            let results: Vec<io::Result<u32>> = thread::scope(|scope| {
                let probes: Vec<_> = (0..THREADS)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            uid_from_probe(dir.path())
                        })
                    })
                    .collect();
                probes
                    .into_iter()
                    .map(|probe| probe.join().expect("a probe thread panicked"))
                    .collect()
            });
            for result in results {
                match result {
                    Ok(uid) => assert_eq!(uid, expected, "round {round}"),
                    Err(err) => panic!("round {round}: a probe failed: {err}"),
                }
            }
        }

        let left: Vec<_> = fs::read_dir(dir.path())
            .expect("list the probe dir")
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<_>>()
            .expect("read the probe dir");
        assert!(left.is_empty(), "probe files left behind: {left:?}");
    }

    /// A probe that cannot be made says where it was made and what for, and
    /// keeps the kind of the error underneath.
    #[test]
    fn a_failed_probe_names_its_path_and_purpose() {
        let dir = tempfile::tempdir().expect("create a probe dir");
        let missing = dir.path().join("missing");
        let err = uid_from_probe(&missing).expect_err("a probe in a missing dir fails");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "cannot learn this process's uid from a probe file at {}",
                missing.join("taria-uid-probe-").display()
            )),
            "{message}"
        );
    }
}
