//! The platform half of the bridge's link to the app: how it connects, who is
//! on the other end, and which connect failures mean nothing is there.
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
//! - [`ensure_supported`], which the server builders call first, so a
//!   platform with no transport says so instead of serving tools that can
//!   only ever answer that no app is running.
//!
//! Unix has the real thing, a Unix domain socket. Windows has a stub for now:
//! [`ensure_supported`] and [`connect`] fail with
//! [`Unsupported`](std::io::ErrorKind::Unsupported).

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{connect, ensure_supported, nothing_listens, peer_pid};
#[cfg(windows)]
pub(crate) use windows::{connect, ensure_supported, nothing_listens, peer_pid};
