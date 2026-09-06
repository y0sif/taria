//! MCP bridge for taria: connects agent harnesses to a running
//! taria-enabled TUI app.
//!
//! The binary speaks MCP over stdio to the harness and the taria ndjson
//! protocol over a Unix domain socket to the app. It exposes four tools:
//! `read_tree` (latest semantic snapshot), `act` (invoke an advertised
//! action on a node), `type_text` (a literal string in one call), and `key`
//! (raw key fallback, with a repeat count).
//!
//! Every input the bridge sends carries an id, and the app answers it with an
//! ack, so a tool call can tell an input the app applied from one it ignored
//! or dropped rather than guessing from whether the tree happened to change.

pub mod args;
pub mod bridge;
pub mod server;
