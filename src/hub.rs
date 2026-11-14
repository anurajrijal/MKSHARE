//! The computers connected to the server and which one currently has the keyboard and mouse.
//!
//! Pure bookkeeping (no OS calls), so it can be unit-tested. Each computer that has connected
//! before keeps a fixed number 1..=9 (stored in `slots.json`), so "modifiers + digit" always
//! jumps to the same computer, also after it reconnects.

use crate::proto::{ClientInfo, Msg};
use crate::xfer::{AckTable, Link};
use std::{
    path::PathBuf,
    sync::{mpsc::Sender, Arc},
};

pub const MAX_SLOTS: usize = 9;

pub struct Client {
    pub id: u64,
    pub name: String,
    /// IP address the connection came from (used to recognise a computer that reconnects)
    pub addr: String,
    pub tx: Sender<Msg>,
    /// what this computer has acknowledged of the files we are sending it
    pub acks: Arc<AckTable>,
}

pub struct Hub {
    clients: Vec<Client>,
    active: Option<u64>,
    /// the computer used most recently: where the toggle shortcut and the screen edge lead
    last: Option<u64>,
    next_id: u64,
    slot_names: Vec<String>,
    slot_file: Option<PathBuf>,
}

impl Hub {
    pub fn new(slot_file: Option<PathBuf>) -> Self {
        let slot_names = slot_file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .take(MAX_SLOTS)
            .collect();
        Hub { clients: Vec::new(), active: None, last: None, next_id: 1, slot_names, slot_file }
    }

    fn save_slots(&self) {
        if let (Some(p), Ok(s)) = (&self.slot_file, serde_json::to_string(&self.slot_names)) {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, s);
        }
    }

    /// Two computers can have the same name: the second one becomes "name #2".
    fn unique_name(&self, name: &str, addr: &str) -> String {
        let base = if name.trim().is_empty() { "computer" } else { name.trim() };
        let taken = |n: &str| self.clients.iter().any(|c| c.name == n && c.addr != addr);
        let mut n = base.to_string();
        let mut i = 2;
        while taken(&n) {
            n = format!("{base} #{i}");
            i += 1;
        }
        n
    }

    /// Makes sure a connecting computer has a number; a full table frees the oldest name that is not connected.
    fn ensure_slot(&mut self, name: &str) {
        if self.slot_names.iter().any(|n| n == name) {
            return;
        }
        if self.slot_names.len() < MAX_SLOTS {
            self.slot_names.push(name.to_string());
        } else if let Some(i) = self.slot_names.iter().position(|n| !self.clients.iter().any(|c| &c.name == n)) {
            self.slot_names.remove(i);
            self.slot_names.push(name.to_string());
        } else {
            return; // nine computers are connected right now: this one gets no number
        }
        self.save_slots();
    }

    /// Registers a computer. If the same computer (same name and address) was already connected, the
    /// old connection is dropped and the new one takes over, including the keyboard and mouse.
    /// Returns the new id and the (possibly adjusted) name.
    pub fn add(&mut self, name: &str, addr: &str, tx: Sender<Msg>) -> (u64, String) {
        let name = self.unique_name(name, addr);
        let id = self.next_id;
        self.next_id += 1;
        if let Some(old) = self.clients.iter().position(|c| c.name == name && c.addr == addr) {
            let old_id = self.clients.remove(old).id;
            if self.active == Some(old_id) {
                self.active = Some(id);
            }
            if self.last == Some(old_id) {
                self.last = Some(id);
            }
        }
        self.ensure_slot(&name);
        self.clients.push(Client { id, name: name.clone(), addr: addr.to_string(), tx, acks: Arc::new(AckTable::default()) });
        (id, name)
    }

    /// How to send files to this computer.
    pub fn link(&self, id: u64) -> Option<Link> {
        self.clients.iter().find(|c| c.id == id).map(|c| Link::new(c.tx.clone(), c.acks.clone()))
    }

    /// Links to the computers a transfer is for: `only` (or everyone), without `except`.
    pub fn links(&self, only: Option<u64>, except: Option<u64>) -> Vec<(String, Link)> {
        self.clients
            .iter()
            .filter(|c| only.map_or(true, |o| c.id == o) && Some(c.id) != except)
            .map(|c| (c.name.clone(), Link::new(c.tx.clone(), c.acks.clone())))
            .collect()
    }

    /// Removes a computer. Returns true if it was the one that had the keyboard and mouse.
    pub fn remove(&mut self, id: u64) -> bool {
        self.clients.retain(|c| c.id != id);
        if self.last == Some(id) {
            self.last = None;
        }
        if self.active == Some(id) {
            self.active = None;
            return true;
        }
        false
    }

    pub fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }

    pub fn contains(&self, id: u64) -> bool {
        self.clients.iter().any(|c| c.id == id)
    }

    pub fn name_of(&self, id: u64) -> Option<&str> {
        self.clients.iter().find(|c| c.id == id).map(|c| c.name.as_str())
    }

    pub fn slot_of(&self, name: &str) -> Option<u8> {
        self.slot_names.iter().position(|n| n == name).map(|i| i as u8 + 1)
    }

    /// The connected computer that has this number (1..=9).
    pub fn id_for_slot(&self, slot: u8) -> Option<u64> {
        let name = self.slot_names.get((slot as usize).checked_sub(1)?)?;
        self.clients.iter().find(|c| &c.name == name).map(|c| c.id)
    }

    /// Where the toggle shortcut and the screen edge lead: the computer used last, else the first one.
    pub fn toggle_target(&self) -> Option<u64> {
        self.last.filter(|id| self.contains(*id)).or_else(|| self.clients.first().map(|c| c.id))
    }

    pub fn active(&self) -> Option<u64> {
        self.active
    }

    pub fn set_active(&mut self, id: Option<u64>) {
        self.active = id;
        if id.is_some() {
            self.last = id;
        }
    }

    pub fn send_to(&self, id: u64, m: Msg) {
        if let Some(c) = self.clients.iter().find(|c| c.id == id) {
            let _ = c.tx.send(m);
        }
    }

    /// To the computer that has the keyboard and mouse (nothing happens while it is local).
    pub fn send_active(&self, m: Msg) {
        if let Some(id) = self.active {
            self.send_to(id, m);
        }
    }

    pub fn broadcast(&self, m: Msg) {
        for c in &self.clients {
            let _ = c.tx.send(m.clone());
        }
    }

    pub fn broadcast_except(&self, id: u64, m: Msg) {
        for c in self.clients.iter().filter(|c| c.id != id) {
            let _ = c.tx.send(m.clone());
        }
    }

    pub fn info(&self) -> Vec<ClientInfo> {
        self.clients
            .iter()
            .map(|c| ClientInfo { id: c.id, name: c.name.clone(), slot: self.slot_of(&c.name), active: self.active == Some(c.id) })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    fn add(h: &mut Hub, name: &str, addr: &str) -> (u64, Receiver<Msg>) {
        let (tx, rx) = channel();
        let (id, _) = h.add(name, addr, tx);
        (id, rx)
    }

    #[test]
    fn computers_get_stable_numbers() {
        let mut h = Hub::new(None);
        let (a, _ra) = add(&mut h, "A", "10.0.0.1");
        let (b, _rb) = add(&mut h, "B", "10.0.0.2");
        assert_eq!(h.id_for_slot(1), Some(a));
        assert_eq!(h.id_for_slot(2), Some(b));
        // B leaves and comes back: it is still number 2
        h.remove(b);
        assert_eq!(h.id_for_slot(2), None);
        let (b2, _rb2) = add(&mut h, "B", "10.0.0.2");
        assert_eq!(h.id_for_slot(2), Some(b2));
        assert_eq!(h.slot_of("A"), Some(1));
        assert_eq!(h.id_for_slot(0), None);
        assert_eq!(h.id_for_slot(9), None);
    }

    #[test]
    fn same_name_on_another_address_is_renamed() {
        let mut h = Hub::new(None);
        let (_a, _ra) = add(&mut h, "PC", "10.0.0.1");
        let (b, _rb) = add(&mut h, "PC", "10.0.0.2");
        assert_eq!(h.name_of(b), Some("PC #2"));
        assert_eq!(h.info().len(), 2);
    }

    #[test]
    fn a_reconnecting_computer_replaces_its_old_connection_and_keeps_the_input() {
        let mut h = Hub::new(None);
        let (a, _ra) = add(&mut h, "A", "10.0.0.1");
        h.set_active(Some(a));
        let (a2, _ra2) = add(&mut h, "A", "10.0.0.1"); // same name and address
        assert_ne!(a, a2);
        assert_eq!(h.info().len(), 1);
        assert_eq!(h.active(), Some(a2));
        assert_eq!(h.toggle_target(), Some(a2));
    }

    #[test]
    fn toggle_target_is_the_last_used_computer() {
        let mut h = Hub::new(None);
        let (a, _ra) = add(&mut h, "A", "10.0.0.1");
        let (b, _rb) = add(&mut h, "B", "10.0.0.2");
        assert_eq!(h.toggle_target(), Some(a)); // nothing used yet: the first one
        h.set_active(Some(b));
        h.set_active(None); // back to local
        assert_eq!(h.toggle_target(), Some(b)); // still B
        assert!(!h.remove(b));
        assert_eq!(h.toggle_target(), Some(a)); // B is gone
        h.remove(a);
        assert_eq!(h.toggle_target(), None);
    }

    #[test]
    fn removing_the_active_computer_reports_it() {
        let mut h = Hub::new(None);
        let (a, _ra) = add(&mut h, "A", "10.0.0.1");
        let (b, _rb) = add(&mut h, "B", "10.0.0.2");
        h.set_active(Some(a));
        assert!(!h.remove(b));
        assert!(h.remove(a));
        assert_eq!(h.active(), None);
    }

    #[test]
    fn messages_go_to_the_right_computers() {
        let mut h = Hub::new(None);
        let (a, ra) = add(&mut h, "A", "10.0.0.1");
        let (_b, rb) = add(&mut h, "B", "10.0.0.2");
        h.send_active(Msg::Ping); // nobody active: dropped
        assert!(ra.try_recv().is_err() && rb.try_recv().is_err());
        h.set_active(Some(a));
        h.send_active(Msg::Ping);
        assert!(ra.try_recv().is_ok() && rb.try_recv().is_err());
        h.broadcast_except(a, Msg::Ping);
        assert!(ra.try_recv().is_err() && rb.try_recv().is_ok());
        h.broadcast(Msg::Ping);
        assert!(ra.try_recv().is_ok() && rb.try_recv().is_ok());
    }

    #[test]
    fn the_number_table_is_limited_to_nine_and_reuses_names_that_are_gone() {
        let mut h = Hub::new(None);
        let mut keep = Vec::new();
        for i in 0..9 {
            let (id, rx) = add(&mut h, &format!("PC{i}"), &format!("10.0.0.{i}"));
            keep.push((id, rx));
        }
        // all nine connected: a tenth gets no number
        let (_x, _rx) = add(&mut h, "EXTRA", "10.0.1.1");
        assert_eq!(h.slot_of("EXTRA"), None);
        // PC0 leaves: a new computer takes a freed number
        h.remove(keep[0].0);
        let (_y, _ry) = add(&mut h, "NEW", "10.0.1.2");
        assert!(h.slot_of("NEW").is_some());
        assert_eq!(h.slot_of("PC0"), None);
    }

    #[test]
    fn numbers_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("mkshare-hub-test-{}", std::process::id()));
        let file = dir.join("slots.json");
        {
            let mut h = Hub::new(Some(file.clone()));
            let (_a, _ra) = add(&mut h, "A", "10.0.0.1");
            let (_b, _rb) = add(&mut h, "B", "10.0.0.2");
        }
        let mut h = Hub::new(Some(file));
        let (b, _rb) = add(&mut h, "B", "10.0.0.2"); // B reconnects first after the restart
        assert_eq!(h.id_for_slot(2), Some(b));
        let _ = std::fs::remove_dir_all(dir);
    }
}
