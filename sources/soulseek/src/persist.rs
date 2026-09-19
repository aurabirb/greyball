//! What Soulseek setup remembers between launches, in `state.json` under the plugin's cache directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub data_dir: Option<String>,
    pub connection: Option<Connection>,
    pub docker: Option<Docker>,
}

#[derive(Serialize, Deserialize)]
pub struct Connection {
    pub base_url: String,
    pub username: String,
    pub password: String,
}

/// The container medley created: its slskd folder and the media cache it mounted as downloads.
#[derive(Clone, Serialize, Deserialize)]
pub struct Docker {
    pub folder: String,
    pub mounted_cache: PathBuf,
}

fn path_file(cache_dir: &Path) -> PathBuf {
    cache_dir.join("state.json")
}

pub fn load(cache_dir: &Path) -> State {
    std::fs::read_to_string(path_file(cache_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Best-effort: a failure just means setup prompts again next launch.
pub fn save(cache_dir: &Path, state: &State) {
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let path = path_file(cache_dir);
    let written = serde_json::to_string(state).map_err(std::io::Error::other).and_then(|t| std::fs::write(&path, t));
    if let Err(e) = written {
        log::warn!("soulseek: cannot write {}: {e}", path.display());
    }
}
