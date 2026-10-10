//! taria-demo: a small task-manager TUI that an agent drives end to end
//! through taria's semantic tree (lists, tabs, a text input, a confirm-delete
//! dialog, and the way out of the app), all reachable via semantic acts, with
//! raw keys as a fallback rather than as the way in. Typed text is not a
//! fallback: it goes to the field that accepts typing, and nowhere else (see
//! [`mod@update`]).
//!
//! With `TARIA_DEMO_STATE=<path>` set, the demo also keeps its state as JSON
//! at that path (see [`mod@state`]), for harnesses that grade what an agent
//! did. Unset, nothing is written.

mod app;
mod events;
mod state;
mod tree;
mod ui;
mod update;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;

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
    let mut state_file = StateFile::from_env();
    let result = event_loop(terminal, layer, &mut app, &mut state_file);
    // On exit, however the loop ended, so the file holds the final state.
    sync_state(&mut state_file, &app);
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    layer: &mut TariaLayer,
    app: &mut App,
    state_file: &mut Option<StateFile>,
) -> io::Result<()> {
    let rx = setup_event_channel();

    while app.running {
        // Agent inputs are drained around the blocking recv below; the 50ms
        // tick bounds the latency between an agent act and its effect.
        drain_agent_input(app, layer);
        // Before the draw and the publish, so a harness that sees the effect
        // on screen or in the tree finds it in the file too.
        sync_state(state_file, app);

        terminal.draw(|frame| view(app, frame))?;

        // Publish the semantic tree for the exact state just drawn; the
        // layer dedups identical trees, so publishing every frame is free.
        layer.publish(tree::build_nodes(app));

        match rx.recv() {
            Ok(event) => update(app, event),
            Err(_) => break,
        }
        drain_agent_input(app, layer);
        sync_state(state_file, app);
    }
    Ok(())
}

/// Where the state dump goes, when a harness asked for one with
/// `TARIA_DEMO_STATE`, and the last contents written there.
///
/// Unset or empty means no file at all and no behaviour change: the dump is
/// for benchmarks and tests, never for the person running the demo.
struct StateFile {
    path: PathBuf,
    written: Option<String>,
}

impl StateFile {
    fn from_env() -> Option<Self> {
        std::env::var_os("TARIA_DEMO_STATE")
            .filter(|path| !path.is_empty())
            .map(|path| Self {
                path: PathBuf::from(path),
                written: None,
            })
    }
}

/// Write the state dump if the state changed since the last write.
///
/// Written whole to `<path>.tmp` and renamed over `<path>`, so a reader never
/// sees half a file. A failed write is ignored and retried on the next change
/// check: the dump serves a harness, and the app must not fail, print over its
/// own screen, or stop for one.
fn sync_state(state_file: &mut Option<StateFile>, app: &App) {
    let Some(file) = state_file else { return };
    let Ok(json) = state::to_json(app) else {
        return;
    };
    if file.written.as_deref() == Some(json.as_str()) {
        return;
    }
    let mut tmp = OsString::from(file.path.as_os_str());
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if fs::write(&tmp, &json).is_ok() && fs::rename(&tmp, &file.path).is_ok() {
        file.written = Some(json);
    }
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
