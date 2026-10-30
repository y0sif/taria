//! Tests that drive the socket manager and the server's tool methods against
//! fake apps, reaching into the crate for what the public API does not show:
//! the bridge's channels, its snapshots, and the tools' parameter types.
//! Tests that need only the binary live in the crate's `tests/` directory.

mod bridge_integration;
mod relayed;
mod released_restart;
