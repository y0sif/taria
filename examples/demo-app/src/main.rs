//! taria-demo: a small task-manager TUI that an agent drives end to end
//! through taria's semantic tree — lists, tabs, a text input, and a
//! confirm-delete dialog, all reachable via semantic acts with a raw-key
//! fallback.

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
    // Bind before the terminal enters the alternate screen so the path is
    // printed to the primary buffer and stays visible after exit.
    let mut layer = TariaLayer::bind("taria-demo")?;
    eprintln!(
        "taria-demo: taria socket at {}",
        layer.socket_path().display()
    );

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut layer);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, layer: &mut TariaLayer) -> io::Result<()> {
    let mut app = App::new();
    app.socket_hint = layer.socket_path().display().to_string();
    let rx = setup_event_channel();

    while app.running {
        // Agent inputs are drained around the blocking recv below; the 50ms
        // tick bounds the latency between an agent act and its effect.
        while let Some(input) = layer.try_recv() {
            apply_agent_input(&mut app, input);
        }

        terminal.draw(|frame| view(&app, frame))?;

        // Publish the semantic tree for the exact state just drawn; the
        // layer dedups identical trees, so publishing every frame is free.
        let mut rec = layer.frame();
        for node in tree::build_nodes(&app) {
            rec.push(node);
        }
        rec.publish();

        match rx.recv() {
            Ok(event) => update(&mut app, event),
            Err(_) => break,
        }
        while let Some(input) = layer.try_recv() {
            apply_agent_input(&mut app, input);
        }
    }
    Ok(())
}
