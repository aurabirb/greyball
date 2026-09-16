//! Cache for the slskd data directory path collected via `SetupKind::TextInput`
//! — same on-disk-file pattern as `sources_soundcloud::auth`'s token cache,
//! just for a plain path instead of a secret.

use std::path::{Path, PathBuf};

fn path_file(cache_dir: &Path) -> PathBuf {
    cache_dir.join("data_dir.txt")
}

pub fn load_cached(cache_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path_file(cache_dir)).ok()?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Best-effort: a failure just means setup prompts again next launch.
pub fn persist(cache_dir: &Path, data_dir: &str) {
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let path = path_file(cache_dir);
    if let Err(e) = std::fs::write(&path, data_dir) {
        log::warn!("soulseek: cannot write {}: {e}", path.display());
    }
}
