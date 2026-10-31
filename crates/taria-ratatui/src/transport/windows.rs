//! The Windows transport, not written yet: a stub with the Unix transport's
//! shape, so that the workspace builds for Windows and an app built there runs
//! with taria disabled instead of failing to compile.
//!
//! [`Listener::bind`] always fails, with
//! [`Unsupported`](io::ErrorKind::Unsupported), so no other part of this
//! module is ever reached: every type in it is uninhabited, and each method is
//! an empty match on `self`. The transport this stands in for will serve a
//! named pipe, named by [`taria::socket::resolve_pipe_name`].

use std::env;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Accepted;

/// Never bound: see [`Listener::bind`].
pub(crate) enum Listener {}

impl Listener {
    /// Refuse to bind: this build has no Windows transport, so the layer
    /// comes up disabled and its `bind_error` says why.
    pub(crate) fn bind(path: PathBuf) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "taria cannot serve {} on Windows yet: this build has no Windows transport (named \
                 pipes are planned), so the app runs without taria",
                path.display()
            ),
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        match *self {}
    }

    pub(crate) fn accept(&mut self, _shutdown: impl Fn() -> bool) -> Option<Accepted> {
        match *self {}
    }
}

/// Never accepted, since no [`Listener`] is ever bound.
pub(crate) enum Connection {}

impl Connection {
    pub(crate) fn set_write_timeout(&self, _timeout: Duration) -> io::Result<()> {
        match *self {}
    }

    pub(crate) fn split(self) -> io::Result<(Reader, Writer)> {
        match self {}
    }
}

/// The reading half of a [`Connection`], which never exists.
pub(crate) enum Reader {}

impl Read for Reader {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        match *self {}
    }
}

impl Reader {
    pub(crate) fn with_nonblocking_reads<R>(
        &mut self,
        _f: impl FnOnce(&mut dyn Read) -> R,
    ) -> io::Result<R> {
        match *self {}
    }
}

/// The writing half of a [`Connection`], which never exists.
pub(crate) enum Writer {}

impl Writer {
    pub(crate) fn write_all_before(&mut self, _bytes: &[u8], _deadline: Instant) -> io::Result<()> {
        match *self {}
    }

    pub(crate) fn shutdown(&self) -> io::Result<()> {
        match *self {}
    }
}

/// `$USERNAME`, else `$USER`/`$LOGNAME`, else a fixed fallback, to namespace
/// the default path the stub reports in its error. The Windows transport will
/// name its pipe by the user's SID instead (see [`taria::socket::pipe_name`]).
pub(crate) fn user_identity() -> String {
    env::var("USERNAME")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "default".to_string())
}
