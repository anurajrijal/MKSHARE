//! Wire protocol and platform-neutral input events.
//!
//! Key codes on the wire are Linux evdev codes (KEY_*), which identify the
//! *physical* key and therefore work independent of keyboard layout and OS.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Msg {
    Hello,
    Ping,
    Move { dx: i32, dy: i32 },
    /// btn: 0 left, 1 right, 2 middle, 3 back, 4 forward
    Button { btn: u8, down: bool },
    /// Scroll amounts in 1/120 notch units (same as Windows WHEEL_DELTA)
    Wheel { dx: i32, dy: i32 },
    Key { code: u16, down: bool },
    ReleaseAll,
}

/// An input event captured on the server machine.
#[derive(Debug, Clone, Copy)]
pub enum InEvent {
    Move(i32, i32),
    Button(u8, bool),
    Wheel(i32, i32),
    Key(u16, bool),
}

// evdev key codes used for the switch hotkey (Ctrl+Alt+K)
pub const KEY_K: u16 = 37;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;
