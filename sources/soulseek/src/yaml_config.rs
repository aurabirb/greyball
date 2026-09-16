//! Best-effort read of slskd's own `slskd.yml` for its actual web-API
//! credentials, so `[soulseek]` in `config.toml` doesn't need to duplicate
//! them by hand. A missing file, an unreadable file, unparsable YAML, or a
//! `web.authentication` block that's absent/commented-out in the file all
//! just mean "no override" — the caller falls back to config.toml/defaults.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Default)]
pub struct YamlAuth {
    pub username: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Deserialize, Default)]
struct Root {
    #[serde(default)]
    web: Option<Web>,
}

#[derive(Deserialize, Default)]
struct Web {
    #[serde(default)]
    authentication: Option<Auth>,
}

#[derive(Deserialize, Default)]
struct Auth {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    api_keys: Option<HashMap<String, ApiKey>>,
}

#[derive(Deserialize)]
struct ApiKey {
    key: String,
}

/// Reads `<data_dir>/slskd.yml`'s `web.authentication` block, if the file
/// exists and that block is actually present (uncommented) in it.
pub fn read(data_dir: &Path) -> Option<YamlAuth> {
    let text = std::fs::read_to_string(data_dir.join("slskd.yml")).ok()?;
    let root: Root = serde_yaml::from_str(&text).ok()?;
    let auth = root.web?.authentication?;
    let api_key = auth.api_keys.and_then(|keys| keys.into_values().next()).map(|k| k.key);
    if auth.username.is_none() && auth.password.is_none() && api_key.is_none() {
        return None;
    }
    Some(YamlAuth { username: auth.username, password: auth.password, api_key })
}
