//! Clipboard text sharing.
//!
//! One background thread owns the system clipboard. It polls for changes made on
//! this computer (and hands the new text to the `send` callback) and applies text
//! received from the other computer. Text we set ourselves is remembered so it is
//! never echoed back (which would cause ping-pong between the two machines).

use std::{
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread,
    time::Duration,
};

/// Largest text that is shared (bytes). Must fit in one encrypted frame (see net.rs).
pub const MAX_TEXT: usize = 32 * 1024;

const POLL: Duration = Duration::from_millis(400);

/// Handle used to put text received from the other computer on this clipboard.
#[derive(Clone)]
pub struct Clipboard {
    tx: Sender<String>,
}

impl Clipboard {
    pub fn set(&self, text: String) {
        if text.len() <= MAX_TEXT {
            let _ = self.tx.send(text);
        }
    }
}

/// Starts the clipboard thread. `send` is called with every new text copied on this computer.
/// Returns None if the clipboard cannot be used here (e.g. no desktop session).
pub fn start(send: impl Fn(String) + Send + 'static) -> Option<Clipboard> {
    let (tx, rx) = mpsc::channel::<String>();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => {
                let _ = ready_tx.send(true);
                c
            }
            Err(e) => {
                eprintln!("Clipboard sharing is unavailable: {e}");
                let _ = ready_tx.send(false);
                return;
            }
        };
        // Whatever is on the clipboard now is not "new": only later changes are shared.
        let mut last = cb.get_text().unwrap_or_default();
        loop {
            match rx.recv_timeout(POLL) {
                Ok(text) => {
                    if text != last {
                        if cb.set_text(text.clone()).is_ok() {
                            last = text;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // The clipboard may be busy or hold something that is not text: just try again later.
                    if let Ok(text) = cb.get_text() {
                        if text != last {
                            last = text.clone();
                            if !text.is_empty() && text.len() <= MAX_TEXT {
                                send(text);
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    if ready_rx.recv().unwrap_or(false) {
        Some(Clipboard { tx })
    } else {
        None
    }
}
