//! The platform half of the bridge's link to the app: how it connects, who is
//! on the other end, and what a failed connect says about the app.
//!
//! [`bridge`](crate::bridge) keeps everything that is the same on every
//! platform. It serves whatever [`connect`] returns as a plain
//! `AsyncRead + AsyncWrite` stream, split with `tokio::io::split`, so a new
//! platform needs only what this module names:
//!
//! - [`connect`], which opens a connection at the path the bridge was given,
//!   as the platform's own stream type;
//! - [`peer_pid`], the process on the other end where the platform can name
//!   it, which is how the bridge tells a stopped app from a held one and one
//!   app from the next at the same path;
//! - [`nothing_listens`], which picks out the connect failures that mean no
//!   app is there at all;
//! - [`refusal`], which picks out the ones that mean something is there and
//!   turned this bridge away, as a [`Refusal`];
//! - [`BACKLOG`], whether a connect can open before the app accepts it, which
//!   decides what a connection that stays silent means;
//! - [`ENDPOINT`] and [`ENDPOINT_ADDRESS`], what the answers call the app's
//!   endpoint and what the bridge was given to find it by: a socket and a
//!   socket path on Unix, a pipe and a pipe name on Windows;
//! - [`check_endpoint`], which refuses a path the platform's transport must
//!   never open, before a server is built around it;
//! - [`ensure_supported`], which the server builders call first, so a
//!   platform with no transport says so instead of serving tools that can
//!   only ever answer that no app is running.
//!
//! Unix connects to a Unix domain socket; Windows to a named pipe, named by
//! [`taria::socket::resolve_pipe_name`].

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_sid;

#[cfg(unix)]
pub(crate) use unix::{
    BACKLOG, ENDPOINT, ENDPOINT_ADDRESS, check_endpoint, connect, ensure_supported,
    nothing_listens, peer_pid, refusal,
};
#[cfg(all(test, windows))]
pub(crate) use windows::testing;
#[cfg(windows)]
pub(crate) use windows::{
    BACKLOG, ENDPOINT, ENDPOINT_ADDRESS, check_endpoint, connect, ensure_supported,
    nothing_listens, peer_pid, refusal,
};

/// Why something listening at the path turned a connect away, so that no
/// connection is open although the app, or something posing as it, is there.
///
/// Only the Windows transport refuses this way. On Unix a second bridge's
/// connect opens into the app's listen backlog instead, and reads as an open
/// connection that stays silent; there is no owner to check either, since the
/// socket lives in a directory only its user can reach.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The app's endpoint is serving another client: on Windows a pipe whose
    /// one instance is taken (`ERROR_PIPE_BUSY`). A taria adapter serves one
    /// bridge at a time, so the app is held by another bridge, and this one
    /// gets in on a later retry, once the holder lets go.
    Busy,
    /// The endpoint at the path is not the app's: on Windows a pipe whose
    /// owner is not this user's SID, or one that refused this user access,
    /// which the app's own pipe never does. Pipe names are machine-wide, and
    /// any process can create one under the app's name first. The bridge
    /// parsed nothing it sent and wrote nothing to it.
    Foreign,
}
