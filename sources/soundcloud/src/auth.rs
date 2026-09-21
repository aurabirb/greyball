//! The SoundCloud user OAuth token: extraction from what the user pastes, and the local cache file.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use percent_encoding::percent_decode_str;
use regex::Regex;

const TOKEN: &str = r"\d+-\d+-\d+-[A-Za-z0-9_.~-]+";
const NOT_FOUND: &str = "no SoundCloud token found in that; paste document.cookie or the Authorization header value as a single line";

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
    let hex4 = |it: &mut std::str::Chars| u32::from_str_radix(&it.by_ref().take(4).collect::<String>(), 16).ok();
    let mut out = String::new();
    let mut it = s[1..s.len() - 1].chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            None => {}
            Some('u') => {
                let mut code = hex4(&mut it);
                if let Some(hi @ 0xD800..0xDC00) = code {
                    let mut ahead = it.clone();
                    let low = (ahead.next() == Some('\\') && ahead.next() == Some('u')).then(|| hex4(&mut ahead)).flatten();
                    code = match low {
                        Some(lo @ 0xDC00..0xE000) => {
                            it = ahead;
                            Some(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                        }
                        _ => None,
                    };
                }
                out.push(code.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
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
    // Percent-decoding comes before the shape check, so an encoded token is accepted.
    let values = |name: &'static str| {
        pairs.iter().filter(move |(n, _)| n == name).map(|(_, v)| percent_decode_str(v.trim().trim_matches('"')).decode_utf8_lossy().into_owned())
    };
    values("oauth_token").find(|t| BARE.is_match(t)).or_else(|| {
        values("_soundcloud_session").find_map(|raw| {
            let raw = raw.trim_end_matches('=');
            let bytes = STANDARD_NO_PAD.decode(raw).or_else(|_| URL_SAFE_NO_PAD.decode(raw)).ok()?;
            let text = String::from_utf8(bytes).ok()?;
            let token = text.split("--").next()?;
            BARE.is_match(token).then(|| token.to_string())
        })
    })
}

fn token_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("token.txt")
}

/// A previously pasted-and-cached token: `Ok(None)` when there is no file, `Err` when it no longer parses.
pub fn load_cached(cache_dir: &Path) -> Result<Option<String>, String> {
    let path = token_path(cache_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    extract_token(&text).map(Some).map_err(|e| format!("{} is unusable: {e}", path.display()))
}

/// Cache a freshly pasted token, readable by the owner only from the moment the file exists.
pub fn persist(cache_dir: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(cache_dir)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(token_path(cache_dir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(token.as_bytes())
}
