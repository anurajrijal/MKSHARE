//! Encrypted, authenticated framing over TCP.
//!
//! The passphrase is stretched with PBKDF2 using a random per-connection salt
//! sent by the server; every frame is ChaCha20-Poly1305 with a counter nonce.
//! A client that does not know the passphrase cannot complete the handshake.

use crate::proto::Msg;
use anyhow::{anyhow, bail, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

const ROUNDS: u32 = 100_000;
const SALT_LEN: usize = 16;
const MAX_FRAME: usize = 4096;

pub struct Secure {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    send_dir: u8,
    recv_dir: u8,
    send_ctr: u64,
    recv_ctr: u64,
}

fn make_cipher(pass: &str, salt: &[u8]) -> ChaCha20Poly1305 {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pass.as_bytes(), salt, ROUNDS, &mut key);
    ChaCha20Poly1305::new(Key::from_slice(&key))
}

fn nonce(dir: u8, ctr: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = dir;
    n[4..].copy_from_slice(&ctr.to_le_bytes());
    Nonce::clone_from_slice(&n)
}

impl Secure {
    /// Server side of the handshake.
    pub fn accept(mut stream: TcpStream, pass: &str) -> Result<Self> {
        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt).map_err(|e| anyhow!("rng: {e}"))?;
        stream.write_all(&salt)?;
        let mut s = Secure {
            stream,
            cipher: make_cipher(pass, &salt),
            send_dir: 1,
            recv_dir: 0,
            send_ctr: 0,
            recv_ctr: 0,
        };
        match s.recv() {
            Ok(Msg::Hello) => Ok(s),
            _ => bail!("wrong passphrase or not an mkshare client"),
        }
    }

    /// Client side of the handshake.
    pub fn connect(mut stream: TcpStream, pass: &str) -> Result<Self> {
        let mut salt = [0u8; SALT_LEN];
        stream.read_exact(&mut salt)?;
        let mut s = Secure {
            stream,
            cipher: make_cipher(pass, &salt),
            send_dir: 0,
            recv_dir: 1,
            send_ctr: 0,
            recv_ctr: 0,
        };
        s.send(&Msg::Hello)?;
        Ok(s)
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(d)?;
        Ok(())
    }

    pub fn send(&mut self, m: &Msg) -> Result<()> {
        let pt = bincode::serialize(m)?;
        let ct = self
            .cipher
            .encrypt(&nonce(self.send_dir, self.send_ctr), pt.as_slice())
            .map_err(|_| anyhow!("encrypt failed"))?;
        self.send_ctr += 1;
        let mut buf = Vec::with_capacity(2 + ct.len());
        buf.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        buf.extend_from_slice(&ct);
        self.stream.write_all(&buf)?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Msg> {
        let mut len = [0u8; 2];
        self.stream.read_exact(&mut len)?;
        let len = u16::from_be_bytes(len) as usize;
        if len > MAX_FRAME {
            bail!("frame too large");
        }
        let mut ct = vec![0u8; len];
        self.stream.read_exact(&mut ct)?;
        let pt = self
            .cipher
            .decrypt(&nonce(self.recv_dir, self.recv_ctr), ct.as_slice())
            .map_err(|_| anyhow!("authentication failed"))?;
        self.recv_ctr += 1;
        Ok(bincode::deserialize(&pt)?)
    }
}
