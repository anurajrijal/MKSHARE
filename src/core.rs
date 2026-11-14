//! Server-side logic shared by all platforms: the switch shortcuts, screen-edge
//! switching, and routing captured input either to this machine or to one of the
//! connected computers.

use crate::{
    edge::{self, Rect, VirtualCursor},
    hub::Hub,
    platform,
    proto::*,
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::Sender,
        Mutex,
    },
    time::{Duration, Instant},
};

/// evdev key codes of the digit keys 1..=9 (KEY_1 is 2).
fn slot_for_key(code: u16) -> Option<u8> {
    (2..=10).contains(&code).then(|| (code - 1) as u8)
}

pub struct Core {
    hotkey: Hotkey,
    /// Side of THIS screen where the other computer sits (None = edge switching off).
    edge: Option<Side>,
    rect: Option<Rect>,
    /// true while one of the connected computers has the keyboard and mouse
    remote: AtomicBool,
    buttons_down: AtomicI32,
    hub: Mutex<Hub>,
    /// where received files go
    dirs: Mutex<crate::xfer::Dirs>,
    held: Mutex<HashSet<u16>>,
    swallow: Mutex<HashSet<u16>>,
    last_switch: Mutex<Instant>,
    /// Estimated cursor position where the real one cannot be read (Linux).
    vcursor: Mutex<Option<VirtualCursor>>,
}

impl Core {
    pub fn new(hotkey: Hotkey, edge: Option<Side>) -> Self {
        let slot_file = crate::secret::config_dir().map(|d| d.join("slots.json"));
        Self::with_hub(hotkey, edge, Hub::new(slot_file))
    }

    pub(crate) fn with_hub(hotkey: Hotkey, edge: Option<Side>, hub: Hub) -> Self {
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
            buttons_down: AtomicI32::new(0),
            hub: Mutex::new(hub),
            dirs: Mutex::new(crate::xfer::Dirs::standard()),
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

    /// Input events and control messages for the computer that has the keyboard and mouse.
    fn send(&self, m: Msg) {
        self.hub.lock().unwrap().send_active(m);
    }

    /// To every connected computer (used for the server's clipboard).
    pub fn send_msg(&self, m: Msg) {
        self.hub.lock().unwrap().broadcast(m);
    }

    /// To every connected computer except `from` (a client's clipboard is passed on to the others).
    pub fn relay(&self, from: u64, m: Msg) {
        self.hub.lock().unwrap().broadcast_except(from, m);
    }

    /// Where received files go.
    pub fn dirs(&self) -> crate::xfer::Dirs {
        self.dirs.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub(crate) fn set_dirs(&self, d: crate::xfer::Dirs) {
        *self.dirs.lock().unwrap() = d;
    }

    /// How to send files to the computer with this id.
    pub fn link(&self, id: u64) -> Option<crate::xfer::Link> {
        self.hub.lock().unwrap().link(id)
    }

    /// Sends a copied image to every connected computer (except `except`, the one it came from).
    pub fn send_image(&self, png: Vec<u8>, except: Option<u64>) {
        let png = std::sync::Arc::new(png);
        for (peer, link) in self.hub.lock().unwrap().links(None, except) {
            crate::xfer::spawn_send_image(link, peer, png.clone());
        }
    }

    /// Sends files to `to` (or to every connected computer if None), except `except`.
    pub fn send_files(&self, paths: Vec<std::path::PathBuf>, kind: XferKind, to: Option<u64>, except: Option<u64>) {
        for (peer, link) in self.hub.lock().unwrap().links(to, except) {
            crate::xfer::spawn_send_files(link, peer, paths.clone(), kind);
        }
    }

    /// Tells the app which computers are connected (one JSON line the app reads).
    fn print_clients(&self) {
        let info = self.hub.lock().unwrap().info();
        if let Ok(json) = serde_json::to_string(&info) {
            println!("Clients: {json}");
        }
    }

    /// A computer connected. Returns its id and name (made unique if another computer has the same name).
    pub fn attach(&self, tx: Sender<Msg>, name: &str, addr: &str) -> (u64, String) {
        let (id, name) = {
            let mut hub = self.hub.lock().unwrap();
            let (id, name) = hub.add(name, addr, tx);
            hub.send_to(id, Msg::EdgeConfig { back: self.edge.map(Side::opposite) });
            hub.send_to(id, Msg::Active(hub.active() == Some(id)));
            (id, name)
        };
        self.print_clients();
        (id, name)
    }

    pub fn detach(&self, id: u64) {
        let was_active = self.hub.lock().unwrap().remove(id);
        if was_active {
            self.finish_local();
        }
        self.print_clients();
    }

    // ---------------------------------------------------------------- switching

    /// Marks input as local again (the computer that had it is already told, or gone).
    fn finish_local(&self) {
        self.remote.store(false, Ordering::SeqCst);
        *self.last_switch.lock().unwrap() = Instant::now();
        platform::on_switch(false);
        println!("Input -> LOCAL");
    }

    /// Gives the keyboard and mouse back to this computer.
    pub fn go_local(&self) {
        let prev = {
            let mut hub = self.hub.lock().unwrap();
            let Some(prev) = hub.active() else { return };
            hub.set_active(None);
            prev
        };
        self.finish_local();
        let hub = self.hub.lock().unwrap();
        hub.send_to(prev, Msg::ReleaseAll);
        hub.send_to(prev, Msg::Active(false));
        drop(hub);
        self.print_clients();
    }

    /// Sends the keyboard and mouse to this computer (from here or from another computer).
    pub fn switch_to(&self, id: u64) {
        let (prev, name) = {
            let mut hub = self.hub.lock().unwrap();
            if !hub.contains(id) || hub.active() == Some(id) {
                return;
            }
            let prev = hub.active();
            if let Some(p) = prev {
                hub.send_to(p, Msg::ReleaseAll);
                hub.send_to(p, Msg::Active(false));
            }
            hub.set_active(Some(id));
            hub.send_to(id, Msg::Active(true));
            (prev, hub.name_of(id).unwrap_or("").to_string())
        };
        self.remote.store(true, Ordering::SeqCst);
        *self.last_switch.lock().unwrap() = Instant::now();
        if prev.is_none() {
            platform::on_switch(true);
        }
        println!("Input -> REMOTE ({name})");
        self.print_clients();
    }

    /// Jumps to the computer with this number (the digit keys 1-9).
    pub fn switch_slot(&self, slot: u8) {
        let id = self.hub.lock().unwrap().id_for_slot(slot);
        match id {
            Some(id) => self.switch_to(id),
            None => eprintln!("No computer with number {slot} is connected."),
        }
    }

    /// Sends the input to the computer used last (or the only one). False if nobody is connected.
    fn go_remote(&self) -> bool {
        let target = self.hub.lock().unwrap().toggle_target();
        match target {
            Some(id) => {
                self.switch_to(id);
                self.is_remote()
            }
            None => {
                eprintln!("No client connected - staying local.");
                false
            }
        }
    }

    /// The switch shortcut / button: between this computer and the computer used last.
    pub fn toggle(&self) {
        if self.is_remote() {
            self.go_local();
        } else {
            self.go_remote();
        }
    }

    /// A computer asks for the keyboard and mouse, or hands them back.
    pub fn toggle_for_client(&self, id: u64) {
        let active = self.hub.lock().unwrap().active();
        if active == Some(id) {
            self.go_local();
        } else {
            self.switch_to(id);
        }
    }

    // ------------------------------------------------------------ screen edge

    /// Called with the real cursor position while input is local (Windows).
    /// Returns true if input just moved to a computer (the event must be swallowed).
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
            || !self.hub.lock().unwrap().has_clients()
        {
            return false;
        }
        let (ex, ey) = edge::enter_point(side.opposite(), edge::along(side, &rect, x, y));
        if !self.go_remote() {
            return false;
        }
        self.send(Msg::Enter { x: ex, y: ey });
        true
    }

    /// The computer that has the input reports that the cursor left through its return edge.
    pub fn leave_remote(&self, from: u64, x: f32, y: f32) {
        if self.hub.lock().unwrap().active() != Some(from) {
            return; // not the computer that has the input
        }
        self.go_local();
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
                let mods_ok = (!hk.ctrl || ctrl) && (!hk.alt || alt) && (!hk.shift || shift) && (!hk.meta || meta);
                // the switch shortcut itself: between this computer and the computer used last
                if code == hk.key {
                    let mut sw = self.swallow.lock().unwrap();
                    if down && mods_ok {
                        let first = sw.insert(hk.key);
                        drop(sw);
                        if first {
                            self.toggle();
                        }
                        return true;
                    }
                    if !down && sw.remove(&hk.key) {
                        return true;
                    }
                }
                // the same modifiers + a digit key jump straight to that computer
                if let Some(slot) = slot_for_key(code).filter(|_| code != hk.key && hk.has_modifier()) {
                    let mut sw = self.swallow.lock().unwrap();
                    if down && mods_ok {
                        let first = sw.insert(code);
                        drop(sw);
                        if first {
                            self.switch_slot(slot);
                        }
                        return true;
                    }
                    if !down && sw.remove(&code) {
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

// These tests drive the real routing code; on Linux `platform::on_switch` does nothing, so no
// device is touched. (On Windows it would move the cursor, so they are not built there.)
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    fn core() -> Core {
        Core::with_hub(Hotkey::DEFAULT, None, Hub::new(None))
    }

    fn connect(c: &Core, name: &str, addr: &str) -> (u64, Receiver<Msg>) {
        let (tx, rx) = channel();
        let (id, _) = c.attach(tx, name, addr);
        (id, rx)
    }

    fn drain(rx: &Receiver<Msg>) -> Vec<Msg> {
        rx.try_iter().collect()
    }

    fn press(c: &Core, code: u16) {
        c.on_event(InEvent::Key(code, true));
        c.on_event(InEvent::Key(code, false));
    }

    #[test]
    fn input_goes_only_to_the_computer_that_has_it() {
        let c = core();
        let (a, ra) = connect(&c, "A", "10.0.0.1");
        let (b, rb) = connect(&c, "B", "10.0.0.2");
        drain(&ra);
        drain(&rb);
        // local: events stay on this computer
        assert!(!c.on_event(InEvent::Move(1, 1)));
        c.switch_to(b);
        assert!(c.is_remote());
        assert!(c.on_event(InEvent::Move(5, 6)));
        assert!(drain(&ra).is_empty());
        let got = drain(&rb);
        assert!(matches!(got.first(), Some(Msg::Active(true))));
        assert!(got.iter().any(|m| matches!(m, Msg::Move { dx: 5, dy: 6 })));
        // jump from B straight to A: B is released and told it is no longer active
        c.switch_to(a);
        let b_msgs = drain(&rb);
        assert!(b_msgs.iter().any(|m| matches!(m, Msg::ReleaseAll)));
        assert!(b_msgs.iter().any(|m| matches!(m, Msg::Active(false))));
        assert!(drain(&ra).iter().any(|m| matches!(m, Msg::Active(true))));
        assert!(c.on_event(InEvent::Key(30, true)));
        assert!(drain(&ra).iter().any(|m| matches!(m, Msg::Key { code: 30, down: true })));
        assert!(drain(&rb).is_empty());
    }

    #[test]
    fn digit_shortcuts_jump_to_a_numbered_computer() {
        let c = core(); // Ctrl+Alt+K
        let (_a, ra) = connect(&c, "A", "10.0.0.1");
        let (_b, rb) = connect(&c, "B", "10.0.0.2");
        drain(&ra);
        drain(&rb);
        c.on_event(InEvent::Key(KEY_LEFTCTRL, true));
        c.on_event(InEvent::Key(KEY_LEFTALT, true));
        press(&c, 3); // Ctrl+Alt+2 -> computer B
        assert!(c.is_remote());
        assert!(drain(&rb).iter().any(|m| matches!(m, Msg::Active(true))));
        assert!(drain(&ra).is_empty());
        press(&c, 2); // Ctrl+Alt+1 -> computer A
        assert!(drain(&ra).iter().any(|m| matches!(m, Msg::Active(true))));
        press(&c, 37); // Ctrl+Alt+K -> back to this computer
        assert!(!c.is_remote());
        press(&c, 37); // and again -> the computer used last (A)
        assert!(c.is_remote());
        // a digit without the modifiers is ordinary typing
        c.on_event(InEvent::Key(KEY_LEFTCTRL, false));
        c.on_event(InEvent::Key(KEY_LEFTALT, false));
        c.go_local();
        assert!(!c.on_event(InEvent::Key(3, true)));
        assert!(!c.is_remote());
    }

    #[test]
    fn the_toggle_shortcut_does_nothing_without_a_client() {
        let c = core();
        c.toggle();
        assert!(!c.is_remote());
        let (_a, _ra) = connect(&c, "A", "10.0.0.1");
        c.toggle();
        assert!(c.is_remote());
        c.toggle();
        assert!(!c.is_remote());
    }

    #[test]
    fn losing_the_active_computer_returns_the_input_here() {
        let c = core();
        let (a, _ra) = connect(&c, "A", "10.0.0.1");
        let (b, _rb) = connect(&c, "B", "10.0.0.2");
        c.switch_to(a);
        c.detach(b); // another computer leaving does not matter
        assert!(c.is_remote());
        c.detach(a);
        assert!(!c.is_remote());
        assert!(!c.on_event(InEvent::Move(1, 1)));
    }

    #[test]
    fn only_the_active_computer_can_hand_the_input_back() {
        let c = core();
        let (a, _ra) = connect(&c, "A", "10.0.0.1");
        let (b, _rb) = connect(&c, "B", "10.0.0.2");
        c.switch_to(a);
        c.leave_remote(b, 0.5, 0.5); // B does not have the input: ignored
        assert!(c.is_remote());
        c.leave_remote(a, 0.5, 0.5);
        assert!(!c.is_remote());
    }

    #[test]
    fn a_client_can_ask_for_the_input_and_hand_it_back() {
        let c = core();
        let (a, _ra) = connect(&c, "A", "10.0.0.1");
        let (b, _rb) = connect(&c, "B", "10.0.0.2");
        c.toggle_for_client(b);
        assert!(c.is_remote());
        c.toggle_for_client(a); // A asks while B has it: A gets it
        assert_eq!(c.hub.lock().unwrap().active(), Some(a));
        c.toggle_for_client(a); // A asks again: back here
        assert!(!c.is_remote());
    }

    #[test]
    fn clipboard_text_is_passed_on_to_the_other_computers_only() {
        let c = core();
        let (a, ra) = connect(&c, "A", "10.0.0.1");
        let (_b, rb) = connect(&c, "B", "10.0.0.2");
        drain(&ra);
        drain(&rb);
        c.relay(a, Msg::Clipboard("hello".into()));
        assert!(drain(&ra).is_empty());
        assert!(matches!(drain(&rb).first(), Some(Msg::Clipboard(t)) if t == "hello"));
        c.send_msg(Msg::Clipboard("from server".into()));
        assert_eq!(drain(&ra).len(), 1);
        assert_eq!(drain(&rb).len(), 1);
    }

    #[test]
    fn a_reconnecting_computer_keeps_the_input() {
        let c = core();
        let (a, _ra) = connect(&c, "A", "10.0.0.1");
        c.switch_to(a);
        let (a2, ra2) = connect(&c, "A", "10.0.0.1");
        assert!(c.is_remote());
        assert_eq!(c.hub.lock().unwrap().active(), Some(a2));
        // the new connection is told it has the input
        assert!(drain(&ra2).iter().any(|m| matches!(m, Msg::Active(true))));
    }
}

impl Core {
    /// (id, name) of the connected computers - for tests.
    #[cfg(test)]
    pub(crate) fn client_ids(&self) -> Vec<(u64, String)> {
        self.hub.lock().unwrap().info().into_iter().map(|c| (c.id, c.name)).collect()
    }
}
