//! Ratatui adapter for taria.
//!
//! Wraps a ratatui app so that each rendered frame also publishes a semantic
//! [`taria::Snapshot`] over the taria transport, and agent inputs arrive as
//! events the app handles like any other input source.

// Implementation lands with the v0 vertical slice.
