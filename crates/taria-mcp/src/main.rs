//! MCP bridge: connects agent harnesses to a running taria-enabled TUI app.
//!
//! Planned tools: `read_tree` (current semantic snapshot), `act` (invoke an
//! advertised action on a node), `key` (raw key fallback).

fn main() {
    eprintln!(
        "taria-mcp {} (protocol v{}): implementation lands with the v0 vertical slice",
        env!("CARGO_PKG_VERSION"),
        taria::PROTOCOL_VERSION
    );
}
