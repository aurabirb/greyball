//! OAuth login + credential/token cache.
//!
//! Two *separate* Spotify apps are involved, each OAuth'd independently —
//! not one id doing double duty:
//!
//! * [`MUSIC_CLIENT_ID`] — Spotify's own official client id (the one
//!   librespot/most third-party clients use for the streaming/Connect
//!   session). Used only to obtain librespot [`Credentials`].
//! * [`WEBAPI_CLIENT_ID`] — the dedicated `blueball`/medley app registered
//!   on the Spotify dashboard. Its "APIs used" is Web API only (Development
//!   mode, no Web Playback SDK/streaming grant) — using it for the librespot
//!   session was the bug; it's only ever meant to mint the bearer token for
//!   `webapi.rs` search/resolve calls.
//!
//! The Web API side supports several named credential *pairs* at once
//! (`TokenStore`, `webapi_tokens.json`) instead of a single cached token:
//! `_spotify addlogin [name] [client_id]` (see `crate::plugin`) adds one
//! without disturbing whatever's already stored, and `webapi.rs`'s `403`
//! fallback (`fallback_webapi_token`) switches the active one when the
//! current pair is refused a call medley's own app isn't approved for.
//!
//! Artefacts are cached under the medley data dir (`$XDG_DATA_HOME/medley/
//! spotify/`):
//!
//! * `credentials.json` — librespot's reusable auth blob, written by
//!   `Session::connect(_, true)`. Re-used forever; never expires. Shared by
//!   every Web API credential pair — only the Web API token layer is
//!   multi-account, playback isn't.
//! * `webapi_tokens.json` — every named Web API credential pair (access +
//!   refresh token) plus which one is active.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Instant;

use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_oauth::OAuthClientBuilder;
use serde::{Deserialize, Serialize};

/// Spotify's own official client id — not something we registered. Used
/// solely for the librespot playback/Connect session. Its redirect URI
/// matching is loopback-any-port (RFC 8252 style), so a
/// dynamically chosen free port works here.
pub(crate) const MUSIC_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

/// The dedicated `blueball` app registered for medley on the Spotify
/// developer dashboard — Development mode, "APIs used: Web API" only. Used
/// solely to mint the bearer token for Web API search/resolve calls; never
/// for the librespot session (it has no streaming/Connect grant).
const WEBAPI_CLIENT_ID: &str = "89485716cfd24928b4d7ffc2bee5e07e";

/// Must exactly match a redirect URI registered against `WEBAPI_CLIENT_ID`
/// on the Spotify developer dashboard — Spotify validates it byte-for-byte
/// for this app, unlike `MUSIC_CLIENT_ID`'s loopback-any-port match. (The
/// app also has `https://medley.awebo.click/spcb/` registered, for a
/// remote/non-loopback flow — not wired up: `librespot-oauth` only opens a
/// local listener for an `http://127.0.0.1:<port>/…` redirect_uri.)
const WEBAPI_REDIRECT_URI: &str = "http://127.0.0.1:3121/callback";

/// ncspot's own published Web API client id (`src/authentication.rs`,
/// https://github.com/hrkfdn/ncspot) — a working fallback for endpoints
/// medley's own Development-mode app 403s on (e.g. `/me/tracks` PUT, Like).
/// The default for `addlogin`'s `client_id`. Redirect matching is
/// loopback-any-port, like `MUSIC_CLIENT_ID`.
const NCSPOT_CLIENT_ID: &str = "d420a117a32841c2b3474932e49fb54b";
/// Name of the original single Web API credential pair, from before
/// multiple pairs existed — the one `Auth::login`/`probe`/`wiring` use
/// unless something promoted a different one active.
const DEFAULT_ACCOUNT: &str = "default";

/// `SPOTIFY_WEBAPI_CLIENT_ID` env var overrides `WEBAPI_CLIENT_ID` — for
/// testing whether search 400s ("Invalid limit") are a Development-mode
/// catalog-access restriction on the `blueball` app rather than the request
/// shape: point this at a different registered client id without a rebuild.
/// Only the web-api id is overridable this way; `MUSIC_CLIENT_ID` (playback)
/// isn't in question here.
fn webapi_client_id() -> String {
    std::env::var("SPOTIFY_WEBAPI_CLIENT_ID").unwrap_or_else(|_| WEBAPI_CLIENT_ID.to_string())
}

/// Redirect URI to match whatever client id `webapi_client_id()` resolves
/// to — Spotify validates it byte-for-byte for the `blueball` app, so
/// swapping the client id alone isn't enough. Pass the full URI, port and
/// all (e.g. `http://127.0.0.1:8721/login`). No need to split the port out
/// ourselves: `librespot_oauth` parses it
/// straight back out of this string (`get_socket_address()`) to bind its
/// own local callback listener, so whatever port is embedded here is
/// exactly the one that ends up listening.
fn webapi_redirect_uri() -> String {
    std::env::var("SPOTIFY_WEBAPI_REDIRECT_URI").unwrap_or_else(|_| WEBAPI_REDIRECT_URI.to_string())
}

const MUSIC_SCOPES: &[&str] = &["streaming"];

const WEBAPI_SCOPES: &[&str] = &[
    "user-read-email",
    "user-read-private",
    "user-library-read",
    "user-read-playback-state",
    "user-modify-playback-state",
    "playlist-read-private",
    // Needed for `webapi.rs`'s add/remove-track calls — without these the
    // API 403s on every playlist, even ones the user owns.
    "playlist-modify-public",
    "playlist-modify-private",
    // Needed for `webapi.rs`'s save_track/remove_saved_track (Liked Songs,
    // `/v1/me/tracks`) — without this the like/unlike hotkey 403s.
    "user-library-modify",
];

/// 60 s slack so a token that is about to expire is treated as expired.
const EXPIRY_SLACK_SECS: i64 = 60;

fn scopes_match(tok: &CachedToken) -> bool {
    tok.scopes == WEBAPI_SCOPES.join(" ")
}

/// `addlogin`'s `client_id` argument: `"medley"`/`"ncspot"` are shorthands
/// for the two apps' ids, anything else is used verbatim, and omitted
/// defaults to ncspot's (medley's own app 403s some endpoints outright).
fn resolve_client_id(arg: Option<&str>) -> String {
    match arg.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("ncspot") => NCSPOT_CLIENT_ID.to_string(),
        Some("medley") => webapi_client_id(),
        Some(other) => other.to_string(),
    }
}

/// The OAuth client to mint/refresh a token with, for `client_id` —
/// medley's own app requires its fixed, exactly-registered
/// `WEBAPI_REDIRECT_URI`; any other client id (ncspot's, or a custom one
/// pasted via `addlogin`) uses loopback-any-port instead, like
/// `MUSIC_CLIENT_ID`.
fn oauth_client_for_id(client_id: &str) -> Result<librespot_oauth::OAuthClient, String> {
    if client_id == webapi_client_id() {
        webapi_oauth_client()
    } else {
        let redirect = format!("http://127.0.0.1:{}/login", free_port()?);
        OAuthClientBuilder::new(client_id, &redirect, WEBAPI_SCOPES.to_vec())
            .open_in_browser()
            .build()
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedToken {
    access_token: String,
    refresh_token: Option<String>,
    /// Unix seconds.
    expires_at: i64,
    /// Which app this token was issued for — what `refresh` re-mints against.
    #[serde(default)]
    client_id: String,
    /// `WEBAPI_SCOPES.join(" ")` at the time this token was issued.
    /// `#[serde(default)]` so a store from before this field existed
    /// deserializes to `""`, which never matches and is correctly discarded.
    #[serde(default)]
    scopes: String,
}

impl CachedToken {
    fn expiry_fresh(&self) -> bool {
        now_unix() + EXPIRY_SLACK_SECS < self.expires_at
    }

    fn from_oauth(t: &librespot_oauth::OAuthToken, client_id: String) -> Self {
        let remaining = t.expires_at.saturating_duration_since(Instant::now());
        Self {
            access_token: t.access_token.clone(),
            refresh_token: if t.refresh_token.is_empty() {
                None
            } else {
                Some(t.refresh_token.clone())
            },
            expires_at: now_unix() + remaining.as_secs() as i64,
            client_id,
            scopes: WEBAPI_SCOPES.join(" "),
        }
    }
}

fn account_is_usable(tok: &CachedToken) -> bool {
    scopes_match(tok) && tok.expiry_fresh()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every named Web API credential pair, plus which one is currently active —
/// `webapi_tokens.json`, replacing the single `token.json` from before
/// multiple pairs existed.
#[derive(Default, Serialize, Deserialize)]
struct TokenStore {
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    accounts: HashMap<String, CachedToken>,
}

fn token_store_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("webapi_tokens.json")
}

fn load_token_store(cache_dir: &Path) -> TokenStore {
    std::fs::read_to_string(token_store_path(cache_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_token_store(cache_dir: &Path, store: &TokenStore) {
    let path = token_store_path(cache_dir);
    match serde_json::to_string_pretty(store) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                log::warn!("spotify: cannot write {}: {e}", path.display());
                return;
            }
            chmod_600(&path);
        }
        Err(e) => log::warn!("spotify: cannot serialize web-api token store: {e}"),
    }
}

/// Best-effort `chmod 600`: a plain write inherits the process umask
/// (commonly 022 -> world-readable), and librespot's own credentials-file
/// write only applies its `0o600` open mode when the file doesn't already
/// exist, so an upgrade from an older build (or any other write path) can
/// leave a stale, wider mode in place forever without this.
#[cfg(unix)]
pub(crate) fn chmod_600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(crate) fn chmod_600(_path: &Path) {}

/// One-time startup self-heal for `credentials.json`/`webapi_tokens.json`
/// left world-readable by a pre-fix medley build. Cheap (two syscalls), so
/// fine to run once at plugin construction — never from `load_cached`, which
/// `probe` calls on every redraw.
pub fn heal_permissions(cache_dir: &Path) {
    chmod_600(&cache_dir.join("credentials.json"));
    chmod_600(&token_store_path(cache_dir));
}

/// The result of a successful login.
pub struct Auth {
    /// librespot session credentials (for playback), from `MUSIC_CLIENT_ID`.
    pub credentials: Credentials,
    /// Bearer token for Spotify Web API calls (search / resolve), from the
    /// active credential pair.
    pub access_token: String,
    /// `$XDG_DATA_HOME/medley/spotify/` — librespot [`Cache`] root.
    pub cache_dir: PathBuf,
}

impl Auth {
    /// A librespot [`Cache`] rooted at `cache_dir`, caching credentials, the
    /// last volume and downloaded audio files.
    pub fn cache(cache_dir: &Path) -> Result<Cache, String> {
        Cache::new(
            Some(cache_dir.to_path_buf()),
            Some(cache_dir.join("volume")),
            Some(cache_dir.join("files")),
            None,
        )
        .map_err(|e| format!("librespot cache: {e}"))
    }

    /// Non-blocking: local disk only, no network, no browser — the
    /// `core::Plugin::probe`/`wiring` path. `None` if either credential is
    /// missing, or the active Web API pair's token has expired (even though
    /// a network refresh might fix that without a full re-login — `login`
    /// below handles that; this fast path is deliberately strict so it
    /// stays startup-safe).
    pub fn load_cached(cache_dir: &Path) -> Option<Auth> {
        let cache = Self::cache(cache_dir).ok()?;
        let credentials = cache.credentials()?;
        let store = load_token_store(cache_dir);
        let name = store.active.clone().unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        let cached = store.accounts.get(&name)?;
        if !account_is_usable(cached) {
            return None;
        }
        Some(Auth {
            credentials,
            access_token: cached.access_token.clone(),
            cache_dir: cache_dir.to_path_buf(),
        })
    }

    /// Load cached credentials + the default Web API pair's token, running
    /// each OAuth browser flow (music, web-api — independently) only if
    /// something reusable is missing for it. Blocking (network, possibly a
    /// browser) — the `core::Plugin::setup` path; never called at startup.
    /// Only ever touches the `DEFAULT_ACCOUNT` pair (and makes it active
    /// again) — any other stored pair is untouched.
    pub fn login(cache_dir: &Path) -> Result<Auth, String> {
        std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
        let cache = Self::cache(cache_dir)?;
        let mut store = load_token_store(cache_dir);

        let mut credentials = cache.credentials();
        let expected = webapi_client_id();
        let mut access_token = store.accounts.get(DEFAULT_ACCOUNT).cloned().and_then(|t| {
            if t.client_id != expected {
                // Cached under a different app (e.g. before the music/web-api
                // split) — never reuse or refresh it, `expiry_fresh`/expiry
                // says nothing about which app it's for.
                log::info!(
                    "spotify: cached web-api token was issued for a different client id, discarding"
                );
                return None;
            }
            if t.scopes != WEBAPI_SCOPES.join(" ") {
                // Cached from before a scope was added (e.g.
                // `playlist-read-private`) — a refresh keeps the *original*
                // grant, it doesn't pick up new scopes, so this must go
                // through the full browser consent screen again, not just a
                // refresh.
                log::info!(
                    "spotify: cached web-api token is missing a now-required scope, discarding"
                );
                return None;
            }
            if t.expiry_fresh() {
                log::info!("spotify: using cached web-api token");
                Some(t.access_token)
            } else if let Some(rt) = &t.refresh_token {
                log::info!("spotify: web-api token expired, refreshing");
                refresh(rt, &expected)
                    .inspect_err(|e| log::warn!("spotify: web-api token refresh failed: {e}"))
                    .ok()
                    .inspect(|new| {
                        store.accounts.insert(DEFAULT_ACCOUNT.to_string(), new.clone());
                        save_token_store(cache_dir, &store);
                    })
                    .map(|new| new.access_token)
            } else {
                None
            }
        });

        if credentials.is_none() {
            log::info!("spotify: launching OAuth browser login (music)");
            let tok = run_music_oauth()?;
            let creds = Credentials::with_access_token(tok.access_token);
            // Otherwise nothing persists these until a later `Session::connect(_, true)`
            // — but `wiring()` needs `credentials.json` to already exist to get that far,
            // so a first-ever login could never bootstrap itself.
            cache.save_credentials(&creds);
            chmod_600(&cache_dir.join("credentials.json"));
            credentials = Some(creds);
        }

        if access_token.is_none() {
            log::info!("spotify: launching OAuth browser login (web api)");
            let tok = run_webapi_oauth()?;
            let cached = CachedToken::from_oauth(&tok, expected);
            access_token = Some(cached.access_token.clone());
            store.accounts.insert(DEFAULT_ACCOUNT.to_string(), cached);
        }
        store.active = Some(DEFAULT_ACCOUNT.to_string());
        save_token_store(cache_dir, &store);

        Ok(Auth {
            credentials: credentials.expect("credentials set above"),
            access_token: access_token.expect("token set above"),
            cache_dir: cache_dir.to_path_buf(),
        })
    }

    /// `_spotify addlogin [name] [client_id]`: OAuth-login a new (or
    /// re-authenticate an existing) Web API credential pair and store it
    /// under `name` — purely a storage key, trimmed, `DEFAULT_ACCOUNT` if
    /// omitted — alongside whatever's already stored. See
    /// `resolve_client_id` for `client_id`. Returns the name it was stored
    /// under.
    pub fn add_login(cache_dir: &Path, name: Option<&str>, client_id: Option<&str>) -> Result<String, String> {
        let name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_ACCOUNT)
            .to_string();
        let client_id = resolve_client_id(client_id);
        std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
        log::info!("spotify: opening browser for Spotify login — web API ({name}, client id {client_id})");
        let tok = oauth_client_for_id(&client_id)?.get_access_token().map_err(|e| e.to_string())?;
        let cached = CachedToken::from_oauth(&tok, client_id);
        let mut store = load_token_store(cache_dir);
        store.accounts.insert(name.clone(), cached);
        save_token_store(cache_dir, &store);
        Ok(name)
    }
}

/// Disk-only (no network): whether the active Web API pair has a stale
/// token but still has a refresh_token worth trying —
/// `SpotifyPlugin::probe`'s cue to kick off an automatic background
/// [`refresh_and_persist`] instead of reporting "needs reauth" for what's
/// actually just an expired access token.
pub fn has_refresh_token(cache_dir: &Path) -> bool {
    let Ok(cache) = Auth::cache(cache_dir) else { return false };
    if cache.credentials().is_none() {
        return false;
    }
    let store = load_token_store(cache_dir);
    let name = store.active.clone().unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
    let Some(cached) = store.accounts.get(&name) else {
        return false;
    };
    scopes_match(cached) && cached.refresh_token.is_some()
}

/// Refresh the *active* Web API pair's token from its refresh_token and
/// persist the result — the `WebApi::get` `401` path. `None` if there's no
/// cached refresh_token or the refresh itself fails (network, revoked
/// grant, ...); on `401` the caller then errors out (a `403`, if the token
/// itself is fine but the endpoint refuses it, goes through
/// `fallback_webapi_token` instead).
pub fn refresh_and_persist(cache_dir: &Path) -> Option<String> {
    let mut store = load_token_store(cache_dir);
    let name = store.active.clone().unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
    let cached = store.accounts.get(&name)?.clone();
    let rt = cached.refresh_token?;
    let new = refresh(&rt, &cached.client_id)
        .inspect_err(|e| log::warn!("spotify: web-api token refresh failed: {e}"))
        .ok()?;
    let access_token = new.access_token.clone();
    store.accounts.insert(name, new);
    save_token_store(cache_dir, &store);
    Some(access_token)
}

/// Try every stored Web API credential pair other than the currently active
/// one, refreshing as needed, and promote the first usable one to active —
/// `webapi.rs`'s `403` fallback: medley's own Development-mode client id can
/// be denied a specific endpoint outright (e.g. `/me/tracks` PUT, i.e.
/// Like), and a fresh/refreshed token under that *same* client id would
/// `403` again, so recovering needs a different credential pair, not just a
/// new token. Never touches (deletes/overwrites) the pair that just
/// 403'd, or any other pair it doesn't end up using — it may work again
/// later (e.g. after a rate-limit window passes). Returns the new access
/// token, or `None` if nothing else stored works either.
pub fn fallback_webapi_token(cache_dir: &Path) -> Option<String> {
    let mut store = load_token_store(cache_dir);
    let current = store.active.clone().unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
    let mut others: Vec<String> = store.accounts.keys().filter(|n| **n != current).cloned().collect();
    others.sort();
    for name in others {
        let Some(cached) = store.accounts.get(&name).cloned() else {
            continue;
        };
        let token = if account_is_usable(&cached) {
            Some(cached.access_token)
        } else {
            let client_id = cached.client_id.clone();
            cached.refresh_token.as_ref().and_then(|rt| refresh(rt, &client_id).ok()).map(|new| {
                let tok = new.access_token.clone();
                store.accounts.insert(name.clone(), new);
                tok
            })
        };
        if let Some(tok) = token {
            store.active = Some(name.clone());
            save_token_store(cache_dir, &store);
            log::info!("spotify: falling back to Web API credential pair {name:?} after a 403");
            return Some(tok);
        }
    }
    None
}

/// A free loopback port for `MUSIC_CLIENT_ID`'s (or `NCSPOT_CLIENT_ID`'s)
/// redirect — both match any port on `127.0.0.1`, so a dynamic one avoids
/// colliding with `WEBAPI_REDIRECT_URI`'s fixed port if flows ever run
/// close together (first-run login with both caches empty).
fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}

fn music_oauth_client() -> Result<librespot_oauth::OAuthClient, String> {
    let redirect = format!("http://127.0.0.1:{}/login", free_port()?);
    OAuthClientBuilder::new(MUSIC_CLIENT_ID, &redirect, MUSIC_SCOPES.to_vec())
        .open_in_browser()
        .build()
        .map_err(|e| e.to_string())
}

fn webapi_oauth_client() -> Result<librespot_oauth::OAuthClient, String> {
    OAuthClientBuilder::new(&webapi_client_id(), &webapi_redirect_uri(), WEBAPI_SCOPES.to_vec())
        .open_in_browser()
        .build()
        .map_err(|e| e.to_string())
}

fn run_music_oauth() -> Result<librespot_oauth::OAuthToken, String> {
    // log::info!, not eprintln! — cursive owns the terminal, a raw stderr
    // write here corrupts the TUI's painted cells.
    log::info!(
        "spotify: opening browser for Spotify login — music/playback (paste the URL below if it doesn't open)"
    );
    music_oauth_client()?.get_access_token().map_err(|e| e.to_string())
}

fn run_webapi_oauth() -> Result<librespot_oauth::OAuthToken, String> {
    log::info!(
        "spotify: opening browser for Spotify login — web API (paste the URL below if it doesn't open)"
    );
    webapi_oauth_client()?.get_access_token().map_err(|e| e.to_string())
}

fn refresh(refresh_token: &str, client_id: &str) -> Result<CachedToken, String> {
    let tok = oauth_client_for_id(client_id)?
        .refresh_token(refresh_token)
        .map_err(|e| e.to_string())?;
    let mut mapped = CachedToken::from_oauth(&tok, client_id.to_string());
    // Spotify may omit the refresh token on refresh — keep the old one.
    if mapped.refresh_token.is_none() {
        mapped.refresh_token = Some(refresh_token.to_string());
    }
    Ok(mapped)
}

