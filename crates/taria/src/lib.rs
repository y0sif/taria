//! Core protocol types for taria, the agent accessibility layer for terminal UIs.
//!
//! A TUI app (via a framework adapter such as `taria-ratatui`) publishes a
//! [`Snapshot`] of its semantic state on every meaningful change. Agents read
//! snapshots and submit [`AgentInput`] back, through a transport such as the
//! `taria-mcp` bridge.

mod action;
mod node;
mod snapshot;

pub use action::{Action, AgentInput};
pub use node::{Node, NodeId, Role};
pub use snapshot::Snapshot;

/// Protocol version, bumped on breaking changes to the wire format.
pub const PROTOCOL_VERSION: u32 = 0;
