#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod clipboard;
mod core;
mod discovery;
mod edge;
mod gui;
mod net;
mod platform;
mod proto;
mod secret;
#[cfg(target_os = "linux")]
mod setup;
mod tray;
mod update;

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
        /// Do not share the clipboard (copied text) with the client
        #[arg(long)]
        no_clipboard: bool,
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
        /// Do not share the clipboard (copied text) with the server
        #[arg(long)]
        no_clipboard: bool,
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
                Cmd::Server { port, pass, hotkey, edge, no_clipboard, stdin_control } => {
                    let hotkey = Hotkey::parse(&hotkey).map_err(|e| anyhow!("Error: invalid --hotkey: {e}"))?;
                    let edge = match edge.as_deref() {
                        None | Some("off") | Some("") => None,
                        Some(s) => Some(Side::parse(s).ok_or_else(|| anyhow!("Error: --edge must be left, right, top, bottom or off"))?),
                    };
                    run_server(port, pass, stdin_control, hotkey, edge, !no_clipboard)
                }
                Cmd::Client { host, port, pass, name, no_clipboard, stdin_control } => {
                    if stdin_control {
                        watch_stdin(None);
                    }
                    run_client(&host, port, &pass, name, !no_clipboard)
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

fn watch_stdin(core: Option<Arc<core::Core>>) {
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim() == "toggle" {
                match &core {
                    // server: switch directly
                    Some(c) => c.set_remote(!c.is_remote()),
                    // client: ask the server to switch
                    None => {
                        if let Some(tx) = CLIENT_TX.lock().unwrap().as_ref() {
                            let _ = tx.send(Msg::Toggle);
                        }
                    }
                }
            }
        }
        std::process::exit(0); // the app that started us is gone
    });
}

// ---------------------------------------------------------------- server

fn run_server(port: u16, pass: String, stdin_control: bool, hotkey: Hotkey, edge: Option<Side>, share_clipboard: bool) -> Result<()> {
    let core = Arc::new(core::Core::new(hotkey, edge));
    let clip = if share_clipboard {
        let c = core.clone();
        let cb = clipboard::start(move |text| c.send_msg(Msg::Clipboard(text)));
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
                    if let Err(e) = serve(core, stream, &pass, &peer, clip) {
                        eprintln!("Connection from {peer} ended: {e:#}");
                    }
                });
            }
        });
    }
    platform::capture(core) // blocks forever
}

fn serve(core: Arc<core::Core>, stream: TcpStream, pass: &str, peer: &str, clip: Option<clipboard::Clipboard>) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut ch = net::Secure::accept(stream, pass)?;
    ch.set_read_timeout(None)?;

    // Requests coming from the client (e.g. "switch input") are handled on their own thread.
    let mut reader = ch.try_clone()?;
    let reader_core = core.clone();
    thread::spawn(move || {
        while let Ok(msg) = reader.recv() {
            match msg {
                Msg::Toggle => reader_core.set_remote(!reader_core.is_remote()),
                Msg::Leave { x, y } => reader_core.leave_remote(x, y),
                Msg::Clipboard(text) => {
                    if let Some(c) = &clip {
                        c.set(text);
                    }
                }
                _ => {}
            }
        }
    });

    let (tx, rx) = mpsc::channel::<Msg>();
    let id = core.attach(tx);
    println!("Client connected: {peer}");
    let result = loop {
        let msg = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => Msg::Ping, // keepalive
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()), // replaced by a newer client
        };
        if let Err(e) = ch.send(&msg) {
            break Err(e);
        }
    };
    core.detach(id);
    ch.shutdown(); // also ends the reader thread
    println!("Client disconnected: {peer}");
    result
}

// ---------------------------------------------------------------- client

fn run_client(host: &str, port: u16, pass: &str, name: Option<String>, share_clipboard: bool) -> Result<()> {
    let mut injector = platform::Injector::new()?;
    let clip = if share_clipboard {
        clipboard::start(|text| {
            if let Some(tx) = CLIENT_TX.lock().unwrap().as_ref() {
                let _ = tx.send(Msg::Clipboard(text));
            }
        })
    } else {
        None
    };
    let (mut host, mut port) = (host.to_string(), port);
    println!("Connecting to {host}:{port} ...");
    let mut failures = 0;
    loop {
        match client_session(&host, port, pass, &mut injector, clip.as_ref()) {
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

fn client_session(host: &str, port: u16, pass: &str, inj: &mut platform::Injector, clip: Option<&clipboard::Clipboard>) -> Result<()> {
    let stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?; // handshake
    let mut ch = net::Secure::connect(stream, pass)?;
    ch.set_read_timeout(Some(Duration::from_secs(10)))?; // server pings every 2s
    println!("Connected to {host}:{port}");

    // Requests to the server (e.g. "switch input") are sent from their own thread.
    let mut writer = ch.try_clone()?;
    let (tx, rx) = mpsc::channel::<Msg>();
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
            Msg::Ping | Msg::Hello | Msg::Toggle | Msg::Leave { .. } => {}
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
    ch.shutdown();
    result
}
