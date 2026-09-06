//! Core protocol types for taria, the agent accessibility layer for terminal UIs.
//!
//! A TUI app (via a framework adapter such as `taria-ratatui`) publishes a
//! [`Snapshot`] of its semantic state on every meaningful change. Agents read
//! snapshots and submit [`AgentInput`] back, through a transport such as the
//! `taria-mcp` bridge.
//!
//! Around those wire types sit three small modules both sides share, so the app
//! and the bridge cannot disagree about the same string: [`key`] parses the key
//! grammar, [`id`] builds and reads back prefixed node ids, and [`socket`]
//! resolves the socket path. All of them are pure; this crate performs no I/O.

mod action;
pub mod id;
pub mod key;
mod node;
mod snapshot;
pub mod socket;
pub mod wire;

pub use action::{Action, AgentInput};
pub use node::{Node, NodeId, Role};
pub use snapshot::Snapshot;

/// Protocol version, bumped on breaking changes to the wire format.
///
/// Version 1 is frozen. Within it, changes must be additive: new optional
/// fields and new message variants are fine, because peers skip lines they
/// cannot parse and serde ignores unknown fields. Changing or removing an
/// existing field, or its meaning, requires a bump. See [`wire`] for the full
/// rule.
pub const PROTOCOL_VERSION: u32 = 1;
