//! MCP bridge for taria: connects agent harnesses to a running
//! taria-enabled TUI app.
//!
//! The binary connects to the app on demand: on the first tool call, not at
//! startup, and it lets go of the app again once no tool call has run for an
//! idle window (`--idle-release`, 60 s by default). A taria adapter serves one
//! bridge at a time, so letting go is what lets several agent sessions share
//! one app; see [`server::TariaMcpServer::on_demand`]. [`bridge::spawn`], for a
//! caller wiring its own server, still connects at once and holds the app.
//!
//! The binary speaks MCP over stdio to the harness and the taria ndjson
//! protocol over a Unix domain socket to the app. It exposes four tools:
//! `read_tree` (latest semantic snapshot), `act` (invoke an advertised
//! action on a node), `type_text` (a literal string in one call), and `key`
//! (raw key fallback, with a repeat count).
//!
//! The tree reaches the agent as the app's own JSON line by default, or with
//! `--tree-format outline` as an indented outline rendered from that same
//! line (see [`tree_format`]). The wire format is the same either way.
//!
//! Every input the bridge sends carries an id, and the app answers it with an
//! ack, so a tool call can tell an input the app applied from one it ignored
//! or dropped rather than guessing from whether the tree happened to change.

#![warn(missing_docs)]

pub mod args;
pub mod bridge;
pub mod server;
pub mod tree_format;
