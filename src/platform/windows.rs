//! Windows backend: low-level hooks (capture + suppress) and SendInput (inject).

use crate::{core::Core, proto::*};
use anyhow::{bail, Result};
use std::{
    collections::HashSet,
    mem::{size_of, zeroed},
    ptr::null_mut,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc, OnceLock,
    },
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::*,
        WindowsAndMessaging::*,
    },
};

static CORE: OnceLock<Arc<Core>> = OnceLock::new();
static ANCHOR_X: AtomicI32 = AtomicI32::new(0);
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);
static SAVED_X: AtomicI32 = AtomicI32::new(0);
static SAVED_Y: AtomicI32 = AtomicI32::new(0);

// ------------------------------------------------------------ key mapping
// Windows hardware scan codes (set 1) <-> Linux evdev codes.
// Non-extended keys 0x01..=0x58 are numerically identical in both.

const EXTENDED: &[(u16, u16)] = &[
    (0x1C, 96),  // keypad enter
    (0x1D, 97),  // right ctrl
    (0x35, 98),  // keypad /
    (0x37, 99),  // print screen
    (0x38, 100), // right alt
    (0x47, 102), // home
    (0x48, 103), // up
    (0x49, 104), // page up
    (0x4B, 105), // left
    (0x4D, 106), // right
    (0x4F, 107), // end
    (0x50, 108), // down
    (0x51, 109), // page down
    (0x52, 110), // insert
    (0x53, 111), // delete
    (0x5B, 125), // left win
    (0x5C, 126), // right win
    (0x5D, 127), // menu
    (0x20, 113), // mute
    (0x2E, 114), // volume down
    (0x30, 115), // volume up
    (0x22, 164), // play/pause
    (0x19, 163), // next track
    (0x10, 165), // previous track
    (0x24, 166), // stop
];

fn scan_to_evdev(scan: u16, ext: bool) -> Option<u16> {
    if ext {
        EXTENDED.iter().find(|(s, _)| *s == scan).map(|(_, e)| *e)
    } else if (1..=0x58).contains(&scan) {
        Some(scan)
    } else {
        None
    }
}

fn evdev_to_scan(code: u16) -> Option<(u16, bool)> {
    if let Some((s, _)) = EXTENDED.iter().find(|(_, e)| *e == code) {
        Some((*s, true))
    } else if (1..=0x58).contains(&code) {
        Some((code, false))
    } else {
        None
    }
}

// ---------------------------------------------------------------- capture

pub fn capture(core: Arc<Core>) -> Result<()> {
    let _ = CORE.set(core);
    unsafe {
        SetProcessDPIAware();
        let hmod = GetModuleHandleW(std::ptr::null());
        let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), hmod, 0);
        let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(ms_proc), hmod, 0);
        if kh.is_null() || mh.is_null() {
            bail!("could not install input hooks");
        }
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        if k.flags & LLKHF_INJECTED == 0 {
            let ext = k.flags & LLKHF_EXTENDED != 0;
            if let Some(c) = scan_to_evdev(k.scanCode as u16, ext) {
                let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                if let Some(core) = CORE.get() {
                    if core.on_event(InEvent::Key(c, down)) {
                        return 1;
                    }
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

unsafe extern "system" fn ms_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        if let Some(core) = CORE.get() {
            let m = &*(lp as *const MSLLHOOKSTRUCT);
            if m.flags & LLMHF_INJECTED == 0 && core.is_remote() {
                let hi = (m.mouseData >> 16) as u16;
                let delta = (m.mouseData >> 16) as u16 as i16 as i32;
                let ev = match wp as u32 {
                    WM_MOUSEMOVE => {
                        let dx = m.pt.x - ANCHOR_X.load(Ordering::Relaxed);
                        let dy = m.pt.y - ANCHOR_Y.load(Ordering::Relaxed);
                        (dx != 0 || dy != 0).then_some(InEvent::Move(dx, dy))
                    }
                    WM_LBUTTONDOWN => Some(InEvent::Button(0, true)),
                    WM_LBUTTONUP => Some(InEvent::Button(0, false)),
                    WM_RBUTTONDOWN => Some(InEvent::Button(1, true)),
                    WM_RBUTTONUP => Some(InEvent::Button(1, false)),
                    WM_MBUTTONDOWN => Some(InEvent::Button(2, true)),
                    WM_MBUTTONUP => Some(InEvent::Button(2, false)),
                    WM_XBUTTONDOWN => Some(InEvent::Button(if hi == 1 { 3 } else { 4 }, true)),
                    WM_XBUTTONUP => Some(InEvent::Button(if hi == 1 { 3 } else { 4 }, false)),
                    WM_MOUSEWHEEL => Some(InEvent::Wheel(0, delta)),
                    WM_MOUSEHWHEEL => Some(InEvent::Wheel(delta, 0)),
                    _ => None,
                };
                if let Some(ev) = ev {
                    core.on_event(ev);
                }
                return 1; // swallow everything while input is remote
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

pub fn on_switch(on: bool) {
    unsafe {
        if on {
            let mut p: POINT = zeroed();
            GetCursorPos(&mut p);
            SAVED_X.store(p.x, Ordering::Relaxed);
            SAVED_Y.store(p.y, Ordering::Relaxed);
            let ax = GetSystemMetrics(SM_CXSCREEN) / 2;
            let ay = GetSystemMetrics(SM_CYSCREEN) / 2;
            ANCHOR_X.store(ax, Ordering::Relaxed);
            ANCHOR_Y.store(ay, Ordering::Relaxed);
            SetCursorPos(ax, ay);
            release_local_modifiers();
        } else {
            SetCursorPos(SAVED_X.load(Ordering::Relaxed), SAVED_Y.load(Ordering::Relaxed));
        }
    }
}

/// The hotkey modifiers are physically down but we are about to swallow their
/// release, so release them locally now (preceded by a dummy key so that
/// letting go of Alt does not open a menu).
unsafe fn release_local_modifiers() {
    let mut inputs = vec![key_vk(0xE8, false), key_vk(0xE8, true)];
    for vk in [VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LSHIFT, VK_RSHIFT, VK_LWIN, VK_RWIN] {
        if GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 {
            inputs.push(key_vk(vk, true));
        }
    }
    send(&inputs);
}

// --------------------------------------------------------------- injection

unsafe fn send(inputs: &[INPUT]) {
    SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
}

fn key_vk(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn key_scan(scan: u16, ext: bool, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if ext {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

pub struct Injector {
    keys: HashSet<u16>,
    buttons: HashSet<u8>,
}

impl Injector {
    pub fn new() -> Result<Self> {
        Ok(Injector { keys: HashSet::new(), buttons: HashSet::new() })
    }

    pub fn apply(&mut self, m: &Msg) {
        unsafe {
            match *m {
                Msg::Move { dx, dy } => send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)]),
                Msg::Button { btn, down } => {
                    let (flags, data) = match (btn, down) {
                        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
                        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
                        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                        (3, true) => (MOUSEEVENTF_XDOWN, 1),
                        (3, false) => (MOUSEEVENTF_XUP, 1),
                        (4, true) => (MOUSEEVENTF_XDOWN, 2),
                        (4, false) => (MOUSEEVENTF_XUP, 2),
                        _ => return,
                    };
                    if down {
                        self.buttons.insert(btn);
                    } else {
                        self.buttons.remove(&btn);
                    }
                    send(&[mouse(0, 0, data, flags)]);
                }
                Msg::Wheel { dx, dy } => {
                    if dy != 0 {
                        send(&[mouse(0, 0, dy, MOUSEEVENTF_WHEEL)]);
                    }
                    if dx != 0 {
                        send(&[mouse(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
                    }
                }
                Msg::Key { code, down } => {
                    if let Some((scan, ext)) = evdev_to_scan(code) {
                        if down {
                            self.keys.insert(code);
                        } else {
                            self.keys.remove(&code);
                        }
                        send(&[key_scan(scan, ext, !down)]);
                    }
                }
                Msg::ReleaseAll => self.release_all(),
                Msg::Hello | Msg::Ping => {}
            }
        }
    }

    pub fn release_all(&mut self) {
        unsafe {
            for code in self.keys.drain() {
                if let Some((scan, ext)) = evdev_to_scan(code) {
                    send(&[key_scan(scan, ext, true)]);
                }
            }
            for btn in self.buttons.drain() {
                let (f, d) = match btn {
                    0 => (MOUSEEVENTF_LEFTUP, 0),
                    1 => (MOUSEEVENTF_RIGHTUP, 0),
                    2 => (MOUSEEVENTF_MIDDLEUP, 0),
                    3 => (MOUSEEVENTF_XUP, 1),
                    _ => (MOUSEEVENTF_XUP, 2),
                };
                send(&[mouse(0, 0, d, f)]);
            }
        }
    }
}
