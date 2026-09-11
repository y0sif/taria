//! Ratatui adapter for taria, the agent accessibility layer for terminal UIs.
//!
//! Embed a [`TariaLayer`] in your ratatui app and each rendered frame can also
//! publish a semantic [`taria::Snapshot`] over the taria transport (one JSON
//! message per line over a Unix domain socket). Agent inputs come back as
//! [`taria::AgentInput`] values your event loop handles like any other input
//! source.
//!
//! # Usage
//!
//! ```no_run
//! use ratatui::widgets::Paragraph;
//! use taria::{Node, Role};
//! use taria_ratatui::{InputStatus, TariaLayer, sem};
//!
//! fn main() -> std::io::Result<()> {
//!     // Never fails: if the socket cannot be bound the layer is inert and
//!     // the app runs exactly as it would without taria.
//!     let mut layer = TariaLayer::bind_or_disabled("my-app");
//!     let mut terminal = ratatui::init();
//!
//!     // Each render pass: record semantics alongside drawing, then publish.
//!     let rec = layer.frame();
//!     terminal.draw(|frame| {
//!         let widget = Paragraph::new("hello");
//!         let node = Node::new("greeting", Role::Text).label("hello");
//!         frame.render_widget(sem(&rec, widget, node), frame.area());
//!     })?;
//!     rec.publish();
//!
//!     // Drain agent input alongside terminal events. Each input is acked
//!     // `Delivered` as it is handed over; return `Ignored` for one you
//!     // looked at and deliberately did nothing with, and the layer sends it.
//!     layer.drain_acking(|_input| {
//!         // Apply to app state exactly like a keyboard event.
//!         InputStatus::Delivered
//!     });
//!
//!     ratatui::restore();
//!     // Only now the alternate screen is gone is printing safe.
//!     if let Some(err) = layer.bind_error() {
//!         eprintln!("taria disabled: {err}");
//!     }
//!     Ok(())
//! }
//! ```

mod key;
mod layer;
mod recorder;
mod semantic;
#[cfg(test)]
mod test_util;

pub use key::{text_to_keys, to_crossterm, to_crossterm_key};
pub use layer::TariaLayer;
pub use recorder::FrameRecorder;
pub use semantic::{Semantic, sem};

// Re-export the protocol crate so apps can depend on `taria-ratatui` alone,
// and the two ack types the layer's own signatures use, so acking an input
// needs no second import path.
pub use taria;
pub use taria::wire::{InputId, InputStatus};
