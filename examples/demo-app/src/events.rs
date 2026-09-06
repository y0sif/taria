//! Terminal event channel (keyboard input, tick events).

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};

pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    Resize,
}

pub fn setup_event_channel() -> Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel::<AppEvent>();

    // Tick thread: fires every 50ms so the main loop drains agent input
    // even while the keyboard is idle.
    let tx_tick = tx.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(50));
            if tx_tick.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });

    // Input thread: blocks on crossterm events and forwards them.
    thread::spawn(move || {
        loop {
            match event::read() {
                // Only process key press events, not release/repeat on some
                // backends.
                Ok(Event::Key(key))
                    if key.kind == KeyEventKind::Press && tx.send(AppEvent::Key(key)).is_err() =>
                {
                    break;
                }
                Ok(Event::Resize(_, _)) if tx.send(AppEvent::Resize).is_err() => {
                    break;
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    rx
}
