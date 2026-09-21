//! SoundCloud user OAuth token cache.
//!
//! Unlike Spotify (`sources_spotify::auth`), there's no browser OAuth flow
//! here — SoundCloud has largely stopped granting new API app
//! registrations, so no client_id/secret pair to drive one exists. The
//! token has to come from a user pasting it in (see
//! `Plugin::setup_prompt` in `sources_soundcloud::plugin`), so this module
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
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use percent_encoding::percent_decode_str;
use regex::Regex;

const TOKEN: &str = r"\d-\d+-\d+-[A-Za-z0-9]+";
const NOT_FOUND: &str = "no SoundCloud token found in that — paste document.cookie or the Authorization header value";

static BARE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("^{TOKEN}$")).unwrap());
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?i)^(?:authorization\s*:\s*)?(?:(?:oauth|bearer)\s+)?({TOKEN})$")).unwrap()
});

/// The bare oauth token from a bare token, an Authorization header value, a cookie string, or a JSON/JS literal of one.
pub fn extract_token(input: &str) -> Result<String, String> {
    let text = unwrap_literal(input.trim());
    let text = text.trim();
    if let Some(c) = HEADER.captures(text) {
        return Ok(c[1].to_string());
    }
    let pairs = if text.starts_with('{') || text.starts_with('[') {
        json_pairs(text)
    } else {
        text.split(';').filter_map(|c| c.split_once('=')).map(|(n, v)| (n.trim().to_string(), v.to_string())).collect()
    };
    token_from_pairs(&pairs).ok_or_else(|| NOT_FOUND.to_string())
}

fn unwrap_literal(s: &str) -> String {
    let mut chars = s.chars();
    let (Some(q @ ('"' | '\'' | '`')), Some(last)) = (chars.next(), s.chars().next_back()) else {
        return s.to_string();
    };
    if s.len() < 2 || last != q {
        return s.to_string();
    }
    let mut out = String::new();
    let mut it = s[1..s.len() - 1].chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n' | 'r' | 't') | None => {}
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some(other) => out.push(other),
        }
    }
    out
}

/// A JSON object of name -> value, or an array of `{name, value}` objects.
fn json_pairs(text: &str) -> Vec<(String, String)> {
    use serde_json::Value;
    let str_of = |v: &Value| v.as_str().map(str::to_string);
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => m.iter().filter_map(|(k, v)| Some((k.clone(), str_of(v)?))).collect(),
        Ok(Value::Array(a)) => a.iter().filter_map(|e| Some((str_of(e.get("name")?)?, str_of(e.get("value")?)?))).collect(),
        _ => Vec::new(),
    }
}

fn token_from_pairs(pairs: &[(String, String)]) -> Option<String> {
    let value = |name: &str| {
        let (_, v) = pairs.iter().find(|(n, _)| n == name)?;
        Some(percent_decode_str(v.trim().trim_matches('"')).decode_utf8_lossy().into_owned())
    };
    let direct = value("oauth_token").filter(|t| BARE.is_match(t));
    direct.or_else(|| {
        let raw = value("_soundcloud_session")?;
        let raw = raw.trim_end_matches('=');
        let bytes = STANDARD_NO_PAD.decode(raw).or_else(|_| URL_SAFE_NO_PAD.decode(raw)).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let token = text.split("--").next()?;
        BARE.is_match(token).then(|| token.to_string())
    })
}

fn token_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("token.txt")
}

/// A previously pasted-and-cached token, if any.
pub fn load_cached(cache_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(token_path(cache_dir)).ok()?;
    extract_token(&text).ok()
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
