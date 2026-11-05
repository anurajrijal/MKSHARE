//! Platform backends. Each exposes:
//!   * `capture(core)`  - block forever, feeding physical input to `Core::on_event`
//!   * `on_switch(on)`  - called when input moves to / returns from the remote machine
//!   * `Injector`       - replays `Msg` input events on this machine

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::*;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("mkshare supports only Windows and Linux");
