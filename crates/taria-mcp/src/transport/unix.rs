//! The Unix transport: a Unix domain socket at the path the bridge was given.

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
