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
//! # Shutting a connection down
//!
//! [`Writer::shutdown`] is called once the writer is done with the
//! connection, the acks flushed at shutdown included, and every transport
//! keeps the same three promises for it:
//!
//! - **What was written stays written.** Every byte a
//!   [`write_all_before`](Writer::write_all_before) finished putting on the
//!   connection is still readable by the bridge after the shutdown, and after
//!   the listener closes, until the bridge has read it. Nothing in the
//!   shutdown throws away what the bridge has not read yet, and nothing in it
//!   waits for the bridge to read it, which could wait forever.
//! - **It wakes the reader.** A [`Reader`] blocked in a read on the same
//!   connection returns end of stream (`Ok(0)`) at once.
//! - **It stays shut.** Every read started after it returns end of stream at
//!   once rather than waiting, so a reader that was between two reads when
//!   the shutdown came cannot start one that never ends. A one-shot wake-up,
//!   such as cancelling the read in flight, does not keep this promise by
//!   itself; a transport that uses one also sets something every read checks
//!   before it starts and waits on while it runs.
//!
//! Unix has it from the kernel: `shutdown(Both)` on the socket is sticky and
//! leaves data already sent readable. Windows has it from a closed event per
//! session, which every read and write checks and waits on, and from never
//! disconnecting a pipe at shutdown, only closing it.
//!
//! # The platforms
//!
//! Unix serves a Unix domain socket in a private directory. Windows serves a
//! named pipe, `\\.\pipe\taria-<SID>-<label>` by default, named by
//! [`taria::socket::resolve_pipe_name`], owned by the user and closed to
//! everyone else, one instance reused from one session to the next.

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
