//! The Windows transport, not written yet: a stub with the Unix transport's
//! shape, so that the bridge builds for Windows and says it cannot run there
//! rather than failing to compile.
//!
//! The transport this stands in for will connect to a named pipe, named by
//! [`taria::socket::resolve_pipe_name`], which is why [`Stream`] is already a
//! pipe client.

use std::io;
use std::path::Path;

/// A connection to the app: a named pipe's client end.
pub(crate) type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

/// What every entry point here fails with.
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "taria-mcp cannot connect to an app on Windows yet: this build has no Windows transport \
         (named pipes are planned)",
    )
}

/// Refuse: there is no transport to connect with.
pub(crate) async fn connect(_path: &Path) -> io::Result<Stream> {
    Err(unsupported())
}

/// No pid: a pipe's server process will come from
/// `GetNamedPipeServerProcessId` once there is a pipe to ask.
pub(crate) fn peer_pid(_stream: &Stream) -> Option<i32> {
    None
}

/// True when a failed connect means no app is there. For a named pipe that
/// is a name nobody serves, `ERROR_FILE_NOT_FOUND`, which `std` reports as
/// `NotFound`; a pipe whose every instance is taken (`ERROR_PIPE_BUSY`) is
/// held, not gone.
pub(crate) fn nothing_listens(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::NotFound)
}

/// Refuse to build a server: with no transport, every tool call could only
/// answer that no app is running, which would send whoever reads it looking
/// for an app that is there.
pub(crate) fn ensure_supported() -> io::Result<()> {
    Err(unsupported())
}
