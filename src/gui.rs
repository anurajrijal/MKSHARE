//! Desktop window for mkshare. The actual work is done by a child process
//! (this same executable started with `server` / `client`), so "Stop" simply
//! ends that process and the OS releases all hooks / device grabs.

use eframe::egui::{self, Align, Color32, Layout, RichText};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::UdpSocket,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
enum Mode {
    Server,
    Client,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    mode: Mode,
    host: String,
    port: String,
    pass: String,
    /// GitHub "owner/name" that publishes MKShare releases
    update_repo: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::Server,
            host: String::new(),
            port: "24800".into(),
            pass: String::new(),
            update_repo: String::new(),
        }
    }
}

#[derive(Clone)]
enum Up {
    Idle,
    Checking,
    UpToDate,
    Available(crate::update::Release),
    Installing,
    Done(String),
    Error(String),
}

fn config_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("mkshare").join("config.json"))
}

fn load_config() -> Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    if let Some(p) = config_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(cfg) {
            let _ = std::fs::write(p, s);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Stopped,
    Starting,
    Waiting,
    Connected,
    Remote,
}

impl Status {
    fn label(self, mode: Mode) -> (&'static str, Color32) {
        match (self, mode) {
            (Status::Stopped, _) => ("Stopped", Color32::GRAY),
            (Status::Starting, Mode::Client) => ("Connecting...", Color32::from_rgb(217, 119, 6)),
            (Status::Starting, _) => ("Starting...", Color32::from_rgb(217, 119, 6)),
            (Status::Waiting, _) => ("Waiting for the other computer", Color32::from_rgb(217, 119, 6)),
            (Status::Connected, Mode::Client) => ("Connected", Color32::from_rgb(22, 163, 74)),
            (Status::Connected, Mode::Server) => ("Other computer connected", Color32::from_rgb(22, 163, 74)),
            (Status::Remote, Mode::Server) => ("Keyboard & mouse are on the other computer", Color32::from_rgb(37, 99, 235)),
            (Status::Remote, Mode::Client) => ("Using the other computer's keyboard & mouse", Color32::from_rgb(37, 99, 235)),
        }
    }
}

struct Shared {
    log: Vec<String>,
    status: Status,
}

fn parse_status(line: &str) -> Option<Status> {
    if line.starts_with("Listening on") {
        Some(Status::Waiting)
    } else if line.starts_with("Client connected") || line.starts_with("Input -> LOCAL") {
        Some(Status::Connected)
    } else if line.starts_with("Client disconnected") {
        Some(Status::Waiting)
    } else if line.starts_with("Input -> REMOTE") || line.starts_with("Active: on") {
        Some(Status::Remote)
    } else if line.starts_with("Active: off") {
        Some(Status::Connected)
    } else if line.starts_with("Connected to") {
        Some(Status::Connected)
    } else if line.starts_with("Connecting to") || line.starts_with("Disconnected") {
        Some(Status::Starting)
    } else {
        None
    }
}

fn spawn_reader<R: Read + Send + 'static>(r: R, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    thread::spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            let mut s = shared.lock().unwrap();
            if let Some(st) = parse_status(&line) {
                s.status = st;
            }
            s.log.push(line);
            if s.log.len() > 300 {
                s.log.remove(0);
            }
            drop(s);
            ctx.request_repaint();
        }
    });
}

fn local_ip() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?; // no packet is sent
    Some(s.local_addr().ok()?.ip().to_string())
}

struct App {
    cfg: Config,
    show_pass: bool,
    error: Option<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    shared: Arc<Mutex<Shared>>,
    my_ip: Option<String>,
    running_mode: Mode,
    update: Arc<Mutex<Up>>,
}

impl App {
    fn new() -> Self {
        let cfg = load_config();
        let running_mode = cfg.mode;
        crate::update::cleanup_old();
        App {
            update: Arc::new(Mutex::new(Up::Idle)),
            cfg,
            show_pass: false,
            error: None,
            child: None,
            stdin: None,
            shared: Arc::new(Mutex::new(Shared { log: vec![], status: Status::Stopped })),
            my_ip: local_ip(),
            running_mode,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.error = None;
        let port: u16 = match self.cfg.port.trim().parse() {
            Ok(p) if p > 0 => p,
            _ => {
                self.error = Some("Port must be a number between 1 and 65535.".into());
                return;
            }
        };
        if self.cfg.pass.is_empty() {
            self.error = Some("Enter a passphrase (use the same one on both computers).".into());
            return;
        }
        if self.cfg.mode == Mode::Client && self.cfg.host.trim().is_empty() {
            self.error = Some("Enter the address of the server computer.".into());
            return;
        }
        save_config(&self.cfg);

        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.error = Some(format!("Cannot locate the program: {e}"));
                return;
            }
        };
        let mut cmd = Command::new(exe);
        match self.cfg.mode {
            Mode::Server => cmd.args(["server", "--port", &port.to_string(), "--stdin-control"]),
            Mode::Client => cmd.args(["client", "--host", self.cfg.host.trim(), "--port", &port.to_string(), "--stdin-control"]),
        };
        cmd.env("MKSHARE_PASS", &self.cfg.pass)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        match cmd.spawn() {
            Ok(mut child) => {
                {
                    let mut s = self.shared.lock().unwrap();
                    s.log.clear();
                    s.status = Status::Starting;
                }
                if let Some(o) = child.stdout.take() {
                    spawn_reader(o, self.shared.clone(), ctx.clone());
                }
                if let Some(e) = child.stderr.take() {
                    spawn_reader(e, self.shared.clone(), ctx.clone());
                }
                self.stdin = child.stdin.take();
                self.child = Some(child);
                self.running_mode = self.cfg.mode;
            }
            Err(e) => self.error = Some(format!("Could not start: {e}")),
        }
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.stdin = None;
        self.shared.lock().unwrap().status = Status::Stopped;
    }

    fn poll_child(&mut self) {
        let Some(c) = self.child.as_mut() else { return };
        if let Ok(Some(code)) = c.try_wait() {
            self.child = None;
            self.stdin = None;
            let mut s = self.shared.lock().unwrap();
            s.log.push(format!("Stopped ({code})"));
            s.status = Status::Stopped;
            if !code.success() {
                self.error = Some(
                    s.log
                        .iter()
                        .rev()
                        .find(|l| l.starts_with("Error"))
                        .cloned()
                        .unwrap_or_else(|| "The program stopped unexpectedly - see the log below.".into()),
                );
            }
        }
    }

    fn check_update(&mut self, ctx: &egui::Context) {
        save_config(&self.cfg);
        *self.update.lock().unwrap() = Up::Checking;
        let (state, repo, ctx) = (self.update.clone(), self.cfg.update_repo.clone(), ctx.clone());
        thread::spawn(move || {
            let next = match crate::update::check(&repo) {
                Ok(None) => Up::UpToDate,
                Ok(Some(rel)) => Up::Available(rel),
                Err(e) => Up::Error(format!("{e:#}")),
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    fn install_update(&mut self, rel: crate::update::Release, ctx: &egui::Context) {
        self.stop(); // the running program file is about to be replaced
        save_config(&self.cfg);
        *self.update.lock().unwrap() = Up::Installing;
        let (state, ctx) = (self.update.clone(), ctx.clone());
        thread::spawn(move || {
            match crate::update::install(&rel) {
                Ok(crate::update::Outcome::Restarted) => std::process::exit(0),
                Ok(crate::update::Outcome::Installed(msg)) => *state.lock().unwrap() = Up::Done(msg),
                Err(e) => *state.lock().unwrap() = Up::Error(format!("{e:#}")),
            }
            ctx.request_repaint();
        });
    }

    fn send_toggle(&mut self) {
        if let Some(i) = self.stdin.as_mut() {
            let _ = i.write_all(b"toggle\n");
            let _ = i.flush();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        save_config(&self.cfg);
        self.stop();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_child();
        ctx.request_repaint_after(Duration::from_millis(500));
        let (status, log) = {
            let s = self.shared.lock().unwrap();
            (s.status, s.log.clone())
        };
        let running = self.child.is_some();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("MKShare");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (text, color) = status.label(self.running_mode);
                    ui.label(RichText::new(text).color(color).strong());
                    ui.label(RichText::new("●").color(color));
                });
            });
            ui.label("Use one keyboard and mouse on two computers.");
            ui.separator();

            ui.add_enabled_ui(!running, |ui| {
                ui.label(RichText::new("What should this computer do?").strong());
                ui.radio_value(&mut self.cfg.mode, Mode::Server, "Share this computer's keyboard and mouse  (Server)");
                ui.radio_value(&mut self.cfg.mode, Mode::Client, "Use the other computer's keyboard and mouse  (Client)");
                ui.add_space(8.0);
                egui::Grid::new("fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    if self.cfg.mode == Mode::Client {
                        ui.label("Server address");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.host)
                                .hint_text("e.g. 192.168.1.79")
                                .desired_width(220.0),
                        );
                        ui.end_row();
                    }
                    ui.label("Port");
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.port).desired_width(80.0));
                    ui.end_row();
                    ui.label("Passphrase");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.pass)
                                .password(!self.show_pass)
                                .desired_width(160.0),
                        );
                        ui.checkbox(&mut self.show_pass, "Show");
                    });
                    ui.end_row();
                });
            });

            if self.cfg.mode == Mode::Server {
                ui.add_space(6.0);
                let ip = self.my_ip.clone().unwrap_or_else(|| "unknown (check your network)".into());
                ui.label(format!("This computer's address: {ip}"));
                ui.label(RichText::new("Enter it in the app on the other computer.").weak());
            } else {
                ui.add_space(6.0);
                ui.label(RichText::new("Use the same passphrase as on the server.").weak());
            }

            if let Some(err) = &self.error {
                ui.add_space(6.0);
                ui.colored_label(Color32::from_rgb(220, 38, 38), err);
            }
            let log_text = log.join("\n");
            if cfg!(target_os = "linux") && (log_text.contains("input' group") || log_text.contains("uinput")) {
                ui.add_space(4.0);
                ui.colored_label(
                    Color32::from_rgb(217, 119, 6),
                    "Permission needed: run  sudo usermod -aG input $USER  then log out and in, or start MKShare with sudo.",
                );
            }

            ui.add_space(10.0);
            let (label, color) = if running {
                ("Stop", Color32::from_rgb(185, 28, 28))
            } else {
                ("Start", Color32::from_rgb(37, 99, 235))
            };
            let btn = egui::Button::new(RichText::new(label).size(17.0).color(Color32::WHITE).strong()).fill(color);
            if ui.add_sized([ui.available_width(), 38.0], btn).clicked() {
                if running {
                    self.stop();
                } else {
                    self.start(ctx);
                }
            }

            if running && self.running_mode == Mode::Client {
                ui.add_space(8.0);
                match status {
                    Status::Connected | Status::Remote => {
                        let text = if status == Status::Remote {
                            "Send keyboard and mouse back to the other computer"
                        } else {
                            "Use the other computer's keyboard and mouse"
                        };
                        if ui.add_sized([ui.available_width(), 32.0], egui::Button::new(text)).clicked() {
                            self.send_toggle();
                        }
                        ui.label(RichText::new("You can also press Ctrl+Alt+K on the server's keyboard.").weak());
                    }
                    _ => {
                        ui.label(RichText::new("Waiting to connect to the server...").weak());
                    }
                }
            }

            if running && self.running_mode == Mode::Server {
                ui.add_space(8.0);
                match status {
                    Status::Connected => {
                        if ui
                            .add_sized([ui.available_width(), 32.0], egui::Button::new("Send keyboard and mouse to the other computer"))
                            .clicked()
                        {
                            self.send_toggle();
                        }
                    }
                    Status::Remote => {
                        ui.label(RichText::new("Press Ctrl+Alt+K to bring them back here.").strong());
                    }
                    _ => {
                        ui.label(RichText::new("Start the app on the other computer (Client) to connect.").weak());
                    }
                }
                ui.label(RichText::new("Shortcut: Ctrl+Alt+K switches back and forth.").weak());
            }

            ui.add_space(10.0);
            ui.separator();
            let up = self.update.lock().unwrap().clone();
            let busy = matches!(up, Up::Checking | Up::Installing);
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Update application")).clicked() {
                    self.check_update(ctx);
                }
                match &up {
                    Up::Idle => {
                        ui.label(RichText::new(format!("Version {}", crate::update::CURRENT)).weak());
                    }
                    Up::Checking => {
                        ui.spinner();
                        ui.label("Checking for updates...");
                    }
                    Up::UpToDate => {
                        ui.label(RichText::new(format!("Up to date (version {})", crate::update::CURRENT)).color(Color32::from_rgb(22, 163, 74)));
                    }
                    Up::Available(rel) => {
                        if ui.button(format!("Install version {}", rel.version)).clicked() {
                            self.install_update(rel.clone(), ctx);
                        }
                    }
                    Up::Installing => {
                        ui.spinner();
                        ui.label("Downloading and installing...");
                    }
                    Up::Done(msg) => {
                        ui.label(RichText::new(msg).color(Color32::from_rgb(22, 163, 74)));
                    }
                    Up::Error(_) => {}
                }
            });
            if let Up::Available(rel) = &up {
                ui.label(RichText::new(format!("A newer version ({}) is available. Installing stops sharing and restarts the app.", rel.version)).weak());
            }
            if let Up::Error(e) = &up {
                ui.colored_label(Color32::from_rgb(220, 38, 38), e);
            }
            egui::CollapsingHeader::new("Update settings").default_open(false).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("GitHub repository");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.update_repo)
                            .hint_text("owner/name, e.g. yourname/mkshare")
                            .desired_width(240.0),
                    );
                });
                ui.label(RichText::new("Updates are downloaded from the latest release of this repository.").weak());
            });

            egui::CollapsingHeader::new("Log").default_open(false).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(150.0).stick_to_bottom(true).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for l in &log {
                        ui.label(RichText::new(l).monospace().size(12.0));
                    }
                });
            });
        });
    }
}

pub fn run() -> anyhow::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("MKShare")
        .with_app_id("mkshare")
        .with_inner_size([470.0, 520.0])
        .with_min_inner_size([420.0, 440.0]);
    if let Some(i) = icon {
        viewport = viewport.with_icon(Arc::new(i));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("MKShare", options, Box::new(|_cc| Ok(Box::new(App::new()))))
        .map_err(|e| anyhow::anyhow!("cannot open window: {e}"))
}
