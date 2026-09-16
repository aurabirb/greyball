//! SoundCloud user OAuth token cache.
//!
//! Unlike Spotify (`sources_spotify::auth`), there's no browser OAuth flow
//! here — SoundCloud has largely stopped granting new API app
//! registrations, so no client_id/secret pair to drive one exists. The
//! token has to come from a user pasting it in (see
//! `SetupKind::TextInput` in `sources_soundcloud::plugin`), so this module
//! is just local read/write — collecting the string itself is entirely the
//! caller's problem now, not this crate's.
//!
//! (This replaces an earlier stdin-prompt-based flow that ran before the TUI
//! took the terminal — blocking `app` startup on it, and whose Ctrl+C
//! handling never actually worked right. Moving token entry into the UI's
//! own event loop sidesteps that whole class of bug rather than fixing the
//! signal wiring: there's no separate raw-stdin-plus-SIGINT-handler path to
//! get right, `Esc` cancels the same way it does for any other text field.)

use std::path::{Path, PathBuf};

fn token_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("token.txt")
}

/// A previously pasted-and-cached token, if any.
pub fn load_cached(cache_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(token_path(cache_dir)).ok()?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Cache a freshly pasted token. Best-effort: a failure just means the next
/// launch prompts again, so it's logged, not propagated.
pub fn persist(cache_dir: &Path, token: &str) {
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let path = token_path(cache_dir);
    if let Err(e) = std::fs::write(&path, token) {
        log::warn!("soundcloud: cannot write {}: {e}", path.display());
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}
