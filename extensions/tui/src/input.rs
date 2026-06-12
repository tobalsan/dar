//! Terminal input on a dedicated OS thread: crossterm's blocking
//! `event::poll`/`event::read` pumped into an async channel. Nothing else
//! consumes stdin while the TUI foreground owns the terminal.

use std::time::Duration;

use crossterm::event::Event;
use tokio::sync::mpsc::UnboundedSender;

/// How often the reader thread re-checks whether the receiver is gone.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn spawn_reader(tx: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        // The thread exits when the event loop drops its receiver (or the
        // terminal goes away); no join handle is needed.
        while !tx.is_closed() {
            match crossterm::event::poll(POLL_INTERVAL) {
                Ok(true) => {
                    let Ok(event) = crossterm::event::read() else {
                        return;
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                Ok(false) => {}
                Err(_) => return,
            }
        }
    });
}
