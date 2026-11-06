//! Self-update from GitHub Releases.
//!
//! Uses the system `curl` (present on Windows 10+ and virtually every Linux)
//! so no TLS stack has to be bundled. If the release contains a
//! `SHA256SUMS.txt` the download is verified against it.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub asset_name: String,
    pub asset_url: String,
    pub sums_url: Option<String>,
}

pub enum Outcome {
    /// The new version was started; this process should exit.
    Restarted,
    /// Installed, but the user has to restart the app (e.g. after a .deb upgrade).
    Installed(String),
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

fn curl() -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--max-time", "180", "-A", "mkshare-updater"]);
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let out = curl()
        .args(["-H", "Accept: application/vnd.github+json", url])
        .output()
        .context("could not run curl - it is not installed (Debian/Ubuntu: sudo apt install curl)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("download failed: {}", err.trim());
    }
    Ok(out.stdout)
}

fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    match repo.split_once('/') {
        Some((a, b)) => ok(a) && ok(b),
        None => false,
    }
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(['.', '-', '+'])
        .map_while(|p| p.parse().ok())
        .collect()
}

fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn installed_from_package() -> bool {
    cfg!(target_os = "linux") && std::env::current_exe().map_or(false, |p| p.starts_with("/usr/"))
}

fn pick_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    if !cfg!(target_arch = "x86_64") {
        return None;
    }
    if cfg!(windows) {
        assets.iter().find(|a| a.name == "mkshare.exe").or_else(|| assets.iter().find(|a| a.name.ends_with(".exe")))
    } else if installed_from_package() {
        assets.iter().find(|a| a.name.ends_with("_amd64.deb"))
    } else {
        assets.iter().find(|a| a.name == "mkshare-linux-x86_64")
    }
}

/// Returns the newest release if it is newer than the running version.
pub fn check(repo: &str) -> Result<Option<Release>> {
    let repo = repo.trim();
    if !valid_repo(repo) {
        bail!("Enter the GitHub repository as owner/name (for example yourname/mkshare).");
    }
    let body = fetch(&format!("https://api.github.com/repos/{repo}/releases/latest")).map_err(|e| {
        if e.to_string().contains("404") {
            anyhow!("No release has been published for {repo} yet.")
        } else {
            e.context("could not reach GitHub (check your internet connection)")
        }
    })?;
    let rel: GhRelease = serde_json::from_slice(&body).context("unexpected reply from GitHub")?;
    if !is_newer(&rel.tag_name, CURRENT) {
        return Ok(None);
    }
    let asset = pick_asset(&rel.assets).ok_or_else(|| anyhow!("release {} has no file for this computer", rel.tag_name))?;
    let sums_url = rel.assets.iter().find(|a| a.name == "SHA256SUMS.txt").map(|a| a.browser_download_url.clone());
    Ok(Some(Release {
        version: rel.tag_name.trim_start_matches('v').to_string(),
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        sums_url,
    }))
}

fn sha256_hex(path: &PathBuf) -> Result<String> {
    let data = std::fs::read(path)?;
    Ok(Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect())
}

fn download(rel: &Release) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("mkshare-update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(&rel.asset_name);
    let st = curl().arg("-o").arg(&dest).arg(&rel.asset_url).output().context("could not run curl - it is not installed (Debian/Ubuntu: sudo apt install curl)")?;
    if !st.status.success() {
        bail!("download failed: {}", String::from_utf8_lossy(&st.stderr).trim());
    }
    if let Some(url) = &rel.sums_url {
        let sums = String::from_utf8(fetch(url)?).context("bad SHA256SUMS.txt")?;
        let want = sums
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let (h, n) = (it.next()?, it.next()?);
                (n.trim_start_matches('*') == rel.asset_name).then(|| h.to_lowercase())
            })
            .next()
            .ok_or_else(|| anyhow!("SHA256SUMS.txt has no entry for {}", rel.asset_name))?;
        if sha256_hex(&dest)? != want {
            let _ = std::fs::remove_file(&dest);
            bail!("the downloaded file does not match its checksum - update cancelled");
        }
    }
    Ok(dest)
}

/// Download and install `rel`. The caller should have stopped any running server/client first.
pub fn install(rel: &Release) -> Result<Outcome> {
    let file = download(rel)?;
    let exe = std::env::current_exe()?;

    if cfg!(windows) {
        // A running .exe cannot be overwritten, but it can be renamed.
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).context("cannot replace the program file (is the folder writable?)")?;
        if let Err(e) = std::fs::copy(&file, &exe) {
            let _ = std::fs::rename(&old, &exe);
            return Err(anyhow!("cannot write the new version: {e}"));
        }
        Command::new(&exe).spawn().context("updated, but could not restart - please open MKShare again")?;
        return Ok(Outcome::Restarted);
    }

    if installed_from_package() {
        let st = Command::new("pkexec")
            .args(["apt-get", "install", "-y"])
            .arg(&file)
            .status()
            .context("could not run pkexec (needs a graphical password prompt)")?;
        if !st.success() {
            bail!("package install was cancelled or failed");
        }
        return Ok(Outcome::Installed("Updated. Please close and reopen MKShare.".into()));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let staged = exe.with_extension("new");
        std::fs::copy(&file, &staged).context("cannot write next to the program (is the folder writable?)")?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&staged, &exe)?;
        Command::new(&exe).spawn().context("updated, but could not restart - please open MKShare again")?;
    }
    Ok(Outcome::Restarted)
}

/// Remove the leftover from a previous Windows update.
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old.exe"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
    }

    #[test]
    fn repo_validation() {
        assert!(valid_repo("someone/mkshare"));
        assert!(!valid_repo("someone"));
        assert!(!valid_repo("a/b/c?x=1"));
        assert!(!valid_repo("/x"));
    }
}
