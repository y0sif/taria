//! taria-demo: a small task-manager TUI that an agent drives end to end
//! through taria's semantic tree (lists, tabs, a text input, and a
//! confirm-delete dialog), all reachable via semantic acts, with raw keys and
//! typed text as fallbacks rather than as the way in.

mod app;
mod events;
mod tree;
mod ui;
mod update;

use std::io;

use ratatui::DefaultTerminal;
use taria_ratatui::{InputStatus, TariaLayer};

use app::App;
use events::setup_event_channel;
use ui::view;
use update::{Applied, apply_agent_input, update};

fn main() -> io::Result<()> {
    // Binding never fails the app: taria being unavailable is a reason to run
    // without it, not a reason not to run.
    let mut layer = TariaLayer::bind_or_disabled("taria-demo");
    // Both lines print here, before `ratatui::init()` takes the alternate
    // screen: a print after that lands on a screen the TUI owns, garbling the
    // frame, and disappears with the alternate screen on exit. Printed to the
    // primary buffer it is still there when the demo quits, which is where an
    // agent operator goes looking for the socket path.
    match layer.bind_error() {
        Some(err) => eprintln!("taria-demo: taria disabled: {err}"),
        None => eprintln!(
            "taria-demo: taria socket at {}",
            layer.socket_path().display()
        ),
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut layer);
    ratatui::restore();

    // Safe to print again only now the alternate screen is gone. A nonzero
    // count means agent inputs arrived faster than this loop drained them.
    let dropped = layer.dropped_inputs();
    if dropped > 0 {
        eprintln!("taria-demo: dropped {dropped} agent input(s): the app could not keep up");
    }
    result
}

fn run(terminal: &mut DefaultTerminal, layer: &mut TariaLayer) -> io::Result<()> {
    let mut app = App::new();
    app.socket_hint = if layer.is_enabled() {
        layer.socket_path().display().to_string()
    } else {
        "taria disabled".to_string()
    };
    let rx = setup_event_channel();

    while app.running {
        // Agent inputs are drained around the blocking recv below; the 50ms
        // tick bounds the latency between an agent act and its effect.
        drain_agent_input(&mut app, layer);

        terminal.draw(|frame| view(&app, frame))?;

        // Publish the semantic tree for the exact state just drawn; the
        // layer dedups identical trees, so publishing every frame is free.
        layer.publish(tree::build_nodes(&app));

        match rx.recv() {
            Ok(event) => update(&mut app, event),
            Err(_) => break,
        }
        drain_agent_input(&mut app, layer);
    }
    Ok(())
}

/// Apply every queued agent input, refining the ack of each one the app
/// deliberately ignored.
///
/// The layer acks `Delivered` as it hands an input over, which only says the
/// event loop dequeued it. `Ignored` is the follow-up that tells an agent
/// waiting on an effect that no effect is coming, and last ack wins.
fn drain_agent_input(app: &mut App, layer: &TariaLayer) {
    layer.drain_with_ids(|id, input| {
        if apply_agent_input(app, input) == Applied::Ignored {
            layer.ack(id, InputStatus::Ignored);
        }
    });
}
