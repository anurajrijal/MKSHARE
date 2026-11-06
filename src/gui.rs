//! Desktop window for mkshare. The actual work is done by a child process
//! (this same executable started with `server` / `client`), so "Stop" simply
//! ends that process and the OS releases all hooks / device grabs.

use crate::proto::{Hotkey, Side, KEYS};
use eframe::egui::{self, Align, Color32, Layout, RichText};

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Log,
    Settings,
}
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
    /// Never written to config.json (see secret.rs); only read to migrate old files.
    #[serde(skip_serializing)]
    pass: String,
    /// Keep the passphrase in protected storage between runs
    remember_pass: bool,
    /// Switch shortcut in text form, e.g. "ctrl+alt+k"
    hotkey: String,
    /// Switch by moving the mouse to the screen edge
    edge_enabled: bool,
    /// Side of this screen where the other computer sits: left, right, top or bottom
    edge_side: String,
    /// Share copied text between the two computers
    share_clipboard: bool,
    /// Windows: closing the window keeps MKShare running in the tray
    tray_close: bool,
    /// Start sharing by itself when MKShare opens (uses the saved settings)
    auto_connect: bool,
    /// Client: name of the server picked from the list (lets the client find it again if its IP changes)
    server_name: String,
    /// The address `server_name` belongs to
    named_host: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::Server,
            host: String::new(),
            port: "24800".into(),
            pass: String::new(),
            remember_pass: true,
            hotkey: Hotkey::DEFAULT.to_string(),
            edge_enabled: false,
            edge_side: "right".into(),
            share_clipboard: true,
            tray_close: true,
            auto_connect: false,
            server_name: String::new(),
            named_host: String::new(),
        }
    }
}

#[derive(Clone, Default)]
struct ScanState {
    running: bool,
    done: bool,
    results: Vec<crate::discovery::Found>,
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

/// State of the Linux "Fix permissions" button.
#[derive(Clone)]
enum PermFix {
    Idle,
    Running,
    Done,
    Failed(String),
}

fn config_path() -> Option<PathBuf> {
    Some(crate::secret::config_dir()?.join("config.json"))
}

/// Where "Update application" looks for new versions (the project's GitHub Releases).
const UPDATE_REPO: &str = "anurajrijal/MKSHARE";

fn load_config() -> Config {
    let mut cfg: Config = config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !cfg.pass.is_empty() {
        // Old versions kept the passphrase in plain text inside config.json:
        // move it to protected storage and rewrite the file without it.
        if !cfg.remember_pass || crate::secret::store(&cfg.pass).is_ok() {
            save_config(&cfg);
        }
    } else if cfg.remember_pass {
        cfg.pass = crate::secret::load().unwrap_or_default();
    }
    cfg
}

/// Saves the settings (without the passphrase) readable by the current user only.
fn save_config(cfg: &Config) {
    if crate::secret::ensure_private_dir().is_some() {
        if let (Some(p), Ok(s)) = (config_path(), serde_json::to_string_pretty(cfg)) {
            if std::fs::write(&p, s).is_ok() {
                crate::secret::restrict(&p, 0o600);
            }
        }
    }
}

/// Keeps the passphrase in protected storage, or removes it if "remember" is off.
fn persist_secret(cfg: &Config) {
    if cfg.remember_pass && !cfg.pass.is_empty() {
        if let Err(e) = crate::secret::store(&cfg.pass) {
            eprintln!("could not store the passphrase: {e}");
        }
    } else if !cfg.remember_pass {
        crate::secret::clear();
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
                crate::tray::set_tooltip(&format!("MKShare - {}", short_status(st).0));
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
    secret_backend: &'static str,
    hk: Hotkey,
    scan: Arc<Mutex<ScanState>>,
    scanned_once: bool,
    page: Page,
    logo: egui::TextureHandle,
    /// The tray icon is on screen (Windows)
    tray_ok: bool,
    autostart_on: bool,
    startup_error: Option<String>,
    perm_fix: Arc<Mutex<PermFix>>,
    /// started by "start on login": hide the window once its first frame exists (Windows)
    hide_first_frame: bool,
}

impl App {
    fn start_scan(&mut self, ctx: &egui::Context) {
        {
            let mut s = self.scan.lock().unwrap();
            if s.running {
                return;
            }
            s.running = true;
            s.done = false;
        }
        let (state, ctx) = (self.scan.clone(), ctx.clone());
        thread::spawn(move || {
            let results = crate::discovery::scan(Duration::from_millis(1500));
            *state.lock().unwrap() = ScanState { running: false, done: true, results };
            ctx.request_repaint();
        });
    }

    fn new(ctx: &egui::Context, tray_ok: bool, background: bool) -> Self {
        let cfg = load_config();
        if background && cfg!(windows) && !tray_ok {
            // started hidden, but there is no tray icon to open it from: show the window
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        } else if background && !cfg!(windows) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        let running_mode = cfg.mode;
        crate::update::cleanup_old();
        let logo = match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
            Ok(i) => egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba),
            Err(_) => egui::ColorImage::example(),
        };
        let mut app = App {
            tray_ok,
            autostart_on: crate::autostart::is_enabled(),
            startup_error: None,
            perm_fix: Arc::new(Mutex::new(PermFix::Idle)),
            hide_first_frame: background && tray_ok && cfg!(windows),
            page: Page::Home,
            logo: ctx.load_texture("logo", logo, egui::TextureOptions::LINEAR),
            hk: Hotkey::parse(&cfg.hotkey).unwrap_or(Hotkey::DEFAULT),
            scan: Arc::new(Mutex::new(ScanState::default())),
            scanned_once: false,
            secret_backend: crate::secret::backend(),
            update: Arc::new(Mutex::new(Up::Idle)),
            cfg,
            show_pass: false,
            error: None,
            child: None,
            stdin: None,
            shared: Arc::new(Mutex::new(Shared { log: vec![], status: Status::Stopped })),
            my_ip: local_ip(),
            running_mode,
        };
        if app.cfg.auto_connect {
            app.start(ctx);
        }
        app
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
            self.error = Some("Enter the address of the server computer, or use Find servers.".into());
            return;
        }
        if self.cfg.mode == Mode::Server {
            if let Err(e) = self.hk.validate() {
                self.error = Some(format!("Switch shortcut: {e}"));
                return;
            }
            self.cfg.hotkey = self.hk.to_string();
        }
        save_config(&self.cfg);
        persist_secret(&self.cfg);

        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.error = Some(format!("Cannot locate the program: {e}"));
                return;
            }
        };
        let mut cmd = Command::new(exe);
        match self.cfg.mode {
            Mode::Server => {
                cmd.args(["server", "--port", &port.to_string(), "--stdin-control", "--hotkey", &self.hk.to_string()]);
                if self.cfg.edge_enabled {
                    cmd.args(["--edge", &self.cfg.edge_side]);
                }
            }
            Mode::Client => {
                cmd.args(["client", "--host", self.cfg.host.trim(), "--port", &port.to_string(), "--stdin-control"]);
                // lets the client find the server again if its IP address changes
                if !self.cfg.server_name.is_empty() && self.cfg.named_host == self.cfg.host.trim() {
                    cmd.args(["--name", &self.cfg.server_name]);
                }
            }
        };
        if !self.cfg.share_clipboard {
            cmd.arg("--no-clipboard");
        }
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
        crate::tray::set_tooltip("MKShare - Ready");
    }

    fn poll_child(&mut self) {
        let Some(c) = self.child.as_mut() else { return };
        if let Ok(Some(code)) = c.try_wait() {
            self.child = None;
            self.stdin = None;
            let mut s = self.shared.lock().unwrap();
            s.log.push(format!("Stopped ({code})"));
            s.status = Status::Stopped;
            crate::tray::set_tooltip("MKShare - Ready");
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
        let (state, ctx) = (self.update.clone(), ctx.clone());
        thread::spawn(move || {
            let next = match crate::update::check(UPDATE_REPO) {
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

    /// Linux: installs the udev rule that lets the logged-in user use the input devices.
    /// Needs administrator rights, asked for with the system's graphical password prompt (pkexec).
    fn fix_permissions(&mut self, ctx: &egui::Context) {
        *self.perm_fix.lock().unwrap() = PermFix::Running;
        let (state, ctx) = (self.perm_fix.clone(), ctx.clone());
        thread::spawn(move || {
            let result = std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
                let out = Command::new("pkexec")
                    .arg(exe)
                    .arg("setup-linux")
                    .output()
                    .map_err(|e| format!("could not run pkexec (it needs a graphical password prompt): {e}"))?;
                if out.status.success() {
                    Ok(())
                } else {
                    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    Err(if err.is_empty() { "cancelled, or the password was not accepted".to_string() } else { err })
                }
            });
            *state.lock().unwrap() = match result {
                Ok(()) => PermFix::Done,
                Err(e) => PermFix::Failed(e),
            };
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
        persist_secret(&self.cfg);
        self.stop();
        crate::tray::remove();
    }
}

// ------------------------------------------------------------------ look & feel

const BG: Color32 = Color32::from_rgb(12, 14, 27);
const SIDEBAR: Color32 = Color32::from_rgb(15, 17, 33);
const CARD: Color32 = Color32::from_rgb(22, 25, 45);
const CARD_BORDER: Color32 = Color32::from_rgb(42, 47, 76);
const FIELD: Color32 = Color32::from_rgb(9, 10, 20);
const ACCENT: Color32 = Color32::from_rgb(79, 91, 255);
const ACCENT2: Color32 = Color32::from_rgb(56, 170, 255);
const DIM: Color32 = Color32::from_rgb(160, 168, 205);
const GREEN: Color32 = Color32::from_rgb(34, 197, 94);
const AMBER: Color32 = Color32::from_rgb(245, 158, 11);
const RED: Color32 = Color32::from_rgb(239, 68, 68);

fn theme(ctx: &egui::Context) {
    use egui::{CornerRadius, FontId, Stroke, TextStyle};
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = CARD;
    v.override_text_color = Some(Color32::from_rgb(232, 235, 250));
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    let r = CornerRadius::same(9);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.inactive.bg_fill = Color32::from_rgb(34, 39, 70);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(34, 39, 70);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    v.widgets.hovered.bg_fill = Color32::from_rgb(46, 53, 96);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(46, 53, 96);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 10.0);
        s.spacing.button_padding = egui::vec2(14.0, 8.0);
        s.text_styles.insert(TextStyle::Body, FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(26.0));
    });
}

fn dim_if_disabled(ui: &egui::Ui, c: Color32) -> Color32 {
    if ui.is_enabled() {
        c
    } else {
        c.gamma_multiply(0.45)
    }
}

/// A filled rounded rectangle with a left-to-right colour gradient.
fn gradient_rect(painter: &egui::Painter, rect: egui::Rect, radius: f32, left: Color32, right: Color32) {
    use std::f32::consts::PI;
    let r = radius.min(rect.height() / 2.0).min(rect.width() / 2.0);
    let corners = [
        (egui::pos2(rect.left() + r, rect.top() + r), PI),
        (egui::pos2(rect.right() - r, rect.top() + r), 1.5 * PI),
        (egui::pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (egui::pos2(rect.left() + r, rect.bottom() - r), 0.5 * PI),
    ];
    let mut pts = Vec::new();
    for (c, a0) in corners {
        for i in 0..=8 {
            let a = a0 + i as f32 * (PI / 2.0) / 8.0;
            pts.push(c + egui::vec2(a.cos(), a.sin()) * r);
        }
    }
    let color_at = |x: f32| left.lerp_to_gamma(right, ((x - rect.left()) / rect.width()).clamp(0.0, 1.0));
    let mut mesh = egui::epaint::Mesh::default();
    mesh.colored_vertex(rect.center(), color_at(rect.center().x));
    for p in &pts {
        mesh.colored_vertex(*p, color_at(p.x));
    }
    let n = pts.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn card<R>(ui: &mut egui::Ui, selected: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(CARD)
        .stroke(egui::Stroke::new(if selected { 1.5_f32 } else { 1.0_f32 }, if selected { ACCENT } else { CARD_BORDER }))
        .corner_radius(egui::CornerRadius::same(16))
        .inner_margin(egui::Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
}

/// Large button with a gradient fill. Returns true when clicked.
fn big_button(ui: &mut egui::Ui, icon: &str, label: &str, left: Color32, right: Color32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 54.0), egui::Sense::click());
    let (l, r) = if resp.hovered() {
        (left.lerp_to_gamma(Color32::WHITE, 0.12), right.lerp_to_gamma(Color32::WHITE, 0.12))
    } else {
        (left, right)
    };
    gradient_rect(ui.painter(), rect, 16.0, l, r);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{icon}  {label}"),
        egui::FontId::proportional(21.0),
        Color32::WHITE,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn secondary_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add_sized(
        [ui.available_width(), 42.0],
        egui::Button::new(RichText::new(text).size(16.0)).corner_radius(egui::CornerRadius::same(12)),
    )
    .clicked()
}

/// Checkbox drawn in the app style (blue box with a white tick).
fn check(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let inner = ui.horizontal(|ui| {
        let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
        resp = resp.union(ui.add(egui::Label::new(RichText::new(label)).sense(egui::Sense::click())));
        if resp.clicked() {
            *value = !*value;
        }
        let p = ui.painter();
        let rr = egui::CornerRadius::same(6);
        if *value {
            p.rect_filled(rect, rr, dim_if_disabled(ui, ACCENT));
            let c = rect.center();
            let s = egui::Stroke::new(2.2_f32, dim_if_disabled(ui, Color32::WHITE));
            p.line_segment([c + egui::vec2(-5.5, 0.5), c + egui::vec2(-1.5, 4.5)], s);
            p.line_segment([c + egui::vec2(-1.5, 4.5), c + egui::vec2(6.0, -4.5)], s);
        } else {
            p.rect_filled(rect, rr, FIELD);
            p.rect_stroke(rect, rr, egui::Stroke::new(1.5_f32, dim_if_disabled(ui, Color32::from_rgb(88, 95, 135))), egui::StrokeKind::Inside);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    });
    inner.inner
}

/// A big selectable card with a round radio mark. Returns true when clicked.
fn mode_card(ui: &mut egui::Ui, selected: bool, icon: &str, text: &str) -> bool {
    let r = card(ui, selected, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
            let ring = if selected { ACCENT } else { Color32::from_rgb(92, 98, 138) };
            ui.painter().circle_stroke(rect.center(), 12.0, egui::Stroke::new(2.5_f32, dim_if_disabled(ui, ring)));
            if selected {
                ui.painter().circle_filled(rect.center(), 6.5, dim_if_disabled(ui, Color32::WHITE));
            }
            ui.add_space(10.0);
            ui.label(RichText::new(icon).size(30.0).color(dim_if_disabled(ui, Color32::from_rgb(170, 175, 255))));
            ui.add_space(10.0);
            ui.label(RichText::new(text).size(17.0));
        });
    });
    let resp = ui.interact(r.response.rect, ui.id().with(text), egui::Sense::click());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A small filled circle (the status light).
fn dot(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
}

fn section_title(ui: &mut egui::Ui, icon: &str, text: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(26.0).color(Color32::from_rgb(170, 175, 255)));
        ui.label(RichText::new(text).size(20.0).strong());
    });
}

fn info_box(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::from_rgb(17, 20, 38))
        .stroke(egui::Stroke::new(1.0_f32, CARD_BORDER))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
                ui.painter().circle_stroke(rect.center(), 11.0, egui::Stroke::new(2.0_f32, ACCENT2));
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "i", egui::FontId::proportional(16.0), ACCENT2);
                ui.vertical(|ui| add(ui));
            });
        });
}

fn field_label(ui: &mut egui::Ui, text: &str, width: f32) {
    ui.allocate_ui_with_layout(egui::vec2(width, 38.0), Layout::left_to_right(Align::Center), |ui| {
        ui.label(RichText::new(text).size(16.0));
    });
}

fn text_field(text: &mut String, width: f32) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).margin(egui::Margin::symmetric(12, 10)).desired_width(width)
}

/// The illustration in the top banner: two screens with an arrow between them.
fn paint_hero(ui: &egui::Ui, rect: egui::Rect) {
    let p = ui.painter();
    let screen = |r: egui::Rect| {
        gradient_rect(p, r, 8.0, Color32::from_rgb(26, 34, 86), Color32::from_rgb(54, 72, 160));
        p.rect_stroke(r, egui::CornerRadius::same(8), egui::Stroke::new(2.0_f32, Color32::from_rgb(120, 130, 190)), egui::StrokeKind::Inside);
        let stand = egui::Rect::from_center_size(egui::pos2(r.center().x, r.bottom() + 9.0), egui::vec2(10.0, 18.0));
        p.rect_filled(stand, 2.0, Color32::from_rgb(70, 76, 110));
        let base = egui::Rect::from_center_size(egui::pos2(r.center().x, r.bottom() + 19.0), egui::vec2(r.width() * 0.5, 6.0));
        p.rect_filled(base, 3.0, Color32::from_rgb(70, 76, 110));
    };
    let h = rect.height() - 24.0;
    let w = (h * 1.3).min(rect.width() * 0.38);
    let left = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.top() + 4.0), egui::vec2(w, h));
    let right = egui::Rect::from_min_size(egui::pos2(rect.right() - w, rect.top() + 4.0), egui::vec2(w, h));
    screen(left);
    screen(right);
    // double arrow
    let y = left.center().y;
    let (x0, x1) = (left.right() + 12.0, right.left() - 12.0);
    let s = egui::Stroke::new(3.0_f32, ACCENT2);
    p.line_segment([egui::pos2(x0 + 8.0, y), egui::pos2(x1 - 8.0, y)], s);
    p.add(egui::Shape::convex_polygon(
        vec![egui::pos2(x0, y), egui::pos2(x0 + 11.0, y - 7.0), egui::pos2(x0 + 11.0, y + 7.0)],
        ACCENT2,
        egui::Stroke::NONE,
    ));
    p.add(egui::Shape::convex_polygon(
        vec![egui::pos2(x1, y), egui::pos2(x1 - 11.0, y - 7.0), egui::pos2(x1 - 11.0, y + 7.0)],
        ACCENT2,
        egui::Stroke::NONE,
    ));
    // a mouse on the right screen
    let m = egui::Rect::from_center_size(right.center() + egui::vec2(w * 0.12, h * 0.12), egui::vec2(18.0, 28.0));
    p.rect_filled(m, egui::CornerRadius::same(9), Color32::from_rgb(190, 195, 255));
    p.line_segment([egui::pos2(m.center().x, m.top() + 1.0), egui::pos2(m.center().x, m.top() + 12.0)], egui::Stroke::new(1.5_f32, Color32::from_rgb(70, 76, 140)));
}

fn short_status(status: Status) -> (&'static str, Color32) {
    match status {
        Status::Stopped => ("Ready", GREEN),
        Status::Starting => ("Starting...", AMBER),
        Status::Waiting => ("Waiting", AMBER),
        Status::Connected => ("Connected", GREEN),
        Status::Remote => ("Sharing active", ACCENT2),
    }
}

// ------------------------------------------------------------------ screens

impl App {
    fn sidebar(&mut self, ui: &mut egui::Ui, status: Status) {
        ui.vertical_centered(|ui| {
            ui.add_space(26.0);
            ui.image((self.logo.id(), egui::vec2(72.0, 72.0)));
            ui.add_space(2.0);
            ui.label(RichText::new("MKShare").size(30.0).strong());
            ui.label(RichText::new("Share. Control. Together.").size(13.0));
        });
        ui.add_space(34.0);
        for (page, icon, text) in [(Page::Home, "🏠", "Home"), (Page::Log, "🕓", "Log"), (Page::Settings, "⚙", "Settings")] {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::click());
            let selected = self.page == page;
            if selected {
                gradient_rect(ui.painter(), rect, 12.0, Color32::from_rgb(62, 74, 210), Color32::from_rgb(44, 52, 140));
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, egui::CornerRadius::same(12), Color32::from_rgb(26, 30, 56));
            }
            let col = if selected { Color32::WHITE } else { Color32::from_rgb(200, 205, 235) };
            ui.painter().text(rect.left_center() + egui::vec2(18.0, 0.0), egui::Align2::LEFT_CENTER, icon, egui::FontId::proportional(24.0), col);
            ui.painter().text(rect.left_center() + egui::vec2(58.0, 0.0), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(18.0), col);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.page = page;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(14.0);
            let (text, color) = short_status(status);
            egui::Frame::new()
                .fill(Color32::from_rgb(20, 23, 44))
                .stroke(egui::Stroke::new(1.0_f32, CARD_BORDER))
                .corner_radius(egui::CornerRadius::same(14))
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        dot(ui, color, 14.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(RichText::new(text).size(16.0));
                            ui.label(RichText::new(format!("v{}", crate::update::CURRENT)).size(12.0).color(DIM));
                        });
                    });
                });
        });
    }

    fn home(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, status: Status, log: &[String], running: bool, scan: &ScanState) {
        // ---- banner
        card(ui, false, |ui| {
            let height = 112.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
            let art_w = (rect.width() * 0.36).min(340.0);
            let art = egui::Rect::from_min_size(egui::pos2(rect.right() - art_w, rect.top()), egui::vec2(art_w, height));
            paint_hero(ui, art);
            let text_rect = egui::Rect::from_min_max(rect.min, egui::pos2(art.left() - 12.0, rect.bottom()));
            ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
                ui.add_space(14.0);
                ui.label(RichText::new("What should this computer do?").size(28.0).strong());
                ui.label(RichText::new("Share your keyboard and mouse across computers.").size(17.0).color(DIM));
            });
        });

        // ---- role
        ui.add_enabled_ui(!running, |ui| {
            if mode_card(ui, self.cfg.mode == Mode::Server, "🖥", "Share this computer's keyboard and mouse\n(Server)") {
                self.cfg.mode = Mode::Server;
            }
            if mode_card(ui, self.cfg.mode == Mode::Client, "👤", "Use the other computer's keyboard and mouse\n(Client)") {
                self.cfg.mode = Mode::Client;
            }
        });

        // ---- settings
        let client = self.cfg.mode == Mode::Client;
        card(ui, false, |ui| {
            section_title(ui, if client { "👤" } else { "🖥" }, if client { "Client Settings" } else { "Server Settings" });
            ui.add_enabled_ui(!running, |ui| {
                let total = ui.available_width();
                if client {
                    ui.horizontal(|ui| {
                        field_label(ui, "Server Address", 124.0);
                        let w = (total - 124.0 - 170.0 - 40.0).max(120.0);
                        ui.add(text_field(&mut self.cfg.host, w).hint_text("e.g. 192.168.1.79"));
                        let find = egui::Button::new(RichText::new("🔍  Find Servers").size(16.0)).min_size(egui::vec2(170.0, 40.0));
                        if ui.add_enabled(!scan.running, find).clicked() {
                            self.start_scan(ctx);
                        }
                    });
                }
                ui.horizontal(|ui| {
                    field_label(ui, "Port", if client { 124.0 } else { 70.0 });
                    ui.add(text_field(&mut self.cfg.port, 110.0));
                    ui.add_space(14.0);
                    field_label(ui, "Passphrase", 96.0);
                    let used = (if client { 124.0 } else { 70.0 }) + 110.0 + 14.0 + 96.0 + 100.0 + 90.0;
                    ui.add(text_field(&mut self.cfg.pass, (total - used).max(110.0)).password(!self.show_pass));
                    check(ui, &mut self.show_pass, "Show");
                });
                check(ui, &mut self.cfg.remember_pass, "Remember passphrase");
                if self.cfg.remember_pass {
                    ui.label(RichText::new(format!("Stored: {}", self.secret_backend)).size(12.0).color(DIM));
                }
                check(ui, &mut self.cfg.share_clipboard, "Share the clipboard (copy on one computer, paste on the other)");
                ui.label(RichText::new("Text only. Turn off if you copy passwords or other private text.").size(12.0).color(DIM));
            });

            if client {
                if scan.running {
                    info_box(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Looking for servers on your network...");
                        });
                    });
                } else if scan.done && scan.results.is_empty() {
                    info_box(ui, |ui| {
                        ui.label(RichText::new("No servers found. Start MKShare as Server on the other computer (same network), or type its address.").color(DIM));
                    });
                } else if !scan.results.is_empty() {
                    info_box(ui, |ui| {
                        ui.label(RichText::new("Servers found - click one to use it:").strong());
                        for f in &scan.results {
                            let chosen = self.cfg.host == f.addr && self.cfg.port == f.port.to_string();
                            let button = egui::Button::new(format!("{}   ({})", f.name, f.addr)).selected(chosen);
                            if ui.add(button).clicked() {
                                self.cfg.host = f.addr.clone();
                                self.cfg.port = f.port.to_string();
                                self.cfg.server_name = f.name.clone();
                                self.cfg.named_host = f.addr.clone();
                            }
                        }
                        if let Some(f) = scan.results.iter().find(|f| self.cfg.host == f.addr && self.cfg.port == f.port.to_string()) {
                            let msg = if self.cfg.pass.is_empty() {
                                format!("Using {} ({}). Now enter the passphrase and press Start.", f.name, f.addr)
                            } else {
                                format!("Using {} ({}). Press Start.", f.name, f.addr)
                            };
                            ui.colored_label(GREEN, msg);
                        }
                    });
                } else {
                    info_box(ui, |ui| {
                        ui.label(RichText::new("Use the same passphrase as on the server.").color(DIM));
                    });
                }
            } else {
                let ip = self.my_ip.clone().unwrap_or_else(|| "unknown (check your network)".into());
                info_box(ui, |ui| {
                    ui.label(RichText::new(format!("This computer's address: {ip}")).strong());
                    ui.label(RichText::new("Enter it in the app on the other computer, or let it find this one with Find Servers.").color(DIM));
                });
            }
        });

        // ---- messages
        if let Some(err) = &self.error {
            ui.colored_label(RED, err);
        }
        let log_text = log.join("\n");
        if cfg!(target_os = "linux") && (log_text.contains("input' group") || log_text.contains("uinput")) {
            ui.colored_label(AMBER, "MKShare needs permission to read and create input devices on this computer.");
            let pf = self.perm_fix.lock().unwrap().clone();
            match &pf {
                PermFix::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Waiting for the password prompt...");
                    });
                }
                PermFix::Done => {
                    ui.colored_label(GREEN, "Permissions fixed. Press Start again.");
                }
                PermFix::Failed(e) => {
                    ui.colored_label(RED, e);
                }
                PermFix::Idle => {}
            }
            if !matches!(pf, PermFix::Running) && secondary_button(ui, "Fix permissions (asks for your password once)") {
                self.fix_permissions(ctx);
            }
            ui.label(
                RichText::new("Or in a terminal: sudo modprobe uinput && sudo usermod -aG input $USER, then log out and in.")
                    .size(12.0)
                    .color(DIM),
            );
        }

        // ---- start / stop
        let clicked = if running {
            big_button(ui, "⏹", "Stop", Color32::from_rgb(220, 60, 80), Color32::from_rgb(170, 40, 70))
        } else {
            big_button(ui, "▶", "Start", Color32::from_rgb(88, 70, 245), ACCENT2)
        };
        if clicked {
            if running {
                self.stop();
            } else {
                self.start(ctx);
            }
        }

        if running {
            let (text, color) = status.label(self.running_mode);
            ui.horizontal(|ui| {
                dot(ui, color, 12.0);
                ui.label(RichText::new(text).color(color).strong());
            });
        }
        self.run_controls(ui, status, running);

        // ---- log
        card(ui, false, |ui| {
            egui::CollapsingHeader::new(RichText::new("📄  Application Log").size(17.0)).default_open(false).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(180.0).stick_to_bottom(true).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for l in log {
                        ui.label(RichText::new(l).monospace().size(12.5));
                    }
                });
            });
        });
    }

    /// Buttons that are shown while sharing is running.
    fn run_controls(&mut self, ui: &mut egui::Ui, status: Status, running: bool) {
        let hk_text = self.hk.pretty();
        if running && self.running_mode == Mode::Client {
            match status {
                Status::Connected | Status::Remote => {
                    let text = if status == Status::Remote {
                        "Send keyboard and mouse back to the other computer"
                    } else {
                        "Use the other computer's keyboard and mouse"
                    };
                    if secondary_button(ui, text) {
                        self.send_toggle();
                    }
                    ui.label(RichText::new("You can also use the switch shortcut (or the screen edge, if enabled) on the server.").color(DIM));
                }
                _ => {
                    ui.label(RichText::new("Waiting to connect to the server...").color(DIM));
                }
            }
        }
        if running && self.running_mode == Mode::Server {
            match status {
                Status::Connected => {
                    if secondary_button(ui, "Send keyboard and mouse to the other computer") {
                        self.send_toggle();
                    }
                }
                Status::Remote => {
                    ui.label(RichText::new(format!("Press {hk_text} to bring them back here.")).strong());
                }
                _ => {
                    ui.label(RichText::new("Start the app on the other computer (Client) to connect.").color(DIM));
                }
            }
            ui.label(RichText::new(format!("Shortcut: {hk_text} switches back and forth.")).color(DIM));
            if self.cfg.edge_enabled {
                ui.label(RichText::new(format!("Screen edge: move the mouse off the {} edge.", self.cfg.edge_side)).color(DIM));
            }
        }
    }

    fn log_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, log: &[String]) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").size(26.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Copy log").clicked() {
                    ctx.copy_text(log.join("\n"));
                }
            });
        });
        card(ui, false, |ui| {
            if log.is_empty() {
                ui.label(RichText::new("Nothing yet. Messages appear here once you press Start.").color(DIM));
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                for l in log {
                    ui.label(RichText::new(l).monospace().size(13.0));
                }
            });
        });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, running: bool) {
        ui.label(RichText::new("Settings").size(26.0).strong());

        card(ui, false, |ui| {
            section_title(ui, "🖱", "Switching");
            ui.label(RichText::new("These apply to the computer that shares its keyboard and mouse (the Server).").color(DIM));
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Shortcut:");
                    check(ui, &mut self.hk.ctrl, "Ctrl");
                    check(ui, &mut self.hk.alt, "Alt");
                    check(ui, &mut self.hk.shift, "Shift");
                    check(ui, &mut self.hk.meta, "Win");
                    ui.label("+");
                    let current = KEYS.iter().find(|k| k.2 == self.hk.key).map_or("?", |k| k.1);
                    egui::ComboBox::from_id_salt("hotkey_key").selected_text(current).width(90.0).show_ui(ui, |ui| {
                        for (_, label, code) in KEYS {
                            ui.selectable_value(&mut self.hk.key, *code, *label);
                        }
                    });
                });
                match self.hk.validate() {
                    Ok(()) => {
                        self.cfg.hotkey = self.hk.to_string();
                        ui.label(RichText::new(format!("Press {} on the server's keyboard to switch.", self.hk.pretty())).color(DIM));
                    }
                    Err(e) => {
                        ui.colored_label(RED, e);
                    }
                }
                ui.add_space(4.0);
                check(ui, &mut self.cfg.edge_enabled, "Also switch by moving the mouse to the screen edge");
                if self.cfg.edge_enabled {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("The other computer is on the:");
                        for s in Side::ALL {
                            ui.radio_value(&mut self.cfg.edge_side, s.name().to_string(), s.label());
                        }
                    });
                    ui.label(RichText::new("Move the mouse off that edge to cross over. On the other computer, push against the opposite edge to come back.").color(DIM));
                    if cfg!(target_os = "linux") {
                        ui.label(RichText::new("On Linux the cursor position is estimated, so edge switching can be slightly off.").color(DIM));
                    }
                }
            });
            if running {
                ui.label(RichText::new("Stop sharing to change these settings.").color(AMBER));
            }
        });

        card(ui, false, |ui| {
            section_title(ui, "▶", "Startup");
            let before = self.autostart_on;
            check(ui, &mut self.autostart_on, "Start MKShare when I sign in");
            if self.autostart_on != before {
                match crate::autostart::set(self.autostart_on) {
                    Ok(()) => self.startup_error = None,
                    Err(e) => {
                        self.autostart_on = before;
                        self.startup_error = Some(e);
                    }
                }
            }
            check(ui, &mut self.cfg.auto_connect, "Connect automatically when MKShare opens");
            ui.label(RichText::new("Uses your saved settings. Turn on \"Remember passphrase\" on the Home page so it can connect without asking.").size(12.0).color(DIM));
            if crate::tray::AVAILABLE {
                check(ui, &mut self.cfg.tray_close, "Keep MKShare running in the tray when I close the window");
                ui.label(RichText::new("The tray icon (near the clock) opens the window again; right-click it and choose Quit to exit completely.").size(12.0).color(DIM));
            } else {
                ui.label(RichText::new("On Linux there is no tray icon: closing the window quits MKShare. With start on login it opens minimized.").size(12.0).color(DIM));
            }
            if let Some(e) = &self.startup_error {
                ui.colored_label(RED, e);
            }
        });

        card(ui, false, |ui| {
            section_title(ui, "🔄", "Updates");
            let up = self.update.lock().unwrap().clone();
            let busy = matches!(up, Up::Checking | Up::Installing);
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Update application")).clicked() {
                    self.check_update(ctx);
                }
                match &up {
                    Up::Idle => {
                        ui.label(RichText::new(format!("Version {}", crate::update::CURRENT)).color(DIM));
                    }
                    Up::Checking => {
                        ui.spinner();
                        ui.label("Checking for updates...");
                    }
                    Up::UpToDate => {
                        ui.label(RichText::new(format!("Up to date (version {})", crate::update::CURRENT)).color(GREEN));
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
                        ui.label(RichText::new(msg).color(GREEN));
                    }
                    Up::Error(_) => {}
                }
            });
            if let Up::Available(rel) = &up {
                ui.label(RichText::new(format!("A newer version ({}) is available. Installing stops sharing and restarts the app.", rel.version)).color(DIM));
            }
            if let Up::Error(e) = &up {
                ui.colored_label(RED, e);
            }
        });

        card(ui, false, |ui| {
            section_title(ui, "ℹ", "About");
            ui.label(format!("MKShare version {}", crate::update::CURRENT));
            ui.label(RichText::new(format!("Project: github.com/{UPDATE_REPO}")).color(DIM));
            ui.label(RichText::new(format!("Passphrase storage: {}", self.secret_backend)).color(DIM));
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

impl App {
    /// One frame of the whole window (separate from `update` so tests can drive it).
    fn ui(&mut self, ctx: &egui::Context) {
        if self.hide_first_frame {
            self.hide_first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Closing the window hides it in the tray (Windows) so sharing keeps running.
        if ctx.input(|i| i.viewport().close_requested()) && self.tray_ok && self.cfg.tray_close && !crate::tray::is_quitting() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        self.poll_child();
        ctx.request_repaint_after(Duration::from_millis(500));
        let (status, log) = {
            let s = self.shared.lock().unwrap();
            (s.status, s.log.clone())
        };
        let running = self.child.is_some();

        // Look for servers once when the client screen is first shown without an address.
        if self.cfg.mode == Mode::Client && !running && !self.scanned_once && self.cfg.host.trim().is_empty() {
            self.scanned_once = true;
            self.start_scan(ctx);
        }
        let scan = self.scan.lock().unwrap().clone();

        egui::SidePanel::left("nav")
            .exact_width(210.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ctx, |ui| self.sidebar(ui, status));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(24)))
            .show(ctx, |ui| match self.page {
                Page::Log => self.log_page(ui, ctx, &log),
                page => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        if page == Page::Home {
                            self.home(ui, ctx, status, &log, running, &scan);
                        } else {
                            self.settings_page(ui, ctx, running);
                        }
                    });
                }
            });
    }
}

pub fn run(background: bool) -> anyhow::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("MKShare")
        .with_app_id("mkshare")
        .with_inner_size([920.0, 800.0])
        .with_min_inner_size([760.0, 560.0]);
    if background {
        // started by "start on login": Windows hides the window (the tray icon opens it), Linux minimizes it
        if cfg!(windows) {
            viewport = viewport.with_visible(false);
        }
    }
    if let Some(i) = icon {
        viewport = viewport.with_icon(Arc::new(i));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "MKShare",
        options,
        Box::new(|cc| {
            theme(&cc.egui_ctx);
            let tray_ok = crate::tray::spawn(cc.egui_ctx.clone());
            Ok(Box::new(App::new(&cc.egui_ctx, tray_ok, background)))
        }),
    )
        .map_err(|e| anyhow::anyhow!("cannot open window: {e}"))
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    /// Drives the real UI with synthetic mouse events (no window needed).
    struct Harness {
        ctx: egui::Context,
        app: App,
        time: f64,
    }

    impl Harness {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("mkshare-ui-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            std::env::set_var("APPDATA", &dir);
            let ctx = egui::Context::default();
            theme(&ctx);
            let mut app = App::new(&ctx, false, false);
            app.cfg = Config::default();
            app.cfg.mode = Mode::Client;
            app.scanned_once = true;
            Harness { ctx, app, time: 0.0 }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(920.0, 800.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let app = &mut self.app;
            let _ = self.ctx.run(input, |ctx| app.ui(ctx));
        }

        fn click(&mut self, x: f32, y: f32) {
            let pos = egui::pos2(x, y);
            let btn = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            };
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![btn(true)]);
            self.frame(vec![btn(false)]);
            self.frame(vec![]);
        }
    }

    fn found() -> crate::discovery::Found {
        serde_json::from_str(r#"{"name":"PC-ONE","port":24811,"version":"x"}"#).map(|mut f: crate::discovery::Found| {
            f.addr = "192.168.1.78".into();
            f
        })
        .unwrap()
    }

    /// Clicking a server in the "Servers found" list must fill in its address and port.
    #[test]
    fn clicking_a_found_server_uses_it() {
        let mut h = Harness::new();
        *h.app.scan.lock().unwrap() = ScanState { running: false, done: true, results: vec![found()] };
        h.frame(vec![]);
        h.frame(vec![]);
        let mut hit = None;
        // sweep the window top to bottom: the button must be hit somewhere
        'outer: for y in (100..800).step_by(3) {
            for x in [400.0_f32, 500.0, 600.0] {
                h.app.cfg.host.clear();
                h.click(x, y as f32);
                if h.app.cfg.host == "192.168.1.78" {
                    hit = Some((x, y));
                    break 'outer;
                }
            }
        }
        let (x, y) = hit.expect("no click position selected the found server");
        println!("found server selected by a click at ({x}, {y})");
        assert_eq!(h.app.cfg.port, "24811");
        assert_eq!(h.app.cfg.server_name, "PC-ONE");
        assert_eq!(h.app.cfg.named_host, "192.168.1.78");
        // the chosen entry stays chosen after more frames, and the address field keeps the value
        h.frame(vec![]);
        assert_eq!(h.app.cfg.host, "192.168.1.78");
    }
}
