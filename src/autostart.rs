//! "Start MKShare when I sign in": a per-user entry that launches `mkshare --background`.
//!
//! Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (no administrator rights needed).
//! Linux:   `~/.config/autostart/mkshare.desktop`.

use std::path::PathBuf;

fn exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

#[cfg(windows)]
mod imp {
    use std::process::{Command, Stdio};

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "MKShare";

    fn reg(args: &[&str]) -> Option<std::process::Output> {
        use std::os::windows::process::CommandExt;
        Command::new("reg")
            .args(args)
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .ok()
    }

    pub fn is_enabled() -> bool {
        reg(&["query", KEY, "/v", NAME]).is_some_and(|o| o.status.success())
    }

    pub fn set(on: bool) -> Result<(), String> {
        let out = if on {
            let exe = super::exe().ok_or("cannot locate the program")?;
            let value = format!("\"{}\" --background", exe.display());
            reg(&["add", KEY, "/v", NAME, "/t", "REG_SZ", "/d", &value, "/f"])
        } else {
            if !is_enabled() {
                return Ok(());
            }
            reg(&["delete", KEY, "/v", NAME, "/f"])
        };
        match out {
            Some(o) if o.status.success() => Ok(()),
            _ => Err("could not change the Windows startup entry".into()),
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;

    fn file() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart").join("mkshare.desktop"))
    }

    pub fn is_enabled() -> bool {
        file().is_some_and(|f| f.exists())
    }

    pub fn set(on: bool) -> Result<(), String> {
        let path = file().ok_or("cannot find your config folder")?;
        if !on {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let exe = super::exe().ok_or("cannot locate the program")?;
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=MKShare\nComment=Share keyboard and mouse\nExec=\"{}\" --background\nIcon=mkshare\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, entry).map_err(|e| e.to_string())
    }
}

pub use imp::*;
