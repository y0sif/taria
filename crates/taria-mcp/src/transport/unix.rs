//! The Unix transport: a Unix domain socket at the path the bridge was given.

use std::io;
use std::path::Path;

/// A connection to the app.
pub(crate) type Stream = tokio::net::UnixStream;

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

/// The bridge works here, so there is nothing to refuse.
pub(crate) fn ensure_supported() -> io::Result<()> {
    Ok(())
}
