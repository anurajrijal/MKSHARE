//! Linux backend (X11 and Wayland): evdev (capture, exclusive grab) and uinput (inject).
//!
//! Needs read access to /dev/input/event* and write access to /dev/uinput:
//! run as root, or add your user to the `input` group and install the udev rule
//! described in the README.

use crate::{core::Core, edge::Rect, proto::*};
use anyhow::{bail, Context, Result};
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsInfo, AbsoluteAxisType, AttributeSet, Device, EventType, InputEvent, Key, RelativeAxisType, UinputAbsSetup,
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const BTN_LEFT: u16 = 0x110;
const BTN_FORWARD_END: u16 = 0x114; // BTN_EXTRA
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;

fn btn_index(code: u16) -> Option<u8> {
    (BTN_LEFT..=BTN_FORWARD_END).contains(&code).then(|| (code - BTN_LEFT) as u8)
}

// ---------------------------------------------------------------- capture

pub fn on_switch(_on: bool) {}

// ------------------------------------------------- screen size and cursor placement

const ABS_MAX: i32 = 32767;
static WARP: OnceLock<Mutex<Option<VirtualDevice>>> = OnceLock::new();

fn build_warp_device() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::BTN_LEFT); // makes desktops treat it as a pointer
    let info = AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0);
    Ok(VirtualDeviceBuilder::new()
        .context("cannot open /dev/uinput (permission?)")?
        .name("mkshare cursor placement")
        .with_keys(&keys)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_X, info))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, info))?
        .build()?)
}

/// Create the helper device used to place the cursor. Call early: desktops need a
/// moment to notice a new input device.
pub fn init() {
    WARP.get_or_init(|| {
        Mutex::new(
            build_warp_device()
                .map_err(|e| eprintln!("Cursor placement unavailable: {e:#}"))
                .ok(),
        )
    });
}

/// Put the cursor at a position given as fractions (0..1) of the screen.
pub fn warp(xf: f32, yf: f32) {
    init();
    let v = |f: f32| (f.clamp(0.002, 0.998) * ABS_MAX as f32) as i32;
    let Some(slot) = WARP.get() else { return };
    if let Some(dev) = slot.lock().unwrap().as_mut() {
        let _ = dev.emit(&[
            InputEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, v(xf)),
            InputEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, v(yf)),
        ]);
    }
}

/// The real cursor position cannot be read on Wayland; the caller estimates it.
pub fn cursor_pos() -> Option<(i32, i32)> {
    None
}

/// Screen size: `MKSHARE_SCREEN=1920x1080` if set, otherwise the first connected display's mode.
pub fn screen_rect() -> Option<Rect> {
    let parse = |s: &str| -> Option<Rect> {
        let (w, h) = s.trim().split_once(['x', 'X'])?;
        let h: String = h.chars().take_while(|c| c.is_ascii_digit()).collect(); // "1080i" -> 1080
        let (w, h) = (w.trim().parse().ok()?, h.parse().ok()?);
        (w > 0 && h > 0).then_some(Rect { x: 0, y: 0, w, h })
    };
    if let Ok(v) = std::env::var("MKSHARE_SCREEN") {
        return parse(&v);
    }
    let mut dirs: Vec<_> = std::fs::read_dir("/sys/class/drm").ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for p in dirs {
        let connected = std::fs::read_to_string(p.join("status")).map_or(false, |s| s.trim() == "connected");
        if connected {
            if let Some(r) = std::fs::read_to_string(p.join("modes")).ok().and_then(|m| m.lines().next().and_then(parse)) {
                return Some(r);
            }
        }
    }
    None
}

pub fn capture(core: Arc<Core>) -> Result<()> {
    init();
    let mut handles = Vec::new();
    for (_path, dev) in evdev::enumerate() {
        let name = dev.name().unwrap_or("unknown").to_string();
        if name.starts_with("mkshare") {
            continue;
        }
        let keys = dev.supported_keys();
        let is_kbd = keys.map_or(false, |k| k.contains(Key::KEY_A) && k.contains(Key::KEY_ENTER));
        let is_mouse = keys.map_or(false, |k| k.contains(Key::BTN_LEFT))
            && dev
                .supported_relative_axes()
                .map_or(false, |r| r.contains(RelativeAxisType::REL_X) && r.contains(RelativeAxisType::REL_Y));
        if !(is_kbd || is_mouse) {
            continue;
        }
        println!("Capturing: {name}");
        let core = core.clone();
        handles.push(thread::spawn(move || {
            if let Err(e) = run_device(core, dev, &name) {
                eprintln!("Device '{name}' stopped: {e:#}");
            }
        }));
    }
    if handles.is_empty() {
        bail!("no readable keyboard/mouse found - run as root or add your user to the 'input' group (see README)");
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn clone_device(dev: &Device, name: &str) -> Result<VirtualDevice> {
    let vname = format!("mkshare passthrough ({name})");
    let mut b = VirtualDeviceBuilder::new()
        .context("cannot open /dev/uinput (permission?)")?
        .name(&vname);
    if let Some(k) = dev.supported_keys() {
        b = b.with_keys(k)?;
    }
    if let Some(r) = dev.supported_relative_axes() {
        b = b.with_relative_axes(r)?;
    }
    Ok(b.build()?)
}

fn run_device(core: Arc<Core>, mut dev: Device, name: &str) -> Result<()> {
    let mut vdev = clone_device(&dev, name)?;

    // Don't grab while a key (e.g. the Enter used to launch us) is still held,
    // otherwise the desktop would never see its release.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        match dev.get_key_state() {
            Ok(s) if s.iter().next().is_none() => break,
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
    dev.grab().context("could not grab device")?;

    let mut pressed: HashSet<u16> = HashSet::new();
    let mut batch: Vec<InputEvent> = Vec::new();
    loop {
        let events: Vec<InputEvent> = dev.fetch_events()?.collect();
        for ev in events {
            if ev.event_type() == EventType::SYNCHRONIZATION {
                if ev.code() == 0 {
                    process(&core, &mut vdev, &mut pressed, &mut batch)?;
                }
                batch.clear_if_dropped(ev.code());
                continue;
            }
            batch.push(ev);
        }
    }
}

trait BatchExt {
    fn clear_if_dropped(&mut self, syn_code: u16);
}
impl BatchExt for Vec<InputEvent> {
    /// SYN_DROPPED (3): the kernel buffer overflowed, discard the partial batch.
    fn clear_if_dropped(&mut self, syn_code: u16) {
        if syn_code == 3 {
            self.clear();
        }
    }
}

fn process(core: &Core, vdev: &mut VirtualDevice, pressed: &mut HashSet<u16>, batch: &mut Vec<InputEvent>) -> Result<()> {
    // Switched to remote while keys were passed through locally: release them.
    if core.is_remote() && !pressed.is_empty() {
        let ups: Vec<InputEvent> = pressed.drain().map(|c| InputEvent::new(EventType::KEY, c, 0)).collect();
        vdev.emit(&ups)?;
    }

    let (mut dx, mut dy, mut wx, mut wy) = (0, 0, 0, 0);
    let (mut ldx, mut ldy) = (0, 0); // movement passed through locally (for edge detection)
    let mut pass: Vec<InputEvent> = Vec::new();
    for ev in batch.drain(..) {
        let (code, val) = (ev.code(), ev.value());
        match ev.event_type() {
            EventType::KEY => {
                let down = val != 0;
                let suppress = match btn_index(code) {
                    Some(i) => core.on_event(InEvent::Button(i, down)),
                    None => core.on_event(InEvent::Key(code, down)),
                };
                if !suppress {
                    if down {
                        pressed.insert(code);
                    } else {
                        pressed.remove(&code);
                    }
                    pass.push(ev);
                }
            }
            EventType::RELATIVE => {
                if core.is_remote() {
                    match code {
                        REL_X => dx += val,
                        REL_Y => dy += val,
                        REL_WHEEL => wy += val * 120,
                        REL_HWHEEL => wx += val * 120,
                        _ => {} // hi-res wheel duplicates REL_WHEEL
                    }
                } else {
                    match code {
                        REL_X => ldx += val,
                        REL_Y => ldy += val,
                        _ => {}
                    }
                    pass.push(ev);
                }
            }
            _ => {
                if !core.is_remote() {
                    pass.push(ev);
                }
            }
        }
    }
    // Screen-edge switching: if this movement reached the edge, input moves to the other computer
    // and the rest of the movement must not reach this one.
    if !core.is_remote() && (ldx != 0 || ldy != 0) && core.on_local_delta(ldx, ldy) {
        pass.retain(|e| !(e.event_type() == EventType::RELATIVE && (e.code() == REL_X || e.code() == REL_Y)));
    }
    if dx != 0 || dy != 0 {
        core.on_event(InEvent::Move(dx, dy));
    }
    if wx != 0 || wy != 0 {
        core.on_event(InEvent::Wheel(wx, wy));
    }
    if !pass.is_empty() {
        vdev.emit(&pass)?;
    }
    Ok(())
}

// --------------------------------------------------------------- injection

pub struct Injector {
    dev: VirtualDevice,
    keys: HashSet<u16>,
    buttons: HashSet<u16>,
    acc_x: i32,
    acc_y: i32,
}

impl Injector {
    pub fn new() -> Result<Self> {
        init();
        let mut keys = AttributeSet::<Key>::new();
        for c in 1..=248u16 {
            keys.insert(Key::new(c));
        }
        for c in BTN_LEFT..=BTN_FORWARD_END {
            keys.insert(Key::new(c));
        }
        let mut axes = AttributeSet::<RelativeAxisType>::new();
        for a in [
            RelativeAxisType::REL_X,
            RelativeAxisType::REL_Y,
            RelativeAxisType::REL_WHEEL,
            RelativeAxisType::REL_HWHEEL,
            RelativeAxisType::REL_WHEEL_HI_RES,
            RelativeAxisType::REL_HWHEEL_HI_RES,
        ] {
            axes.insert(a);
        }
        let dev = VirtualDeviceBuilder::new()
            .context("cannot open /dev/uinput: no permission. Use the 'Fix permissions' button in the app (or run 'sudo mkshare setup-linux'); alternatively add yourself to the 'input' group (sudo usermod -aG input $USER, then log out and in) - see README")?
            .name("mkshare virtual input")
            .with_keys(&keys)?
            .with_relative_axes(&axes)?
            .build()?;
        Ok(Injector { dev, keys: HashSet::new(), buttons: HashSet::new(), acc_x: 0, acc_y: 0 })
    }

    fn emit(&mut self, evs: &[InputEvent]) {
        if let Err(e) = self.dev.emit(evs) {
            eprintln!("inject error: {e}");
        }
    }

    pub fn apply(&mut self, m: &Msg) {
        match *m {
            Msg::Move { dx, dy } => self.emit(&[
                InputEvent::new(EventType::RELATIVE, REL_X, dx),
                InputEvent::new(EventType::RELATIVE, REL_Y, dy),
            ]),
            Msg::Button { btn, down } => {
                let code = BTN_LEFT + btn as u16;
                if code > BTN_FORWARD_END {
                    return;
                }
                if down {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                self.emit(&[InputEvent::new(EventType::KEY, code, down as i32)]);
            }
            Msg::Wheel { dx, dy } => {
                self.acc_x += dx;
                self.acc_y += dy;
                let (sx, sy) = (self.acc_x / 120, self.acc_y / 120);
                self.acc_x -= sx * 120;
                self.acc_y -= sy * 120;
                let mut evs = Vec::new();
                if dy != 0 {
                    evs.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_WHEEL_HI_RES.0, dy));
                }
                if sy != 0 {
                    evs.push(InputEvent::new(EventType::RELATIVE, REL_WHEEL, sy));
                }
                if dx != 0 {
                    evs.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_HWHEEL_HI_RES.0, dx));
                }
                if sx != 0 {
                    evs.push(InputEvent::new(EventType::RELATIVE, REL_HWHEEL, sx));
                }
                if !evs.is_empty() {
                    self.emit(&evs);
                }
            }
            Msg::Key { code, down } => {
                if code == 0 || code > 248 {
                    return;
                }
                let value = if !down {
                    self.keys.remove(&code);
                    0
                } else if self.keys.insert(code) {
                    1
                } else {
                    2 // auto-repeat
                };
                self.emit(&[InputEvent::new(EventType::KEY, code, value)]);
            }
            Msg::ReleaseAll => self.release_all(),
            Msg::Hello | Msg::Ping | Msg::Toggle | Msg::Active(_) => {}
            Msg::EdgeConfig { .. } | Msg::Enter { .. } | Msg::Leave { .. } | Msg::Clipboard(_) => {}
        }
    }

    pub fn release_all(&mut self) {
        let evs: Vec<InputEvent> = self
            .keys
            .drain()
            .chain(self.buttons.drain())
            .map(|c| InputEvent::new(EventType::KEY, c, 0))
            .collect();
        if !evs.is_empty() {
            self.emit(&evs);
        }
    }
}
