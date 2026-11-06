//! Protected storage for the passphrase, so it never sits in plain text in config.json.
//!
//! * Windows: encrypted with DPAPI (bound to the current Windows user account).
//! * Linux:   the system keyring via `secret-tool` (libsecret-tools) when available,
//!            otherwise a private file (mode 0600) in the config directory.

use std::path::PathBuf;

/// Directory for MKShare's settings (created on demand).
pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("mkshare"))
}

pub fn ensure_private_dir() -> Option<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    restrict(&dir, 0o700);
    Some(dir)
}

/// Make a file or directory accessible to the current user only (Unix).
pub fn restrict(path: &std::path::Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB},
    };

    fn file() -> Option<PathBuf> {
        Some(ensure_private_dir()?.join("pass.dpapi"))
    }

    pub fn backend() -> &'static str {
        "encrypted for your Windows account"
    }

    pub fn store(pass: &str) -> Result<(), String> {
        let mut data = pass.as_bytes().to_vec();
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let ok = unsafe { CryptProtectData(&input, null(), null(), null(), null(), 0, &mut out) };
        data.iter_mut().for_each(|b| *b = 0);
        if ok == 0 {
            return Err("Windows could not encrypt the passphrase".into());
        }
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        std::fs::write(file().ok_or("no settings folder")?, bytes).map_err(|e| e.to_string())
    }

    pub fn load() -> Option<String> {
        let mut bytes = std::fs::read(file()?).ok()?;
        let input = CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_mut_ptr() };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let ok = unsafe { CryptUnprotectData(&input, null_mut(), null(), null(), null(), 0, &mut out) };
        if ok == 0 {
            return None; // different Windows account, or damaged file
        }
        let plain = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        String::from_utf8(plain).ok()
    }

    pub fn clear() {
        if let Some(f) = file() {
            let _ = std::fs::remove_file(f);
        }
    }
}

// -------------------------------------------------------------------- Linux

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    const ATTRS: [&str; 4] = ["service", "mkshare", "account", "passphrase"];

    fn secret_tool() -> Command {
        let mut c = Command::new("secret-tool");
        c.stderr(Stdio::null());
        c
    }

    fn have_secret_tool() -> bool {
        secret_tool().arg("--help").stdout(Stdio::null()).status().is_ok()
    }

    fn file() -> Option<PathBuf> {
        Some(ensure_private_dir()?.join("pass"))
    }

    pub fn backend() -> &'static str {
        if have_secret_tool() {
            "system keyring"
        } else {
            "private file (install libsecret-tools for the system keyring)"
        }
    }

    fn store_in_keyring(pass: &str) -> bool {
        let Ok(mut child) = secret_tool()
            .args(["store", "--label=MKShare"])
            .args(ATTRS)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(mut i) = child.stdin.take() {
            let _ = i.write_all(pass.as_bytes());
        }
        child.wait().map_or(false, |s| s.success())
    }

    pub fn store(pass: &str) -> Result<(), String> {
        if store_in_keyring(pass) {
            if let Some(f) = file() {
                let _ = std::fs::remove_file(f); // no plain copy left behind
            }
            return Ok(());
        }
        // No usable keyring: a file only the current user can read.
        use std::os::unix::fs::OpenOptionsExt;
        let path = file().ok_or("no settings folder")?;
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        f.write_all(pass.as_bytes()).map_err(|e| e.to_string())
    }

    pub fn load() -> Option<String> {
        if let Ok(out) = secret_tool().arg("lookup").args(ATTRS).stdout(Stdio::piped()).output() {
            if out.status.success() && !out.stdout.is_empty() {
                return String::from_utf8(out.stdout).ok();
            }
        }
        std::fs::read_to_string(file()?).ok().filter(|s| !s.is_empty())
    }

    pub fn clear() {
        let _ = secret_tool().arg("clear").args(ATTRS).stdout(Stdio::null()).status();
        if let Some(f) = file() {
            let _ = std::fs::remove_file(f);
        }
    }
}

pub use imp::{backend, clear, load, store};

#[cfg(test)]
mod tests {
    #[test]
    fn store_load_clear_roundtrip() {
        // Use a private settings folder so the user's real settings are never touched.
        let dir = std::env::temp_dir().join(format!("mkshare-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if cfg!(windows) {
            std::env::set_var("APPDATA", &dir);
        } else {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
            // keep the test off the real keyring: pretend secret-tool is not installed
            std::env::set_var("PATH", "/nonexistent");
        }
        let pass = "correct horse battery staple \u{00e9}";
        super::store(pass).unwrap();
        assert_eq!(super::load().as_deref(), Some(pass));

        // the stored bytes must not contain the plain text
        let f = super::config_dir().unwrap().join(if cfg!(windows) { "pass.dpapi" } else { "pass" });
        let raw = std::fs::read(&f).unwrap();
        #[cfg(windows)]
        assert!(!raw.windows(7).any(|w| w == b"correct"), "DPAPI file contains plain text");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
            let _ = raw;
        }

        super::clear();
        assert_eq!(super::load(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
