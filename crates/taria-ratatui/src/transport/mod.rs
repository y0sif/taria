//! The platform half of the layer: the endpoint an app is reached at, and the
//! connection each bridge gets on it.
//!
//! [`layer`](crate::layer) keeps everything that is the same on every
//! platform: the threads, the input and ack queues, generations and stamps,
//! the dedupe and the bounding of trees. What it needs from the platform is
//! small, and this module is that much:
//!
//! - a [`Listener`], bound with [`Listener::bind`] at the path the app asked
//!   for (resolved as [`Listener::path`] reports it), that accepts one bridge
//!   at a time with [`Listener::accept`], gives up once the layer's shutdown
//!   flag is set, says whether the bridge it hands over [`waited`](Accepted)
//!   behind the previous session, and cleans up after itself when dropped;
//! - a [`Connection`] per bridge, with a timeout that bounds its writes
//!   ([`Connection::set_write_timeout`]), split by [`Connection::split`] into
//!   a [`Reader`] for the reader thread and a [`Writer`] for the writer loop;
//! - a [`Reader`] that reads like any [`Read`](std::io::Read), and can also
//!   read just what is already waiting
//!   ([`Reader::with_nonblocking_reads`]), for input written before the hello;
//! - a [`Writer`] that writes a whole buffer by a deadline or fails
//!   ([`Writer::write_all_before`]), which bounds both every line and the
//!   flush of acks at shutdown, and whose [`Writer::shutdown`] wakes a
//!   [`Reader`] blocked on the same connection;
//! - [`user_identity`], which namespaces the default path between users.
//!
//! Unix has the real thing, a Unix domain socket in a private directory.
//! Windows has a stub for now: its [`Listener::bind`] fails with
//! [`Unsupported`](std::io::ErrorKind::Unsupported), so
//! [`TariaLayer::bind_or_disabled`](crate::TariaLayer::bind_or_disabled)
//! hands back a disabled layer that says why, and the app runs without taria.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{Connection, Listener, Reader, Writer, user_identity};
#[cfg(windows)]
pub(crate) use windows::{Connection, Listener, Reader, Writer, user_identity};

// The Unix transport's own checks, for the layer's tests of what binding
// refuses.
#[cfg(all(test, unix))]
pub(crate) use unix::{absolutize, ensure_private_dir};

/// A bridge [`Listener::accept`] handed over.
pub(crate) struct Accepted {
    /// The bridge's connection, ready to be served.
    pub(crate) connection: Connection,
    /// Whether this bridge connected while the previous session was still
    /// being served, and so waited behind it in the backlog. A client that
    /// waited has what it wrote before its hello thrown away; see
    /// [`LayerStats::stale_inputs`](crate::LayerStats::stale_inputs).
    pub(crate) waited: bool,
}
