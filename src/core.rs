//! Server-side logic shared by all platforms: the switch shortcut, screen-edge
//! switching, and routing captured input either to this machine or to the client.

use crate::{
    edge::{self, Rect, VirtualCursor},
    platform,
    proto::*,
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        mpsc::Sender,
        Mutex,
    },
    time::{Duration, Instant},
};

pub struct Core {
    hotkey: Hotkey,
    /// Side of THIS screen where the other computer sits (None = edge switching off).
    edge: Option<Side>,
    rect: Option<Rect>,
    remote: AtomicBool,
    next_id: AtomicU64,
    buttons_down: AtomicI32,
    tx: Mutex<Option<(u64, Sender<Msg>)>>,
    held: Mutex<HashSet<u16>>,
    swallow: Mutex<HashSet<u16>>,
    last_switch: Mutex<Instant>,
    /// Estimated cursor position where the real one cannot be read (Linux).
    vcursor: Mutex<Option<VirtualCursor>>,
}

impl Core {
    pub fn new(hotkey: Hotkey, edge: Option<Side>) -> Self {
        let rect = if edge.is_some() { platform::screen_rect() } else { None };
        let edge = match (edge, rect) {
            (Some(_), None) => {
                eprintln!("Screen-edge switching is off: could not detect the screen size (set MKSHARE_SCREEN=WIDTHxHEIGHT).");
                None
            }
            (e, _) => e,
        };
        Core {
            hotkey,
            edge,
            rect,
            remote: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            buttons_down: AtomicI32::new(0),
            tx: Mutex::new(None),
            held: Mutex::new(HashSet::new()),
            swallow: Mutex::new(HashSet::new()),
            last_switch: Mutex::new(Instant::now().checked_sub(Duration::from_secs(5)).unwrap_or_else(Instant::now)),
            vcursor: Mutex::new(None),
        }
    }

    pub fn edge(&self) -> Option<Side> {
        self.edge
    }

    pub fn is_remote(&self) -> bool {
        self.remote.load(Ordering::SeqCst)
    }

    /// Sends a message to the connected client (dropped if none is connected).
    pub fn send_msg(&self, m: Msg) {
        self.send(m);
    }

    fn send(&self, m: Msg) {
        if let Some((_, tx)) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(m);
        }
    }

    pub fn attach(&self, tx: Sender<Msg>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let _ = tx.send(Msg::EdgeConfig { back: self.edge.map(Side::opposite) });
        let _ = tx.send(Msg::Active(self.is_remote()));
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
        *self.last_switch.lock().unwrap() = Instant::now();
        platform::on_switch(on);
        if !on {
            self.send(Msg::ReleaseAll);
        }
        self.send(Msg::Active(on));
        println!("Input -> {}", if on { "REMOTE" } else { "LOCAL" });
    }

    // ------------------------------------------------------------ screen edge

    /// Called with the real cursor position while input is local (Windows).
    /// Returns true if input just moved to the client (the event must be swallowed).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn on_local_abs(&self, x: i32, y: i32) -> bool {
        self.edge_check(x, y)
    }

    /// Called with relative movement while input is local (Linux), where the real
    /// position is unknown and is estimated instead.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn on_local_delta(&self, dx: i32, dy: i32) -> bool {
        let Some(rect) = self.rect else { return false };
        if self.edge.is_none() {
            return false;
        }
        let (x, y) = {
            let mut g = self.vcursor.lock().unwrap();
            let vc = g.get_or_insert_with(|| VirtualCursor::new(rect));
            vc.apply(dx, dy);
            vc.pos()
        };
        self.edge_check(x, y)
    }

    fn edge_check(&self, x: i32, y: i32) -> bool {
        let (Some(side), Some(rect)) = (self.edge, self.rect) else { return false };
        // cheap test first: this runs for every mouse movement
        if !edge::at_edge(side, &rect, x, y) {
            return false;
        }
        if self.is_remote()
            || self.buttons_down.load(Ordering::SeqCst) > 0 // dragging something
            || self.last_switch.lock().unwrap().elapsed() < edge::COOLDOWN
            || self.tx.lock().unwrap().is_none()
        {
            return false;
        }
        let (ex, ey) = edge::enter_point(side.opposite(), edge::along(side, &rect, x, y));
        self.set_remote(true);
        if !self.is_remote() {
            return false;
        }
        self.send(Msg::Enter { x: ex, y: ey });
        true
    }

    /// The client reports that the cursor left through its return edge.
    pub fn leave_remote(&self, x: f32, y: f32) {
        if !self.is_remote() {
            return;
        }
        self.set_remote(false);
        if let Some(side) = self.edge {
            let along = match side {
                Side::Left | Side::Right => y,
                Side::Top | Side::Bottom => x,
            };
            let (px, py) = edge::enter_point(side, along);
            platform::warp(px, py);
            if let Some(vc) = self.vcursor.lock().unwrap().as_mut() {
                vc.set_frac(px, py);
            }
        }
    }

    /// Track mouse buttons while input is local so a drag is never interrupted.
    pub fn local_button(&self, down: bool) {
        if down {
            self.buttons_down.fetch_add(1, Ordering::SeqCst);
        } else {
            if self.buttons_down.fetch_sub(1, Ordering::SeqCst) <= 0 {
                self.buttons_down.store(0, Ordering::SeqCst); // an "up" without a seen "down"
            }
        }
    }

    // ------------------------------------------------------------------ input

    /// Called for every physical input event. Returns `true` if the event must
    /// NOT be delivered to the local machine.
    pub fn on_event(&self, ev: InEvent) -> bool {
        match ev {
            InEvent::Key(code, down) => {
                let (ctrl, alt, shift, meta) = {
                    let mut h = self.held.lock().unwrap();
                    if down {
                        h.insert(code);
                    } else {
                        h.remove(&code);
                    }
                    (
                        h.contains(&KEY_LEFTCTRL) || h.contains(&KEY_RIGHTCTRL),
                        h.contains(&KEY_LEFTALT) || h.contains(&KEY_RIGHTALT),
                        h.contains(&KEY_LEFTSHIFT) || h.contains(&KEY_RIGHTSHIFT),
                        h.contains(&KEY_LEFTMETA) || h.contains(&KEY_RIGHTMETA),
                    )
                };
                let hk = self.hotkey;
                if code == hk.key {
                    let mods_ok = (!hk.ctrl || ctrl) && (!hk.alt || alt) && (!hk.shift || shift) && (!hk.meta || meta);
                    let mut sw = self.swallow.lock().unwrap();
                    if down && mods_ok {
                        let first = sw.insert(hk.key);
                        drop(sw);
                        if first {
                            self.set_remote(!self.is_remote());
                        }
                        return true;
                    }
                    if !down && sw.remove(&hk.key) {
                        return true;
                    }
                }
                if self.is_remote() {
                    self.send(Msg::Key { code, down });
                    return true;
                }
                false
            }
            InEvent::Button(btn, down) => {
                self.local_button(down);
                if self.is_remote() {
                    self.send(Msg::Button { btn, down });
                    true
                } else {
                    false
                }
            }
            _ if !self.is_remote() => false,
            InEvent::Move(dx, dy) => {
                self.send(Msg::Move { dx, dy });
                true
            }
            InEvent::Wheel(dx, dy) => {
                self.send(Msg::Wheel { dx, dy });
                true
            }
        }
    }
}
