//! Server-side logic shared by all platforms: hotkey handling and routing
//! captured input either to the local machine or to the connected client.

use crate::{platform, proto::*};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
        Mutex,
    },
};

#[derive(Default)]
pub struct Core {
    remote: AtomicBool,
    next_id: AtomicU64,
    tx: Mutex<Option<(u64, Sender<Msg>)>>,
    held: Mutex<HashSet<u16>>,
    swallow: Mutex<HashSet<u16>>,
}

impl Core {
    pub fn is_remote(&self) -> bool {
        self.remote.load(Ordering::SeqCst)
    }

    fn send(&self, m: Msg) {
        if let Some((_, tx)) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(m);
        }
    }

    pub fn attach(&self, tx: Sender<Msg>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        *self.tx.lock().unwrap() = Some((id, tx));
        id
    }

    pub fn detach(&self, id: u64) {
        let mut g = self.tx.lock().unwrap();
        if g.as_ref().map(|(i, _)| *i) == Some(id) {
            *g = None;
            drop(g);
            if self.is_remote() {
                self.set_remote(false);
            }
        }
    }

    pub fn set_remote(&self, on: bool) {
        if on && self.tx.lock().unwrap().is_none() {
            eprintln!("No client connected - staying local.");
            return;
        }
        self.remote.store(on, Ordering::SeqCst);
        platform::on_switch(on);
        if !on {
            self.send(Msg::ReleaseAll);
        }
        println!("Input -> {}", if on { "REMOTE" } else { "LOCAL" });
    }

    /// Called for every physical input event. Returns `true` if the event must
    /// NOT be delivered to the local machine.
    pub fn on_event(&self, ev: InEvent) -> bool {
        match ev {
            InEvent::Key(code, down) => {
                let (ctrl, alt) = {
                    let mut h = self.held.lock().unwrap();
                    if down {
                        h.insert(code);
                    } else {
                        h.remove(&code);
                    }
                    (
                        h.contains(&KEY_LEFTCTRL) || h.contains(&KEY_RIGHTCTRL),
                        h.contains(&KEY_LEFTALT) || h.contains(&KEY_RIGHTALT),
                    )
                };
                if code == KEY_K {
                    let mut sw = self.swallow.lock().unwrap();
                    if down && ctrl && alt {
                        let first = sw.insert(KEY_K);
                        drop(sw);
                        if first {
                            self.set_remote(!self.is_remote());
                        }
                        return true;
                    }
                    if !down && sw.remove(&KEY_K) {
                        return true;
                    }
                }
                if self.is_remote() {
                    self.send(Msg::Key { code, down });
                    return true;
                }
                false
            }
            _ if !self.is_remote() => false,
            InEvent::Move(dx, dy) => {
                self.send(Msg::Move { dx, dy });
                true
            }
            InEvent::Button(btn, down) => {
                self.send(Msg::Button { btn, down });
                true
            }
            InEvent::Wheel(dx, dy) => {
                self.send(Msg::Wheel { dx, dy });
                true
            }
        }
    }
}
