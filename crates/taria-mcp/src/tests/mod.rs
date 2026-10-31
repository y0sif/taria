//! Tests that drive the socket manager and the server's tool methods against
//! fake apps, reaching into the crate for what the public API does not show:
//! the bridge's channels, its snapshots, and the tools' parameter types.
//! Tests that need only the binary live in the crate's `tests/` directory.

// The fake apps listen on Unix sockets, so these run where those exist.
#[cfg(unix)]
mod bridge_integration;
#[cfg(unix)]
mod relayed;
mod released_restart;
#[cfg(unix)]
mod stamps;
