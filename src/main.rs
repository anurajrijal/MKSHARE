#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod clipboard;
mod core;
mod discovery;
mod edge;
mod gui;
mod hub;
mod net;
mod platform;
mod proto;
mod secret;
#[cfg(target_os = "linux")]
mod setup;
mod tray;
mod update;
mod xfer;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use proto::{Hotkey, Msg, Side};
use std::{
    io::BufRead,
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(name = "mkshare", version, about = "Share one keyboard and mouse between machines (Windows/Linux). Run without arguments to open the app.")]
struct Cli {
    /// Used by "start on login": open the app minimized (to the tray on Windows)
    #[arg(long, hide = true)]
    background: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Linux, run as root: let the user logged in at this computer use the input devices
    /// (the app's "Fix permissions" button runs this).
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    SetupLinux,
    /// Run on the machine that has the physical keyboard and mouse.
    /// Press the switch shortcut (default Ctrl+Alt+K) to switch input between this machine and the client.
    Server {
        #[arg(long, default_value_t = 24800)]
        port: u16,
        /// Shared passphrase (or set MKSHARE_PASS)
        #[arg(long, env = "MKSHARE_PASS")]
        pass: String,
        /// Switch shortcut, e.g. "ctrl+alt+k", "ctrl+shift+f9" or "f12"
        #[arg(long, default_value = "ctrl+alt+k")]
        hotkey: String,
        /// Switch by moving the mouse off this screen edge: left, right, top or bottom
        /// (the side where the other computer sits). Off by default.
        #[arg(long)]
        edge: Option<String>,
        /// Do not share the clipboard (copied text, images, files) with the clients
        #[arg(long)]
        no_clipboard: bool,
        /// Share copied text only: no images
        #[arg(long)]
        no_images: bool,
        /// Share copied text and images only: no copied files
        #[arg(long)]
        no_files: bool,
        /// Used by the app: read "toggle" commands from stdin, exit when stdin closes
        #[arg(long, hide = true)]
        stdin_control: bool,
    },
    /// Run on the machine that should receive the keyboard and mouse.
    Client {
        /// IP address or hostname of the server
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 24800)]
        port: u16,
        #[arg(long, env = "MKSHARE_PASS")]
        pass: String,
        /// Server name: if the address stops working, look for the server by this name
        #[arg(long)]
        name: Option<String>,
        /// Do not share the clipboard (copied text, images, files) with the server
        #[arg(long)]
        no_clipboard: bool,
        /// Share copied text only: no images
        #[arg(long)]
        no_images: bool,
        /// Share copied text and images only: no copied files
        #[arg(long)]
        no_files: bool,
        /// Used by the app: exit when stdin closes
        #[arg(long, hide = true)]
        stdin_control: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(std::env::args_os());
    match cli.cmd {
        None => gui::run(cli.background),
        Some(cmd) => {
            #[cfg(windows)]
            attach_console();
            match cmd {
                #[cfg(target_os = "linux")]
                Cmd::SetupLinux => setup::run().map_err(|e| anyhow!("{e:#}")),
                Cmd::Server { port, pass, hotkey, edge, no_clipboard, no_images, no_files, stdin_control } => {
                    let hotkey = Hotkey::parse(&hotkey).map_err(|e| anyhow!("Error: invalid --hotkey: {e}"))?;
                    let edge = match edge.as_deref() {
                        None | Some("off") | Some("") => None,
                        Some(s) => Some(Side::parse(s).ok_or_else(|| anyhow!("Error: --edge must be left, right, top, bottom or off"))?),
                    };
                    run_server(port, pass, stdin_control, hotkey, edge, Share::new(!no_clipboard, !no_images, !no_files))
                }
                Cmd::Client { host, port, pass, name, no_clipboard, no_images, no_files, stdin_control } => {
                    if stdin_control {
                        watch_stdin(None);
                    }
                    run_client(&host, port, &pass, name, Share::new(!no_clipboard, !no_images, !no_files))
                }
            }
        }
    }
}

/// The Windows build is a GUI-subsystem program (no console window when
/// double-clicked). For command-line use, connect to the parent's console.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::Console::{AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE},
    };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            return; // output is already redirected (e.g. piped by the app)
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            let con = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if con != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_OUTPUT_HANDLE, con);
                SetStdHandle(STD_ERROR_HANDLE, con);
            }
        }
    }
}

/// Sender for requests from this client to the server (set while connected).
static CLIENT_TX: Mutex<Option<mpsc::Sender<Msg>>> = Mutex::new(None);
/// How to send files to the server (set while connected).
static CLIENT_LINK: Mutex<Option<xfer::Link>> = Mutex::new(None);

/// What is shared through the clipboard.
#[derive(Clone, Copy)]
struct Share {
    /// false: nothing at all
    enabled: bool,
    opts: clipboard::Options,
}

impl Share {
    fn new(enabled: bool, images: bool, files: bool) -> Self {
        Share { enabled, opts: clipboard::Options { images, files } }
    }
}

/// The app asks to send files: `sendfile {"to": 3, "paths": ["..."]}` (`to` null = every computer).
#[derive(serde::Deserialize)]
struct SendFile {
    to: Option<u64>,
    paths: Vec<String>,
}

fn watch_stdin(core: Option<Arc<core::Core>>) {
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            // (a byte-order mark can come first when another program writes the line)
            let line = line.trim_start_matches('\u{feff}').trim();
            if let Some(json) = line.strip_prefix("sendfile ") {
                if let Ok(sf) = serde_json::from_str::<SendFile>(json) {
                    let paths: Vec<std::path::PathBuf> = sf.paths.into_iter().map(Into::into).collect();
                    let (files, notes) = xfer::usable_files(&paths);
                    for n in notes {
                        println!("Not sent: {n}");
                    }
                    if files.is_empty() {
                        continue;
                    }
                    match &core {
                        Some(c) => c.send_files(files, proto::XferKind::Drop, sf.to, None),
                        None => match CLIENT_LINK.lock().unwrap().clone() {
                            Some(link) => xfer::spawn_send_files(link, "server".into(), files, proto::XferKind::Drop),
                            None => println!("Not sent: not connected to the server"),
                        },
                    }
                }
                continue;
            }
            match (&core, line) {
                // server: switch directly
                (Some(c), "toggle") => c.toggle(),
                (Some(c), "local") => c.go_local(),
                (Some(c), _) if line.starts_with("switch ") => {
                    if let Ok(id) = line["switch ".len()..].trim().parse::<u64>() {
                        c.switch_to(id);
                    }
                }
                // client: ask the server to switch
                (None, "toggle") => {
                    if let Some(tx) = CLIENT_TX.lock().unwrap().as_ref() {
                        let _ = tx.send(Msg::Toggle);
                    }
                }
                _ => {}
            }
        }
        std::process::exit(0); // the app that started us is gone
    });
}

// ---------------------------------------------------------------- server

fn run_server(port: u16, pass: String, stdin_control: bool, hotkey: Hotkey, edge: Option<Side>, share: Share) -> Result<()> {
    let core = Arc::new(core::Core::new(hotkey, edge));
    xfer::clean_inbox(&core.dirs().inbox, Duration::from_secs(24 * 3600));
    let clip = if share.enabled {
        let c = core.clone();
        let cb = clipboard::start(share.opts, move |ev| match ev {
            clipboard::ClipEvent::Text(t) => c.send_msg(Msg::Clipboard(t)),
            clipboard::ClipEvent::Image(png) => c.send_image(png, None),
            clipboard::ClipEvent::Files(p) => c.send_files(p, proto::XferKind::File, None, None),
            clipboard::ClipEvent::Notice(s) => println!("Clipboard: {s}"),
        });
        if cb.is_some() {
            println!("Clipboard sharing is on.");
        }
        cb
    } else {
        None
    };
    if stdin_control {
        watch_stdin(Some(core.clone()));
    }
    let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("cannot bind port {port}"))?;
    discovery::spawn_responder(discovery::UDP_PORT, port);
    println!("Listening on port {port}. Switch input with {} once a client is connected.", hotkey.pretty());
    if let Some(side) = core.edge() {
        println!("Screen-edge switching is on: the other computer is on the {} side.", side.name());
    }
    {
        let core = core.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (core, pass, clip) = (core.clone(), pass.clone(), clip.clone());
                thread::spawn(move || {
                    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    if let Err(e) = serve(core, stream, &pass, &peer, clip, share.opts) {
                        eprintln!("Connection from {peer} ended: {e:#}");
                    }
                });
            }
        });
    }
    platform::capture(core) // blocks forever
}

fn serve(core: Arc<core::Core>, stream: TcpStream, pass: &str, peer: &str, clip: Option<clipboard::Clipboard>, opts: clipboard::Options) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let addr = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| peer.to_string());
    let mut ch = net::Secure::accept(stream, pass)?;
    // The computer introduces itself with its name right after the handshake.
    let name = match ch.recv() {
        Ok(Msg::Name(n)) => n,
        _ => addr.clone(),
    };
    ch.set_read_timeout(None)?;

    let (tx, rx) = mpsc::channel::<Msg>();
    let (id, name) = core.attach(tx, &name, &addr);
    println!("Client connected: {name} ({peer})");

    // Requests coming from the client (e.g. "switch input") are handled on their own thread.
    let mut reader = ch.try_clone()?;
    let reader_core = core.clone();
    let link = core.link(id).context("the computer disconnected right away")?;
    let reader_name = name.clone();
    thread::spawn(move || {
        // files and images this computer sends to us
        let mut receiver = xfer::Receiver::new(reader_core.dirs(), {
            let tx = link.tx.clone();
            move |m| {
                let _ = tx.send(m);
            }
        });
        while let Ok(msg) = reader.recv() {
            match msg {
                Msg::Toggle => reader_core.toggle_for_client(id),
                Msg::Leave { x, y } => reader_core.leave_remote(id, x, y),
                Msg::Clipboard(text) => {
                    // copied on this computer: put it on the server's clipboard and pass it on to the other computers
                    if let Some(c) = &clip {
                        c.set(text.clone());
                    }
                    reader_core.relay(id, Msg::Clipboard(text));
                }
                // answers to what we are sending to this computer
                Msg::XferAck { id: xid, bytes } => link.acks.ack(xid, bytes),
                Msg::XferCancel { id: xid, reason } => {
                    link.acks.cancel(xid, reason.clone());
                    receiver.handle(Msg::XferCancel { id: xid, reason });
                }
                // something this computer sends to us
                m @ (Msg::XferBegin { .. } | Msg::XferData { .. } | Msg::XferEnd { .. }) => {
                    for done in xfer::receive(&mut receiver, m, &reader_name) {
                        match done {
                            xfer::Done::Image(png) => {
                                if let (Some(c), true) = (&clip, opts.images) {
                                    c.set_image(png.clone());
                                }
                                reader_core.send_image(png, Some(id)); // pass it on to the other computers
                            }
                            xfer::Done::Files(paths) => {
                                if let (Some(c), true) = (&clip, opts.files) {
                                    c.set_files(paths.clone());
                                }
                                reader_core.send_files(paths, proto::XferKind::File, None, Some(id));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        receiver.abort_all(); // the connection ended: remove half-received files
    });

    let result = loop {
        let msg = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => Msg::Ping, // keepalive
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()), // replaced by a newer connection of the same computer
        };
        if let Err(e) = ch.send(&msg) {
            break Err(e);
        }
    };
    core.detach(id);
    ch.shutdown(); // also ends the reader thread
    println!("Client disconnected: {name} ({peer})");
    result
}

// ---------------------------------------------------------------- client

fn run_client(host: &str, port: u16, pass: &str, name: Option<String>, share: Share) -> Result<()> {
    let mut injector = platform::Injector::new()?;
    let dirs = xfer::Dirs::standard();
    xfer::clean_inbox(&dirs.inbox, Duration::from_secs(24 * 3600));
    let clip = if share.enabled {
        clipboard::start(share.opts, |ev| match ev {
            clipboard::ClipEvent::Text(text) => {
                if let Some(tx) = CLIENT_TX.lock().unwrap().as_ref() {
                    let _ = tx.send(Msg::Clipboard(text));
                }
            }
            clipboard::ClipEvent::Image(png) => {
                if let Some(link) = CLIENT_LINK.lock().unwrap().clone() {
                    xfer::spawn_send_image(link, "server".into(), Arc::new(png));
                }
            }
            clipboard::ClipEvent::Files(paths) => {
                if let Some(link) = CLIENT_LINK.lock().unwrap().clone() {
                    xfer::spawn_send_files(link, "server".into(), paths, proto::XferKind::File);
                }
            }
            clipboard::ClipEvent::Notice(s) => println!("Clipboard: {s}"),
        })
    } else {
        None
    };
    let (mut host, mut port) = (host.to_string(), port);
    println!("Connecting to {host}:{port} ...");
    let mut failures = 0;
    loop {
        match client_session(&host, port, pass, &mut injector, clip.as_ref(), share.opts, &dirs) {
            Ok(()) => failures = 0,
            Err(e) => {
                eprintln!("Disconnected: {e:#}");
                failures += 1;
            }
        }
        injector.release_all();
        // The server's address may have changed (new IP from the router): look for it by name.
        if failures >= 2 {
            if let Some(name) = &name {
                if let Some(f) = discovery::scan(Duration::from_millis(1500)).into_iter().find(|f| &f.name == name) {
                    if f.addr != host || f.port != port {
                        println!("Found {name} at {}:{}", f.addr, f.port);
                        (host, port) = (f.addr, f.port);
                    }
                    failures = 0;
                }
            }
        }
        thread::sleep(Duration::from_secs(3));
    }
}

fn client_session(
    host: &str,
    port: u16,
    pass: &str,
    inj: &mut platform::Injector,
    clip: Option<&clipboard::Clipboard>,
    opts: clipboard::Options,
    dirs: &xfer::Dirs,
) -> Result<()> {
    let stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?; // handshake
    let mut ch = net::Secure::connect(stream, pass)?;
    ch.set_read_timeout(Some(Duration::from_secs(10)))?; // server pings every 2s
    println!("Connected to {host}:{port}");
    ch.send(&Msg::Name(discovery::computer_name()))?; // tells the server which computer this is

    // Requests to the server (e.g. "switch input") are sent from their own thread.
    let mut writer = ch.try_clone()?;
    let (tx, rx) = mpsc::channel::<Msg>();
    // files and images: what the server has acknowledged, and what the server sends us
    let acks = Arc::new(xfer::AckTable::default());
    let mut receiver = xfer::Receiver::new(dirs.clone(), {
        let tx = tx.clone();
        move |m| {
            let _ = tx.send(m);
        }
    });
    *CLIENT_LINK.lock().unwrap() = Some(xfer::Link::new(tx.clone(), acks.clone()));
    *CLIENT_TX.lock().unwrap() = Some(tx);
    thread::spawn(move || {
        while let Ok(m) = rx.recv() {
            if writer.send(&m).is_err() {
                break;
            }
        }
    });

    let mut edge = edge::ClientEdge::new(platform::screen_rect());
    let result = (|| loop {
        match ch.recv()? {
            Msg::Ping | Msg::Hello | Msg::Toggle | Msg::Leave { .. } | Msg::Name(_) => {}
            Msg::Active(on) => {
                edge.set_active(on);
                println!("Active: {}", if on { "on" } else { "off" });
            }
            Msg::EdgeConfig { back } => {
                edge.set_config(back);
                if back.is_some() && !edge.can_track() {
                    eprintln!("Moving back by the screen edge is unavailable here: screen size unknown (set MKSHARE_SCREEN=WIDTHxHEIGHT).");
                }
            }
            Msg::Clipboard(text) => {
                if let Some(c) = clip {
                    c.set(text);
                }
            }
            // answers to what we are sending to the server
            Msg::XferAck { id, bytes } => acks.ack(id, bytes),
            Msg::XferCancel { id, reason } => {
                acks.cancel(id, reason.clone());
                receiver.handle(Msg::XferCancel { id, reason });
            }
            // something the server sends to us
            m @ (Msg::XferBegin { .. } | Msg::XferData { .. } | Msg::XferEnd { .. }) => {
                for done in xfer::receive(&mut receiver, m, "server") {
                    match done {
                        xfer::Done::Image(png) => {
                            if let (Some(c), true) = (clip, opts.images) {
                                c.set_image(png);
                            }
                        }
                        xfer::Done::Files(paths) => {
                            if let (Some(c), true) = (clip, opts.files) {
                                c.set_files(paths);
                            }
                        }
                    }
                }
            }
            Msg::Enter { x, y } => {
                platform::warp(x, y);
                edge.entered(x, y);
            }
            Msg::Move { dx, dy } => {
                if let Some((x, y)) = edge.check_leave(dx, dy, platform::cursor_pos()) {
                    // pushed against the edge that leads back to the server: hand input back
                    if let Some(tx) = CLIENT_TX.lock().unwrap().as_ref() {
                        let _ = tx.send(Msg::Leave { x, y });
                    }
                } else {
                    inj.apply(&Msg::Move { dx, dy });
                    edge.moved(dx, dy);
                }
            }
            m => inj.apply(&m),
        }
    })();
    *CLIENT_TX.lock().unwrap() = None;
    *CLIENT_LINK.lock().unwrap() = None;
    receiver.abort_all(); // remove half-received files
    ch.shutdown();
    result
}

/// End-to-end: the real server loop and real encrypted connections, two computers at once.
/// (Linux only: switching input on Windows moves the real cursor.)
#[cfg(all(test, target_os = "linux"))]
mod e2e {
    use super::*;
    use crate::{hub::Hub, proto::Hotkey};

    fn connect(addr: std::net::SocketAddr, name: &str) -> net::Secure {
        let mut ch = net::Secure::connect(TcpStream::connect(addr).unwrap(), "pw").unwrap();
        ch.send(&Msg::Name(name.into())).unwrap();
        ch.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        ch
    }

    /// Reads messages until one matches (keepalive pings are skipped).
    fn expect(ch: &mut net::Secure, what: &str, pred: impl Fn(&Msg) -> bool) -> Msg {
        for _ in 0..40 {
            let m = ch.recv().unwrap_or_else(|e| panic!("waiting for {what}: {e}"));
            if pred(&m) {
                return m;
            }
        }
        panic!("never received {what}");
    }

    #[test]
    fn two_computers_share_one_server() {
        let core = Arc::new(core::Core::with_hub(Hotkey::DEFAULT, None, Hub::new(None)));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        {
            let core = core.clone();
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let core = core.clone();
                    thread::spawn(move || {
                        let _ = serve(core, stream, "pw", "test", None, clipboard::Options { images: true, files: true });
                    });
                }
            });
        }

        let mut a = connect(addr, "Laptop");
        let mut b = connect(addr, "Desktop");
        for _ in 0..100 {
            if core.client_ids().len() == 2 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let ids = core.client_ids();
        assert_eq!(ids.len(), 2, "both computers should be connected");
        let id_of = |n: &str| ids.iter().find(|(_, name)| name == n).unwrap().0;
        let (id_a, id_b) = (id_of("Laptop"), id_of("Desktop"));

        // each computer first hears the edge setting and that it is not active
        expect(&mut a, "A: Active(false)", |m| matches!(m, Msg::Active(false)));
        expect(&mut b, "B: Active(false)", |m| matches!(m, Msg::Active(false)));

        // the server sends the input to B: only B hears it
        core.switch_to(id_b);
        expect(&mut b, "B: Active(true)", |m| matches!(m, Msg::Active(true)));
        assert!(core.on_event(crate::proto::InEvent::Move(3, 4)));
        expect(&mut b, "B: Move", |m| matches!(m, Msg::Move { dx: 3, dy: 4 }));

        // A asks for the input over its connection: B is released, A gets it
        a.send(&Msg::Toggle).unwrap();
        expect(&mut b, "B: Active(false)", |m| matches!(m, Msg::Active(false)));
        expect(&mut a, "A: Active(true)", |m| matches!(m, Msg::Active(true)));
        assert!(core.on_event(crate::proto::InEvent::Key(30, true)));
        expect(&mut a, "A: Key", |m| matches!(m, Msg::Key { code: 30, down: true }));

        // text copied on A reaches B (through the server), but is not sent back to A
        a.send(&Msg::Clipboard("copied on the laptop".into())).unwrap();
        let m = expect(&mut b, "B: Clipboard", |m| matches!(m, Msg::Clipboard(_)));
        assert!(matches!(m, Msg::Clipboard(t) if t == "copied on the laptop"));

        // A hands the input back: this computer has it again
        a.send(&Msg::Toggle).unwrap();
        expect(&mut a, "A: Active(false)", |m| matches!(m, Msg::Active(false)));
        assert!(!core.is_remote());
        let _ = id_a;
    }

    // ------------------------------------------------ files and images

    /// A raw client that can send and receive transfers (the same structure the real client has).
    struct Raw {
        link: xfer::Link,
        /// everything that is not an answer to our own transfers
        inbox: mpsc::Receiver<Msg>,
    }

    impl Raw {
        fn start(ch: net::Secure) -> Raw {
            let mut writer = ch.try_clone().unwrap();
            let mut reader = ch;
            reader.set_read_timeout(None).unwrap();
            let (tx, rx) = mpsc::channel::<Msg>();
            let acks = Arc::new(xfer::AckTable::default());
            thread::spawn(move || {
                while let Ok(m) = rx.recv() {
                    if writer.send(&m).is_err() {
                        break;
                    }
                }
            });
            let (in_tx, inbox) = mpsc::channel::<Msg>();
            let a = acks.clone();
            thread::spawn(move || {
                while let Ok(m) = reader.recv() {
                    match m {
                        Msg::XferAck { id, bytes } => a.ack(id, bytes),
                        Msg::XferCancel { id, reason } => a.cancel(id, reason),
                        Msg::Ping => {}
                        other => {
                            let _ = in_tx.send(other);
                        }
                    }
                }
            });
            Raw { link: xfer::Link::new(tx, acks), inbox }
        }

        /// Receives transfers until `want` matches one of the events.
        fn receive_until(&self, dirs: &xfer::Dirs, want: impl Fn(&xfer::Event) -> bool) -> Vec<xfer::Event> {
            let tx = self.link.tx.clone();
            let mut rx = xfer::Receiver::new(dirs.clone(), move |m| {
                let _ = tx.send(m);
            });
            let mut seen = Vec::new();
            loop {
                let m = self.inbox.recv_timeout(Duration::from_secs(5)).expect("a transfer should arrive");
                for ev in rx.handle(m) {
                    let hit = want(&ev);
                    seen.push(ev);
                    if hit {
                        return seen;
                    }
                }
            }
        }

        /// True if no transfer message is waiting.
        fn no_transfer_waiting(&self) -> bool {
            self.inbox.try_iter().all(|m| !matches!(m, Msg::XferBegin { .. }))
        }
    }

    struct World {
        core: Arc<core::Core>,
        addr: std::net::SocketAddr,
        server_dirs: xfer::Dirs,
        root: std::path::PathBuf,
    }

    fn world(tag: &str) -> World {
        let root = std::env::temp_dir().join(format!("mkshare-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let server_dirs = xfer::Dirs { inbox: root.join("server-inbox"), downloads: root.join("server-downloads") };
        let core = Arc::new(core::Core::with_hub(Hotkey::DEFAULT, None, Hub::new(None)));
        core.set_dirs(server_dirs.clone());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        {
            let core = core.clone();
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let core = core.clone();
                    thread::spawn(move || {
                        let _ = serve(core, stream, "pw", "test", None, clipboard::Options { images: true, files: true });
                    });
                }
            });
        }
        World { core, addr, server_dirs, root }
    }

    impl World {
        fn client(&self, name: &str) -> (Raw, u64) {
            let raw = Raw::start(connect(self.addr, name));
            for _ in 0..100 {
                if let Some((id, _)) = self.core.client_ids().into_iter().find(|(_, n)| n == name) {
                    return (raw, id);
                }
                thread::sleep(Duration::from_millis(20));
            }
            panic!("{name} did not connect");
        }

        fn dirs(&self, who: &str) -> xfer::Dirs {
            xfer::Dirs { inbox: self.root.join(format!("{who}-inbox")), downloads: self.root.join(format!("{who}-downloads")) }
        }

        fn file(&self, name: &str, len: usize) -> (std::path::PathBuf, Vec<u8>) {
            let bytes: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(13) ^ (i >> 5)) as u8).collect();
            let p = self.root.join(name);
            std::fs::write(&p, &bytes).unwrap();
            (p, bytes)
        }
    }

    #[test]
    fn a_file_dropped_on_a_client_reaches_the_server() {
        let w = world("drop-up");
        let (a, _) = w.client("Laptop");
        let (path, bytes) = w.file("holiday.jpg", 1_500_000);
        let sent = xfer::send_paths(&a.link, &[path], proto::XferKind::Drop, &mut |_, _, _| {}).expect("sent and acknowledged");
        assert_eq!(sent, 1);
        let saved = w.server_dirs.downloads.join("holiday.jpg");
        assert_eq!(std::fs::read(&saved).expect("saved on the server"), bytes);
        let _ = std::fs::remove_dir_all(&w.root);
    }

    #[test]
    fn the_server_can_drop_a_file_on_one_chosen_computer() {
        let w = world("drop-down");
        let (a, id_a) = w.client("Laptop");
        let (b, _id_b) = w.client("Desktop");
        let (path, bytes) = w.file("notes.txt", 200_000);
        w.core.send_files(vec![path], proto::XferKind::Drop, Some(id_a), None);
        let events = a.receive_until(&w.dirs("a"), |e| matches!(e, xfer::Event::Dropped(_)));
        let saved = events.iter().find_map(|e| if let xfer::Event::Dropped(p) = e { Some(p.clone()) } else { None }).unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), bytes);
        thread::sleep(Duration::from_millis(200));
        assert!(b.no_transfer_waiting(), "the other computer must not receive it");
        let _ = std::fs::remove_dir_all(&w.root);
    }

    #[test]
    fn a_copied_image_is_passed_on_to_the_other_computer_but_not_back() {
        let w = world("image");
        let (a, _) = w.client("Laptop");
        let (b, _) = w.client("Desktop");
        let png: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        xfer::send_image(&a.link, &png).expect("sent");
        let events = b.receive_until(&w.dirs("b"), |e| matches!(e, xfer::Event::Image(_)));
        assert!(events.iter().any(|e| matches!(e, xfer::Event::Image(got) if *got == png)));
        thread::sleep(Duration::from_millis(200));
        assert!(a.no_transfer_waiting(), "the image must not bounce back to where it came from");
        let _ = std::fs::remove_dir_all(&w.root);
    }

    #[test]
    fn copied_files_are_passed_on_to_the_other_computer() {
        let w = world("files");
        let (a, _) = w.client("Laptop");
        let (b, _) = w.client("Desktop");
        let (p1, b1) = w.file("one.txt", 1000);
        let (p2, b2) = w.file("two.bin", 400_000);
        xfer::send_paths(&a.link, &[p1, p2], proto::XferKind::File, &mut |_, _, _| {}).expect("sent");
        let events = b.receive_until(&w.dirs("b"), |e| matches!(e, xfer::Event::Files(_)));
        let files = events.iter().find_map(|e| if let xfer::Event::Files(f) = e { Some(f.clone()) } else { None }).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(std::fs::read(&files[0]).unwrap(), b1);
        assert_eq!(std::fs::read(&files[1]).unwrap(), b2);
        assert!(files[0].starts_with(w.dirs("b").inbox), "pasted from the inbox folder");
        let _ = std::fs::remove_dir_all(&w.root);
    }

    #[test]
    fn something_too_large_is_refused_right_away() {
        let w = world("refuse");
        let (a, _) = w.client("Laptop");
        let size = xfer::MAX_IMAGE + 1;
        let meta = xfer::Meta { kind: proto::XferKind::Image, name: "huge.png", size, batch: 1, index: 0, count: 1 };
        let started = std::time::Instant::now();
        let r = xfer::send_stream(&a.link, &meta, std::io::Read::take(std::io::repeat(7), size), &mut |_| {});
        assert!(matches!(r, Err(xfer::SendError::Cancelled(ref why)) if why.contains("too large")), "got {r:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "the sender must stop early, not send 48 MB first");
        let _ = std::fs::remove_dir_all(&w.root);
    }

    #[test]
    fn a_connection_that_drops_mid_file_leaves_no_partial_file() {
        let w = world("drop-mid");
        let mut ch = connect(w.addr, "Laptop");
        ch.send(&Msg::XferBegin { id: 9, kind: proto::XferKind::Drop, name: "half.bin".into(), size: 1_000_000, batch: 9, index: 0, count: 1 }).unwrap();
        ch.send(&Msg::XferData { id: 9, bytes: vec![1; 30_000] }).unwrap();
        let part = w.server_dirs.downloads.join("half.bin.part");
        for _ in 0..100 {
            if part.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(part.exists(), "the partial file exists while the transfer runs");
        ch.shutdown(); // the connection ends in the middle of the file
        for _ in 0..100 {
            if !part.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!part.exists(), "the partial file must be removed");
        assert!(!w.server_dirs.downloads.join("half.bin").exists());
        let _ = std::fs::remove_dir_all(&w.root);
    }
}
