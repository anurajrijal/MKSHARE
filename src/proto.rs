//! Wire protocol, platform-neutral input events, screen sides and the switch shortcut.
//!
//! Key codes on the wire are Linux evdev codes (KEY_*), which identify the
//! *physical* key and therefore work independent of keyboard layout and OS.
//!
//! NOTE: new `Msg` variants must be appended at the END so that the encoding of
//! the existing ones does not change.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Msg {
    Hello,
    Ping,
    /// client -> server: please switch input to / back from the client
    Toggle,
    /// server -> client: true while the client is the one receiving input
    Active(bool),
    Move { dx: i32, dy: i32 },
    /// btn: 0 left, 1 right, 2 middle, 3 back, 4 forward
    Button { btn: u8, down: bool },
    /// Scroll amounts in 1/120 notch units (same as Windows WHEEL_DELTA)
    Wheel { dx: i32, dy: i32 },
    Key { code: u16, down: bool },
    ReleaseAll,
    /// server -> client: screen-edge switching settings. `back` is the edge of the
    /// client's screen that leads back to the server, or None when edge switching is off.
    EdgeConfig { back: Option<Side> },
    /// server -> client: the cursor crossed the edge; put it here (fractions 0..1 of the screen).
    Enter { x: f32, y: f32 },
    /// client -> server: the cursor left through the return edge at this position (fractions 0..1).
    Leave { x: f32, y: f32 },
    /// either direction: text the user copied on one computer (clipboard sharing)
    Clipboard(String),
}

/// An input event captured on the server machine.
#[derive(Debug, Clone, Copy)]
pub enum InEvent {
    Move(i32, i32),
    Button(u8, bool),
    Wheel(i32, i32),
    Key(u16, bool),
}

// ------------------------------------------------------------------- sides

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Left => "Left",
            Side::Right => "Right",
            Side::Top => "Top",
            Side::Bottom => "Bottom",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        Side::ALL.into_iter().find(|x| x.name() == s.trim().to_lowercase())
    }
}

// ---------------------------------------------------------------- shortcut

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

/// Keys that can be the main key of the switch shortcut: (name, label, evdev code).
pub const KEYS: &[(&str, &str, u16)] = &[
    ("a", "A", 30), ("b", "B", 48), ("c", "C", 46), ("d", "D", 32), ("e", "E", 18), ("f", "F", 33),
    ("g", "G", 34), ("h", "H", 35), ("i", "I", 23), ("j", "J", 36), ("k", "K", 37), ("l", "L", 38),
    ("m", "M", 50), ("n", "N", 49), ("o", "O", 24), ("p", "P", 25), ("q", "Q", 16), ("r", "R", 19),
    ("s", "S", 31), ("t", "T", 20), ("u", "U", 22), ("v", "V", 47), ("w", "W", 17), ("x", "X", 45),
    ("y", "Y", 21), ("z", "Z", 44),
    ("1", "1", 2), ("2", "2", 3), ("3", "3", 4), ("4", "4", 5), ("5", "5", 6),
    ("6", "6", 7), ("7", "7", 8), ("8", "8", 9), ("9", "9", 10), ("0", "0", 11),
    ("f1", "F1", 59), ("f2", "F2", 60), ("f3", "F3", 61), ("f4", "F4", 62), ("f5", "F5", 63),
    ("f6", "F6", 64), ("f7", "F7", 65), ("f8", "F8", 66), ("f9", "F9", 67), ("f10", "F10", 68),
    ("f11", "F11", 87), ("f12", "F12", 88),
    ("space", "Space", 57), ("tab", "Tab", 15), ("esc", "Esc", 1), ("enter", "Enter", 28),
    ("backspace", "Backspace", 14), ("insert", "Insert", 110), ("delete", "Delete", 111),
    ("home", "Home", 102), ("end", "End", 107), ("pageup", "PageUp", 104), ("pagedown", "PageDown", 109),
    ("up", "Up", 103), ("down", "Down", 108), ("left", "Left", 105), ("right", "Right", 106),
    ("scrolllock", "ScrollLock", 70), ("pause", "Pause", 119),
];

/// Keys that are safe to use without any modifier.
const STANDALONE_OK: &[&str] = &[
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "scrolllock", "pause",
];

/// The shortcut that switches input between the two computers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: u16,
}

impl Hotkey {
    pub const DEFAULT: Hotkey = Hotkey { ctrl: true, alt: true, shift: false, meta: false, key: 37 };

    pub fn key_name(&self) -> Option<&'static str> {
        KEYS.iter().find(|k| k.2 == self.key).map(|k| k.0)
    }

    pub fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.meta
    }

    /// A shortcut must not fire while typing normally.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.key_name().ok_or("Choose a key for the shortcut.")?;
        if !self.has_modifier() && !STANDALONE_OK.contains(&name) {
            return Err("Tick at least one of Ctrl, Alt, Shift or Win (or use an F-key).".into());
        }
        Ok(())
    }

    /// Parses text such as "ctrl+alt+k".
    pub fn parse(s: &str) -> Result<Hotkey, String> {
        let mut hk = Hotkey { ctrl: false, alt: false, shift: false, meta: false, key: 0 };
        for part in s.split('+').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()) {
            match part.as_str() {
                "ctrl" | "control" => hk.ctrl = true,
                "alt" => hk.alt = true,
                "shift" => hk.shift = true,
                "win" | "meta" | "super" | "cmd" => hk.meta = true,
                key => {
                    let code = KEYS.iter().find(|k| k.0 == key).map(|k| k.2).ok_or_else(|| format!("unknown key '{key}'"))?;
                    if hk.key != 0 {
                        return Err("a shortcut can have only one main key".into());
                    }
                    hk.key = code;
                }
            }
        }
        hk.validate()?;
        Ok(hk)
    }

    /// Human readable, e.g. "Ctrl+Alt+K".
    pub fn pretty(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.meta {
            parts.push("Win");
        }
        parts.push(KEYS.iter().find(|k| k.2 == self.key).map_or("?", |k| k.1));
        parts.join("+")
    }
}

/// Canonical text form, e.g. "ctrl+alt+k" (what is stored and passed on the command line).
impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.meta {
            parts.push("win");
        }
        parts.push(self.key_name().unwrap_or("?"));
        write!(f, "{}", parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_roundtrip() {
        let hk = Hotkey::parse("Ctrl+Alt+K").unwrap();
        assert_eq!(hk, Hotkey::DEFAULT);
        assert_eq!(hk.to_string(), "ctrl+alt+k");
        assert_eq!(hk.pretty(), "Ctrl+Alt+K");
        assert_eq!(Hotkey::parse(&hk.to_string()).unwrap(), hk);
        let hk2 = Hotkey::parse("win+shift+pageup").unwrap();
        assert!(hk2.meta && hk2.shift && !hk2.ctrl);
        assert_eq!(hk2.pretty(), "Shift+Win+PageUp");
    }

    #[test]
    fn hotkey_validation() {
        assert!(Hotkey::parse("k").is_err()); // plain letter would fire while typing
        assert!(Hotkey::parse("f12").is_ok()); // F-keys are fine alone
        assert!(Hotkey::parse("ctrl+alt").is_err()); // no main key
        assert!(Hotkey::parse("ctrl+a+b").is_err()); // two main keys
        assert!(Hotkey::parse("ctrl+banana").is_err());
    }

    #[test]
    fn sides() {
        for s in Side::ALL {
            assert_eq!(s.opposite().opposite(), s);
            assert_eq!(Side::parse(s.name()), Some(s));
        }
        assert_eq!(Side::parse(" RIGHT "), Some(Side::Right));
        assert_eq!(Side::parse("middle"), None);
    }

    #[test]
    fn key_table_has_no_duplicates() {
        for (i, a) in KEYS.iter().enumerate() {
            for b in &KEYS[i + 1..] {
                assert_ne!(a.0, b.0);
                assert_ne!(a.2, b.2);
            }
        }
    }
}
