//! MCP bridge for taria: connects agent harnesses to a running
//! taria-enabled TUI app.
//!
//! The binary speaks MCP over stdio to the harness and the taria ndjson
//! protocol over a Unix domain socket to the app. It exposes three tools:
//! `read_tree` (latest semantic snapshot), `act` (invoke an advertised
//! action on a node), and `key` (raw key fallback).

pub mod args;
pub mod bridge;
pub mod server;
