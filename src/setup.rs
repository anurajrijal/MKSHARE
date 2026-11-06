//! `mkshare setup-linux` (run as root): lets the person logged in at this computer use the
//! input devices, without adding them to the `input` group. The app's "Fix permissions"
//! button runs this through `pkexec`; the .deb package does the same in its postinst.

use anyhow::{bail, Context, Result};
use std::{fs, process::Command};

/// The same rule file the .deb installs.
const RULES: &str = include_str!("../packaging/60-mkshare.rules");

fn run_tool(name: &str, args: &[&str]) {
    match Command::new(name).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("note: `{name}` exited with {s}"),
        Err(e) => eprintln!("note: could not run `{name}`: {e}"),
    }
}

pub fn run() -> Result<()> {
    fs::write("/etc/udev/rules.d/60-mkshare.rules", RULES)
        .context("cannot write /etc/udev/rules.d/60-mkshare.rules - this must run as root (sudo mkshare setup-linux)")?;
    fs::write("/etc/modules-load.d/mkshare.conf", "# MKShare needs uinput to create virtual input devices\nuinput\n")
        .context("cannot write /etc/modules-load.d/mkshare.conf")?;
    run_tool("modprobe", &["uinput"]);
    run_tool("udevadm", &["control", "--reload"]);
    run_tool("udevadm", &["trigger", "--subsystem-match=input", "--subsystem-match=misc"]);
    if !std::path::Path::new("/dev/uinput").exists() {
        bail!("/dev/uinput still does not exist - the uinput kernel module is not available on this system");
    }
    println!("Done: the user logged in at this computer can now use MKShare (no log out needed).");
    Ok(())
}
