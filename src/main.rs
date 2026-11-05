#![cfg_attr(windows, windows_subsystem = "windows")]

mod core;
mod gui;
mod net;
mod platform;
mod proto;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use proto::Msg;
use std::{
    io::BufRead,
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(name = "mkshare", version, about = "Share one keyboard and mouse between machines (Windows/Linux). Run without arguments to open the app.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run on the machine that has the physical keyboard and mouse.
    /// Press Ctrl+Alt+K to switch input between this machine and the client.
    Server {
        #[arg(long, default_value_t = 24800)]
        port: u16,
        /// Shared passphrase (or set MKSHARE_PASS)
        #[arg(long, env = "MKSHARE_PASS")]
        pass: String,
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
        /// Used by the app: exit when stdin closes
        #[arg(long, hide = true)]
        stdin_control: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(std::env::args_os());
    match cli.cmd {
        None => gui::run(),
        Some(cmd) => {
            #[cfg(windows)]
            attach_console();
            match cmd {
                Cmd::Server { port, pass, stdin_control } => run_server(port, pass, stdin_control),
                Cmd::Client { host, port, pass, stdin_control } => {
                    if stdin_control {
                        watch_stdin(None);
                    }
                    run_client(&host, port, &pass)
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

fn watch_stdin(core: Option<Arc<core::Core>>) {
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if line.trim() == "toggle" {
                if let Some(c) = &core {
                    c.set_remote(!c.is_remote());
                }
            }
        }
        std::process::exit(0); // the app that started us is gone
    });
}

// ---------------------------------------------------------------- server

fn run_server(port: u16, pass: String, stdin_control: bool) -> Result<()> {
    let core = Arc::new(core::Core::default());
    if stdin_control {
        watch_stdin(Some(core.clone()));
    }
    let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("cannot bind port {port}"))?;
    println!("Listening on port {port}. Switch input with Ctrl+Alt+K once a client is connected.");
    {
        let core = core.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (core, pass) = (core.clone(), pass.clone());
                thread::spawn(move || {
                    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    if let Err(e) = serve(&core, stream, &pass, &peer) {
                        eprintln!("Connection from {peer} ended: {e:#}");
                    }
                });
            }
        });
    }
    platform::capture(core) // blocks forever
}

fn serve(core: &core::Core, stream: TcpStream, pass: &str, peer: &str) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut ch = net::Secure::accept(stream, pass)?;
    ch.set_read_timeout(None)?;

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
    println!("Client disconnected: {peer}");
    result
}

// ---------------------------------------------------------------- client

fn run_client(host: &str, port: u16, pass: &str) -> Result<()> {
    let mut injector = platform::Injector::new()?;
    println!("Connecting to {host}:{port} ...");
    loop {
        match client_session(host, port, pass, &mut injector) {
            Ok(()) => {}
            Err(e) => eprintln!("Disconnected: {e:#}"),
        }
        injector.release_all();
        thread::sleep(Duration::from_secs(3));
    }
}

fn client_session(host: &str, port: u16, pass: &str, inj: &mut platform::Injector) -> Result<()> {
    let stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?; // handshake
    let mut ch = net::Secure::connect(stream, pass)?;
    ch.set_read_timeout(Some(Duration::from_secs(10)))?; // server pings every 2s
    println!("Connected to {host}:{port}");
    loop {
        match ch.recv()? {
            Msg::Ping | Msg::Hello => {}
            m => inj.apply(&m),
        }
    }
}
