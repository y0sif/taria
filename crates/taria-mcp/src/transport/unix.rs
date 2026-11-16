//! The Unix transport: a Unix domain socket at the path the bridge was given.
//!
//! A connect to a listening socket completes into the app's listen backlog
//! before the app accepts it ([`BACKLOG`]). The adapter serves one bridge at
//! a time, so a second bridge's connection opens and stays silent until the
//! first lets go: on Unix, silence past the grace is the app held by another
//! bridge, and nothing ever refuses a connect ([`refusal`]). A connect that
//! fails as [`nothing_listens`] reads it means the app has gone.
//!
//! [`peer_pid`] reads the pid the kernel records for the other end
//! (`SO_PEERCRED`, `LOCAL_PEERPID` on macOS), which for a connection still in
//! the backlog is the process listening at the path. The bridge keeps it to
//! tell one app instance from the next at the same path. On Linux the tool
//! layer also reads that process's state from `/proc/<pid>/stat`: a stopped
//! app keeps listening and accepts nothing, the same silence as a held one,
//! and is named instead with its pid and how to resume it. A job suspended
//! in its terminal needs the user's `fg`, a tracing stop the debugger, any
//! other stop `kill -CONT`. Elsewhere a stopped app reads as held.

use std::io;
use std::path::Path;

use super::Refusal;

/// A connection to the app.
pub(crate) type Stream = tokio::net::UnixStream;

/// A connect to a listening socket completes into its listen backlog before
/// the app accepts it, so a second bridge's connection opens and stays silent
/// until the first one leaves.
pub(crate) const BACKLOG: bool = true;

/// What the answers call the app's endpoint: a socket.
pub(crate) const ENDPOINT: &str = "socket";

/// What the answers call what the bridge was given to find the app by: a
/// socket path.
pub(crate) const ENDPOINT_ADDRESS: &str = "socket path";

/// Connect to the app's socket at `path`.
pub(crate) async fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(path).await
}

/// The pid of the process on the other end of `stream`, from `SO_PEERCRED`
/// (`LOCAL_PEERPID` on macOS), when the kernel names one.
///
/// For a connection the app has not accepted yet, the process listening at
/// the path. A pid of 0 is the kernel saying it cannot name the peer in this
/// pid namespace, which is no pid at all.
pub(crate) fn peer_pid(stream: &Stream) -> Option<i32> {
    stream
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid())
        .filter(|pid| *pid > 0)
}

/// True when a failed `connect` means nothing is listening at the path: a
/// socket file nobody is bound to any more (`ConnectionRefused`), or no file
/// at all (`NotFound`). Either way the app has gone. Any other failure (a
/// permission, a resource limit) says nothing about whether it is still there.
pub(crate) fn nothing_listens(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

/// Never: a socket that is listening takes every connect into its backlog,
/// and one that is not refuses it as [`nothing_listens`] reads it.
pub(crate) fn refusal(_err: &io::Error) -> Option<Refusal> {
    None
}

/// Any path will do: the kernel refuses what it cannot bind or reach, and
/// the length is checked where the path is chosen.
pub(crate) fn check_endpoint(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// The bridge works here, so there is nothing to refuse.
pub(crate) fn ensure_supported() -> io::Result<()> {
    Ok(())
}
