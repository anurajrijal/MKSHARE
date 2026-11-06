//! Finds MKShare servers on the local network (UDP broadcast), so the client does
//! not need the server's IP address typed in.
//!
//! The server answers a small query with its name and TCP port. Replies carry no
//! secret: connecting still requires the passphrase, and a fake server cannot
//! complete the encrypted handshake without it.

use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, UdpSocket},
    thread,
    time::{Duration, Instant},
};

pub const UDP_PORT: u16 = 24801;
const QUERY: &[u8] = b"MKSHARE-DISCOVER/1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Found {
    pub name: String,
    pub port: u16,
    pub version: String,
    /// Filled in by the scanner: the address the reply came from.
    #[serde(default)]
    pub addr: String,
}

/// This computer's name, as shown to other computers.
pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "computer".into())
}

/// Server side: answer discovery queries until the process ends.
pub fn spawn_responder(udp_port: u16, tcp_port: u16) {
    let sock = match UdpSocket::bind(("0.0.0.0", udp_port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Auto-discovery is off (UDP port {udp_port}: {e}). Clients must enter the address.");
            return;
        }
    };
    let reply = serde_json::to_vec(&Found {
        name: computer_name(),
        port: tcp_port,
        version: env!("CARGO_PKG_VERSION").into(),
        addr: String::new(),
    })
    .unwrap_or_default();
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if &buf[..n] == QUERY {
                let _ = sock.send_to(&reply, from);
            }
        }
    });
}

/// Addresses to ask: the global broadcast plus this network's /24 broadcast.
fn lan_targets() -> Vec<SocketAddr> {
    let mut t: Vec<SocketAddr> = vec![([255, 255, 255, 255], UDP_PORT).into()];
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                let o = a.ip().octets();
                t.push(([o[0], o[1], o[2], 255], UDP_PORT).into());
            }
        }
    }
    t
}

/// Client side: look for servers on the local network.
pub fn scan(timeout: Duration) -> Vec<Found> {
    scan_targets(&lan_targets(), timeout)
}

pub fn scan_targets(targets: &[SocketAddr], timeout: Duration) -> Vec<Found> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return vec![] };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let start = Instant::now();
    let mut found: Vec<Found> = Vec::new();
    let mut last_send: Option<Instant> = None;
    let mut buf = [0u8; 512];
    while start.elapsed() < timeout {
        // ask now, and once more half-way through (UDP can be lost)
        if last_send.map_or(true, |t| t.elapsed() > timeout / 2) {
            for t in targets {
                let _ = sock.send_to(QUERY, t);
            }
            last_send = Some(Instant::now());
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if let Ok(mut f) = serde_json::from_slice::<Found>(&buf[..n]) {
                f.addr = from.ip().to_string();
                if !found.iter().any(|x| x.addr == f.addr && x.port == f.port) {
                    found.push(f);
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_a_responder() {
        spawn_responder(34801, 24800);
        thread::sleep(Duration::from_millis(100));
        let found = scan_targets(&[([127, 0, 0, 1], 34801).into()], Duration::from_millis(600));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port, 24800);
        assert_eq!(found[0].addr, "127.0.0.1");
        assert!(!found[0].name.is_empty());
    }

    #[test]
    fn scan_with_no_server_is_empty() {
        let found = scan_targets(&[([127, 0, 0, 1], 34899).into()], Duration::from_millis(300));
        assert!(found.is_empty());
    }
}
