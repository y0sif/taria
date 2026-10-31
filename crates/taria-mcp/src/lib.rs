//! MCP bridge for taria: connects agent harnesses to a running
//! taria-enabled TUI app.
//!
//! The binary connects to the app on demand: on the first tool call, not at
//! startup, and it lets go of the app again once no tool call has run for an
//! idle window (`--idle-release`, 60 s by default). A taria adapter serves one
//! bridge at a time, so letting go is what lets several agent sessions share
//! one app; see [`TariaMcpServerBuilder::on_demand`].
//! [`TariaMcpServerBuilder::connected`], for a caller that wants the app held,
//! connects at once and never lets go.
//!
//! The binary speaks MCP over stdio to the harness and the taria ndjson
//! protocol over a Unix domain socket to the app. It exposes four tools:
//! `read_tree` (latest semantic snapshot), `act` (invoke an advertised
//! action on a node), `type_text` (a literal string in one call), and `key`
//! (raw key fallback, with a repeat count).
//!
//! The tree reaches the agent as the app's own JSON line by default, or with
//! `--tree-format outline` as an indented outline rendered from that same
//! line (see [`TreeFormat`]). The wire format is the same either way.
//!
//! Every input the bridge sends carries an id, and the app answers it with an
//! ack, so a tool call can tell an input the app applied from one it ignored
//! or dropped rather than guessing from whether the tree happened to change.
//!
//! The library is the server the binary runs: a [`TariaMcpServer`], set up
//! with [`TariaMcpServer::builder`] and built by the builder's
//! [`on_demand`](TariaMcpServerBuilder::on_demand) or
//! [`connected`](TariaMcpServerBuilder::connected), serves over any rmcp
//! transport.

#![warn(missing_docs)]

mod bridge;
mod server;
mod transport;
mod tree_format;

pub use server::{DEFAULT_IDLE_RELEASE, TariaMcpServer, TariaMcpServerBuilder};
pub use tree_format::TreeFormat;

#[cfg(test)]
mod tests;
