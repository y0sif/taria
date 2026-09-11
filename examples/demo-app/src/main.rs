//! taria-demo: a small task-manager TUI that an agent drives end to end
//! through taria's semantic tree (lists, tabs, a text input, a confirm-delete
//! dialog, and the way out of the app), all reachable via semantic acts, with
//! raw keys as a fallback rather than as the way in. Typed text is not a
//! fallback: it goes to the field that accepts typing, and nowhere else (see
//! [`mod@update`]).

mod app;
mod events;
mod tree;
mod ui;
mod update;

use std::io;

use ratatui::DefaultTerminal;
use taria_ratatui::TariaLayer;

use app::App;
use events::setup_event_channel;
use ui::view;
use update::{apply_agent_input, update};

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
    // The other silent discard: inputs the layer threw away because the
    // bridge connection they were aimed at ended before this loop dequeued
    // them. Nothing on the agent side is left to hear about those, so the
    // person running the demo is the only one who can.
    let stale = layer.stale_inputs();
    if stale > 0 {
        eprintln!(
            "taria-demo: discarded {stale} agent input(s): the bridge connection they arrived on \
             ended first"
        );
    }
    // The third: inputs the layer answered `Ignored` on this app's behalf
    // because their kind is one this build of taria cannot read. The agent
    // was told, so nothing is lost on that side, but the reason is a taria
    // version gap that only whoever runs the app can close.
    let unknown = layer.unknown_inputs();
    if unknown > 0 {
        eprintln!(
            "taria-demo: could not read {unknown} agent input(s): the bridge speaks a newer \
             taria than this app; raise the app's taria dependency"
        );
    }
    // The fourth, and the one that points the other way: these inputs were
    // applied, but the answers to them were dropped because the bridge was
    // reading them slower than the app produced them. Every one of those is
    // an agent call that was answered nowhere and waited out a timeout, so it
    // belongs on this list even though the app itself lost nothing.
    //
    // Worded to start differently from the three above, because the e2e gate
    // recognizes each of those lines by its opening words.
    let acks = layer.dropped_acks();
    if acks > 0 {
        eprintln!(
            "taria-demo: lost the answer to {acks} agent input(s): the bridge read them slower \
             than the app answered them, so those agent calls waited out a timeout instead"
        );
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
/// `apply_agent_input` returns the status and `drain_acking` sends the ones
/// that refine it, so no input id passes through this app at all.
fn drain_agent_input(app: &mut App, layer: &TariaLayer) {
    layer.drain_acking(|input| apply_agent_input(app, input));
}
