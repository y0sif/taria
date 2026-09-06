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
//! use taria_ratatui::{TariaLayer, sem};
//!
//! fn main() -> std::io::Result<()> {
//!     let mut layer = TariaLayer::bind("my-app")?;
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
//!     // Poll agent input alongside terminal events.
//!     if let Some(_input) = layer.try_recv() {
//!         // Apply to app state exactly like a keyboard event.
//!     }
//!
//!     ratatui::restore();
//!     Ok(())
//! }
//! ```

mod key;
mod layer;
mod recorder;
mod semantic;

pub use key::to_crossterm_key;
pub use layer::TariaLayer;
pub use recorder::FrameRecorder;
pub use semantic::{Semantic, sem};

// Re-export the protocol crate so apps can depend on `taria-ratatui` alone.
pub use taria;
