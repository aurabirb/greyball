//! `:update` and the startup check — replace the installed binary with the latest GitHub release.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::http_fetch::{fetch_url_bytes, fetch_url_to};

const REPO: &str = "aurabirb/greyball";

/// Package-manager-owned prefixes, which an update never touches.
const MANAGED_PREFIXES: &[&str] = &["/usr/", "/bin/", "/sbin/", "/opt/", "/nix/", "/snap/", "/var/lib/"];

/// (OS, arch, suffix) of the assets `cd.yml` publishes.
const PLATFORMS: &[(&str, &str, &str)] = &[
    ("linux", "x86_64", "linux-x86_64"),
    ("linux", "aarch64", "linux-arm64"),
    ("macos", "x86_64", "macos-x86_64"),
    ("macos", "aarch64", "macos-aarch64"),
];

/// Set once a newer binary is installed, so the periodic check stops re-downloading it.
pub static INSTALLED: AtomicBool = AtomicBool::new(false);

pub enum Outcome {
    Current,
    /// The release is tagged but its binary for this platform is not uploaded yet.
    NotReady,
    Installed(String),
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

fn install_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(Path::new(&home).join(".local/bin/medley"))
}

fn on_path() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("medley"))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The PATH entry to repoint at `install`, or `None` when it already reaches it.
fn link_to_replace(install: &Path) -> Result<Option<PathBuf>, String> {
    let Some(found) = on_path() else { return Ok(None) };
    if found == install || fs::canonicalize(&found).ok() == fs::canonicalize(install).ok() && install.exists() {
        return Ok(None);
    }
    let resolved = fs::canonicalize(&found).unwrap_or_else(|_| found.clone());
    let owner = |p: &Path| MANAGED_PREFIXES.iter().any(|m| p.starts_with(m)) || p.to_string_lossy().contains("/Cellar/");
    if owner(&found) || owner(&resolved) {
        return Err(format!("{} is owned by a package manager; update it there", found.display()));
    }
    let dir = found.parent().ok_or("bad PATH entry")?;
    tempfile::NamedTempFile::new_in(dir).map_err(|e| format!("{} is not writable: {e}", dir.display()))?;
    Ok(Some(found))
}

/// Installs the latest release when newer than the running one.
pub fn run() -> Result<Outcome, String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let suffix = PLATFORMS
        .iter()
        .find(|(o, a, _)| *o == os && *a == arch)
        .map(|(_, _, s)| *s)
        .ok_or_else(|| format!("no release binary for {os}-{arch}"))?;

    let body = fetch_url_bytes(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .map_err(|e| format!("checking the latest release failed: {e}"))?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("unreadable release info: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("release info has no tag_name")?;
    let latest = parse_version(tag).ok_or_else(|| format!("unrecognized release tag {tag}"))?;
    if latest <= parse_version(env!("CARGO_PKG_VERSION")).ok_or("running version is not semver")? {
        return Ok(Outcome::Current);
    }
    let asset = format!("medley-{suffix}");
    let url = json["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["name"] == asset.as_str()))
        .and_then(|a| a["browser_download_url"].as_str());
    let Some(url) = url else { return Ok(Outcome::NotReady) };

    let path = install_path()?;
    let link = link_to_replace(&path)?;
    let dir = path.parent().ok_or("bad install path")?;
    fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| format!("writing to {}: {e}", dir.display()))?;
    fetch_url_to(url, &mut tmp)
        .map_err(|e| format!("downloading {tag} failed: {e}"))?;
    tmp.flush().map_err(|e| e.to_string())?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    tmp.persist(&path).map_err(|e| format!("installing to {}: {}", path.display(), e.error))?;

    if let Some(found) = link {
        let tmp_link = found.with_file_name(format!(".medley-update-{}", std::process::id()));
        symlink(&path, &tmp_link)
            .and_then(|()| fs::rename(&tmp_link, &found))
            .map_err(|e| format!("linking {} to {}: {e}", found.display(), path.display()))?;
    }
    INSTALLED.store(true, Ordering::Relaxed);
    Ok(Outcome::Installed(format!("{tag} is ready; restart to use it")))
}
