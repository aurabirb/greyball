//! Thin blocking wrapper over the Spotify Web API: track search/lookup and
//! playlist browsing, which is all `Source` needs. medley avoids the
//! `rspotify` dep and calls the REST endpoints directly with the OAuth
//! bearer token from [`crate::auth`]. A `401` triggers an in-place token
//! refresh (`crate::auth::refresh_and_persist`) and retry; a `403` (medley's
//! own Development-mode app refused outright, not just an expired token)
//! instead switches to another stored credential pair
//! (`crate::auth::fallback_webapi_token`) and retries under that.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{RateLimiter, RemotePage, Track};
use serde::Deserialize;

const API: &str = "https://api.spotify.com/v1";

/// Smallest gap enforced between two Web API requests, plus a random jitter
/// on top (below) — without the jitter, a burst of calls (e.g. paginating
/// Liked Songs) stays in lockstep and a shared cool-down releases them all on
/// the same tick, right back into the limit.
const API_MIN_INTERVAL: Duration = Duration::from_millis(200);
const API_JITTER_MAX_MS: u64 = 400;
/// Backoff for a `429` with no usable `Retry-After`, and the per-attempt step
/// added on repeated `429`s.
const API_BACKOFF_STEP: Duration = Duration::from_secs(3);
/// Ceiling on a `429`'s `Retry-After` — a Development-mode app that trips
/// its *daily* quota gets one of several hours, and `RateGate` is shared by
/// every caller, so honoring that literally would stall the whole source.
/// Capped: wait out at most this, then fail instead of hanging.
const API_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How many times a single call is retried through rate limiting before
/// giving up.
const API_MAX_ATTEMPTS: u32 = 6;

/// Paces every request through a shared [`RateLimiter`] and, once any
/// request gets a `429`, holds every *subsequent* request (this is the only
/// caller for Spotify's search + browse endpoints, but callers can arrive
/// from several threads at once — see `core::search`'s one-thread-per-source
/// fan-out) back until the shared cool-down expires.
struct RateGate {
    limiter: RateLimiter,
    cooldown_until: Mutex<Option<Instant>>,
}

impl Default for RateGate {
    fn default() -> Self {
        Self {
            limiter: RateLimiter::new(API_MIN_INTERVAL, Duration::from_millis(API_JITTER_MAX_MS)),
            cooldown_until: Mutex::new(None),
        }
    }
}

impl RateGate {
    fn wait_turn(&self) {
        loop {
            let until = *self.cooldown_until.lock().unwrap();
            match until {
                Some(t) => match t.checked_duration_since(Instant::now()) {
                    Some(remaining) => std::thread::sleep(remaining.min(Duration::from_secs(2))),
                    None => {
                        *self.cooldown_until.lock().unwrap() = None;
                        break;
                    }
                },
                None => break,
            }
        }
        self.limiter.throttle();
    }

    /// Make every caller back off for at least `wait`.
    fn set_cooldown(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut slot = self.cooldown_until.lock().unwrap();
        if slot.is_none_or(|current| current < until) {
            *slot = Some(until);
        }
    }
}

/// Cheap to `Clone` (an `Arc` around the shared state) — the background
/// Liked Songs walk (`SpotifySource`) holds its own clone so it keeps
/// pacing/backing off through the same [`RateGate`] as everything else
/// instead of getting a private one.
#[derive(Clone)]
pub struct WebApi {
    inner: Arc<Inner>,
}

struct Inner {
    token: Mutex<String>,
    cache_dir: PathBuf,
    http: reqwest::blocking::Client,
    gate: RateGate,
    /// Cached web-player session for `playlist_tracks_page`'s and
    /// `album_tracks_page`'s 403 fallback — see `web_player`. Lazily
    /// established on first use, reused across pages of the same (or a
    /// different) playlist/album walk.
    web_player: Mutex<Option<crate::web_player::Session>>,
}

#[derive(Deserialize)]
struct Artist {
    name: String,
}

#[derive(Deserialize)]
struct Album {
    name: String,
}

#[derive(Deserialize, Default)]
struct ExternalIds {
    isrc: Option<String>,
}

#[derive(Deserialize)]
struct ApiTrack {
    id: Option<String>,
    name: String,
    #[serde(default)]
    artists: Vec<Artist>,
    album: Option<Album>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    external_ids: ExternalIds,
}

#[derive(Deserialize)]
struct Tracks {
    items: Vec<ApiTrack>,
}

#[derive(Deserialize)]
struct SearchResponse {
    tracks: Tracks,
}

#[derive(Deserialize)]
struct ApiPlaylist {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Playlists {
    items: Vec<ApiPlaylist>,
    total: usize,
}

/// `/v1/me/tracks` (Liked Songs) item shape: `{ items: [{ track: {...} }] }`.
#[derive(Deserialize)]
struct SavedTrackItem {
    // Not known to be nullable here (unlike a playlist item's `item`), but
    // cheap insurance against a malformed/removed entry either way.
    track: Option<ApiTrack>,
}

#[derive(Deserialize)]
struct SavedTracks {
    items: Vec<SavedTrackItem>,
    total: usize,
}

/// `/v1/playlists/{id}/items` item shape: `{ items: [{ item: {...} }] }` — the
/// track fields sit directly under `item` (not `item.track`). Note this is
/// `/items`, not the older `/tracks`: see `WebApi::playlist_tracks`.
#[derive(Deserialize)]
struct PlaylistItem {
    // A playlist item's track can be null (a since-removed local/unavailable track).
    item: Option<ApiTrack>,
}

#[derive(Deserialize)]
struct PlaylistItems {
    items: Vec<PlaylistItem>,
    total: usize,
}

/// `/v1/albums/{id}/tracks` item shape: a *simplified* track object — no
/// `album` or `external_ids.isrc`, unlike the full objects every other list
/// endpoint here returns. Only `id` is needed: `WebApi::album_tracks_page`
/// looks the ids back up via the several-tracks endpoint to get full
/// metadata instead of settling for a degraded hit.
#[derive(Deserialize)]
struct SimplifiedTrack {
    id: Option<String>,
}

#[derive(Deserialize)]
struct AlbumTracks {
    items: Vec<SimplifiedTrack>,
    total: usize,
}

/// `/v1/tracks?ids=...` response — a null entry marks an id the lookup
/// couldn't resolve (shouldn't happen for ids the album endpoint just gave
/// us, but cheap insurance).
#[derive(Deserialize)]
struct SeveralTracks {
    tracks: Vec<Option<ApiTrack>>,
}

impl ApiTrack {
    fn into_track(self) -> Option<Track> {
        let id = self.id?;
        let artists = self.artists.into_iter().map(|a| a.name).collect();
        Some(crate::track(
            self.name,
            artists,
            self.external_ids.isrc,
            self.album.map(|a| a.name),
            format!("spotify:track:{id}"),
            self.duration_ms.min(u32::MAX as u64) as u32,
        ))
    }
}

impl WebApi {
    pub fn new(token: impl Into<String>, cache_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                token: Mutex::new(token.into()),
                cache_dir,
                http: reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()
                    .unwrap_or_default(),
                gate: RateGate::default(),
                web_player: Mutex::new(None),
            }),
        }
    }

    pub fn set_token(&self, token: String) {
        *self.inner.token.lock().unwrap() = token;
    }

    /// Paced through the shared [`RateGate`], and retried through `429`s (up
    /// to [`API_MAX_ATTEMPTS`]) — a `429` puts every caller into a cool-down
    /// for the `Retry-After` it asks for (or [`API_BACKOFF_STEP`], escalating
    /// per attempt, if it doesn't say), so paginating a big Liked Songs list
    /// or an eager search fan-out backs off together instead of hammering
    /// straight through the limit. `body`, if given, is sent as the request's
    /// JSON payload (`POST`/`DELETE`); `GET` never has one.
    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::blocking::Response, String> {
        // At most one credential-pair switch per call — otherwise two stored
        // pairs that both genuinely lack access to this endpoint (e.g. a
        // playlist neither account owns) would ping-pong between each other
        // for the rest of `API_MAX_ATTEMPTS` instead of failing once.
        let mut tried_credential_fallback = false;
        for attempt in 0..API_MAX_ATTEMPTS {
            self.inner.gate.wait_turn();

            log::debug!("spotify: {method} {url}");
            let token = self.inner.token.lock().unwrap().clone();
            let mut req = self.inner.http.request(method.clone(), url).bearer_auth(&token);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().map_err(|e| {
                log::warn!("spotify: {method} {url}: {e}");
                e.to_string()
            })?;
            let status = resp.status();
            if status.is_success() {
                log::debug!("spotify: {method} {url} -> {status}");
                return Ok(resp);
            }
            if status.as_u16() == 401 {
                log::info!("spotify: {method} {url} -> 401, refreshing token");
                match crate::auth::refresh_and_persist(&self.inner.cache_dir) {
                    Some(new) => {
                        *self.inner.token.lock().unwrap() = new;
                        continue;
                    }
                    None => return Err(format!("HTTP 401 from {url}: token refresh failed")),
                }
            }
            if status.as_u16() == 403 && !tried_credential_fallback {
                tried_credential_fallback = true;
                log::info!("spotify: {method} {url} -> 403, trying another stored credential pair");
                if let Some(new) = crate::auth::fallback_webapi_token(&self.inner.cache_dir) {
                    *self.inner.token.lock().unwrap() = new;
                    continue;
                }
                log::info!("spotify: {method} {url}: no other credential pair to fall back to");
            }
            if status.as_u16() == 429 {
                let raw = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs);
                let wait = raw
                    .unwrap_or(API_BACKOFF_STEP)
                    .max(API_BACKOFF_STEP * (attempt + 1))
                    .min(API_MAX_BACKOFF);
                log::warn!(
                    "spotify: rate limited on {url}, backing off {}s (attempt {}/{API_MAX_ATTEMPTS}){}",
                    wait.as_secs(),
                    attempt + 1,
                    match raw {
                        Some(r) if r > API_MAX_BACKOFF => format!(
                            " — server asked for {}s (likely a daily quota trip, not a transient limit); capped",
                            r.as_secs()
                        ),
                        _ => String::new(),
                    },
                );
                self.inner.gate.set_cooldown(wait);
                continue;
            }
            // Spotify error bodies are JSON with a message; log it, not just
            // the bare status.
            let body = resp.text().unwrap_or_default();
            let body_trimmed = body.trim();
            log::warn!("spotify: {method} {url} -> {status}: {body_trimmed}");
            return Err(format!("HTTP {} from {url}: {body_trimmed}", status.as_u16()));
        }
        Err(format!(
            "HTTP 429 from {url}: gave up after {API_MAX_ATTEMPTS} rate-limited attempts"
        ))
    }

    fn get(&self, url: &str) -> Result<reqwest::blocking::Response, String> {
        self.request(reqwest::Method::GET, url, None)
    }

    fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        self.get(url)?.json().map_err(|e| e.to_string())
    }

    fn post(&self, url: &str, body: &serde_json::Value) -> Result<(), String> {
        self.request(reqwest::Method::POST, url, Some(body))?;
        Ok(())
    }

    fn put(&self, url: &str, body: &serde_json::Value) -> Result<(), String> {
        self.request(reqwest::Method::PUT, url, Some(body))?;
        Ok(())
    }

    fn delete(&self, url: &str, body: &serde_json::Value) -> Result<(), String> {
        self.request(reqwest::Method::DELETE, url, Some(body))?;
        Ok(())
    }

    /// Track search. Two things caused the "Invalid limit" 400, found by
    /// curling `/v1/search` directly with this app's own bearer token:
    ///
    /// 1. `market=from_token` was deprecated in Spotify's Nov 2024 API
    ///    changes; sending it 400s with that exact misleading message
    ///    regardless of `limit`. Dropped — a user access token's account
    ///    country already takes priority over `market` per the docs, so
    ///    nothing is lost by omitting it.
    /// 2. Even with `market` gone, `limit` still 400s above 10 — confirmed
    ///    by curl bisection (1/5/10 → 200, 11/12/15/20/50 → 400) against
    ///    this Development-mode app (`WEBAPI_CLIENT_ID` in `auth.rs`),
    ///    which isn't approved for Spotify's Extended Quota Mode. The
    ///    publicly documented max of 50 only applies to quota-extended
    ///    apps; this one is silently capped lower. Clamped to the real,
    ///    empirically-confirmed ceiling instead of the documented one —
    ///    raise this back to 50 if/when the app is approved for Extended
    ///    Quota Mode on the Spotify dashboard.
    const MAX_LIMIT: usize = 10;

    pub fn search_tracks(&self, query: &str, limit: usize) -> Result<Vec<Track>, String> {
        let limit = limit.clamp(1, Self::MAX_LIMIT);
        let q = url_encode(query);
        let url = format!("{API}/search?type=track&limit={limit}&offset=0&q={q}");
        let body: SearchResponse = self.get(&url)?.json().map_err(|e| e.to_string())?;
        Ok(body
            .tracks
            .items
            .into_iter()
            .filter_map(ApiTrack::into_track)
            .collect())
    }

    /// Look up a single track by its base-62 id.
    pub fn track(&self, id: &str) -> Result<Track, String> {
        let url = format!("{API}/tracks/{id}");
        let body: ApiTrack = self.get(&url)?.json().map_err(|e| e.to_string())?;
        body.into_track().ok_or_else(|| "track has no id".to_string())
    }

    /// One page of the current user's playlists (owned + followed), as
    /// `(id, name)`, plus the API's reported `total` — the caller
    /// (`SpotifySource`'s `folders` `PagedList`) walks `offset` across
    /// repeated calls the same way `saved_tracks_page` does for Liked Songs.
    pub fn playlists_page(&self, offset: usize, limit: usize) -> Result<RemotePage<(String, String)>, String> {
        let url = format!("{API}/me/playlists?limit={limit}&offset={offset}");
        let body: Playlists = self.get(&url)?.json().map_err(|e| e.to_string())?;
        let consumed = body.items.len();
        Ok(RemotePage {
            total: body.total,
            consumed,
            hits: body.items.into_iter().map(|p| (p.id, p.name)).collect(),
        })
    }

    /// One page of a playlist's tracks, plus the API's reported `total` — the
    /// caller (`SpotifySource`'s per-playlist `PagedList`) walks `offset`
    /// across repeated calls to load the whole playlist in the background,
    /// the same way `saved_tracks_page` does for Liked Songs.
    ///
    /// Deliberately `/items`, not the older `/tracks`: confirmed against the
    /// live API that `/tracks` now 403s unconditionally (every playlist,
    /// including ones the caller owns) — Spotify has moved this under
    /// `/items` (the `href` `/v1/me/playlists` itself returns for a
    /// playlist's contents points there now, too). Even via `/items`, a
    /// Development-mode app (see `auth.rs`) still 403s on playlists it
    /// doesn't own (followed/other-user playlists) — confirmed live, and not
    /// fixable via this REST client; only Spotify approving the app for
    /// Extended Quota Mode lifts that here. On that specific 403 this falls
    /// back to `web_player`, a read-only client for the same web-player
    /// endpoint a browser itself uses to read a public playlist. Local/
    /// unavailable items (null `item`) are skipped, but still counted in
    /// `consumed` — see `RemotePage::consumed`.
    pub fn playlist_tracks_page(&self, id: &str, offset: usize, limit: usize) -> Result<RemotePage<Track>, String> {
        let url = format!("{API}/playlists/{id}/items?limit={limit}&offset={offset}");
        match self.get(&url) {
            Ok(resp) => {
                let body: PlaylistItems = resp.json().map_err(|e| e.to_string())?;
                let consumed = body.items.len();
                Ok(RemotePage {
                    total: body.total,
                    consumed,
                    hits: body
                        .items
                        .into_iter()
                        .filter_map(|item| item.item)
                        .filter_map(ApiTrack::into_track)
                        .collect(),
                })
            }
            // A 403 here means the account doesn't own/collaborate on this
            // playlist (Development Quota Mode — see this fn's doc comment),
            // not fixable by retrying the same request. Fall back to reading
            // it the way a browser does, via the web player's own API.
            Err(e) if e.starts_with("HTTP 403") => {
                log::info!("spotify: {url} -> 403, falling back to web-player read path");
                let mut session = self.inner.web_player.lock().unwrap();
                crate::web_player::fetch_playlist_page(&self.inner.http, &mut session, id, offset, limit).map_err(
                    |fallback_err| {
                        log::warn!("spotify: web-player fallback for playlist {id} failed: {fallback_err}");
                        fallback_err
                    },
                )
            }
            Err(e) => Err(e),
        }
    }

    /// One page of an album's tracks, plus the API's reported `total` — the
    /// caller (`SpotifySource`'s per-album `PagedList`) walks `offset` across
    /// repeated calls the same way `playlist_tracks_page` does. `/tracks`
    /// only returns simplified track objects (no album/ISRC), so this
    /// resolves the page's ids through `/v1/tracks` (max 50 ids, matching
    /// `ALBUM_PAGE_SIZE`) to get the same full metadata every other list here
    /// returns.
    ///
    /// Same Development Quota Mode gap as `playlist_tracks_page`: a `403` on
    /// an album this app isn't allowed to read falls back to `web_player`'s
    /// `getAlbum` read path instead of failing outright.
    pub fn album_tracks_page(&self, id: &str, offset: usize, limit: usize) -> Result<RemotePage<Track>, String> {
        let url = format!("{API}/albums/{id}/tracks?limit={limit}&offset={offset}");
        let page: AlbumTracks = match self.get(&url) {
            Ok(resp) => resp.json().map_err(|e| e.to_string())?,
            Err(e) if e.starts_with("HTTP 403") => {
                log::info!("spotify: {url} -> 403, falling back to web-player read path");
                let mut session = self.inner.web_player.lock().unwrap();
                return crate::web_player::fetch_album_page(&self.inner.http, &mut session, id, offset, limit)
                    .map_err(|fallback_err| {
                        log::warn!("spotify: web-player fallback for album {id} failed: {fallback_err}");
                        fallback_err
                    });
            }
            Err(e) => return Err(e),
        };
        let consumed = page.items.len();
        let ids: Vec<String> = page.items.into_iter().filter_map(|t| t.id).collect();
        let hits = if ids.is_empty() {
            Vec::new()
        } else {
            let url = format!("{API}/tracks?ids={}", ids.join(","));
            match self.get(&url) {
                Ok(resp) => {
                    let body: SeveralTracks = resp.json().map_err(|e| e.to_string())?;
                    body.tracks
                        .into_iter()
                        .flatten()
                        .filter_map(ApiTrack::into_track)
                        .collect()
                }
                // Same Development Quota Mode gap as the initial `/tracks`
                // call above, just tripped on the batch-lookup instead — the
                // web-player's `getAlbum` query already returns full track
                // metadata (duration/album included) directly, so it replaces
                // this whole page rather than just the missing ids.
                Err(e) if e.starts_with("HTTP 403") => {
                    log::info!("spotify: {url} -> 403, falling back to web-player read path");
                    let mut session = self.inner.web_player.lock().unwrap();
                    return crate::web_player::fetch_album_page(&self.inner.http, &mut session, id, offset, limit)
                        .map_err(|fallback_err| {
                            log::warn!("spotify: web-player fallback for album {id} failed: {fallback_err}");
                            fallback_err
                        });
                }
                Err(e) => return Err(e),
            }
        };
        Ok(RemotePage {
            total: page.total,
            consumed,
            hits,
        })
    }

    /// `POST /v1/playlists/{id}/tracks` — append `track_uri` (a
    /// `spotify:track:...` URI) to the end of the playlist.
    pub fn add_playlist_track(&self, playlist_id: &str, track_uri: &str) -> Result<(), String> {
        let url = format!("{API}/playlists/{playlist_id}/tracks");
        self.post(&url, &serde_json::json!({ "uris": [track_uri] }))
            .map_err(clarify_playlist_write_error)
    }

    /// `DELETE /v1/playlists/{id}/tracks` — remove every occurrence of
    /// `track_uri` from the playlist.
    pub fn remove_playlist_track(&self, playlist_id: &str, track_uri: &str) -> Result<(), String> {
        let url = format!("{API}/playlists/{playlist_id}/tracks");
        self.delete(&url, &serde_json::json!({ "tracks": [{ "uri": track_uri }] }))
            .map_err(clarify_playlist_write_error)
    }

    /// `DELETE /v1/playlists/{id}/tracks` with `positions` and the playlist's `snapshot_id`: removes
    /// only the row at `position`, and only if it still holds `track_uri`.
    pub fn remove_playlist_position(&self, playlist_id: &str, track_uri: &str, position: usize) -> Result<(), String> {
        let snapshot = self.get_json(&format!("{API}/playlists/{playlist_id}?fields=snapshot_id"))?;
        let snapshot = snapshot["snapshot_id"].as_str().ok_or("the playlist has no snapshot_id")?;
        let row = self.get_json(&format!("{API}/playlists/{playlist_id}/items?limit=1&offset={position}"))?;
        if row["items"][0]["item"]["uri"].as_str() != Some(track_uri) {
            return Err("the playlist changed: that row is no longer this track".to_string());
        }
        let url = format!("{API}/playlists/{playlist_id}/tracks");
        let body = serde_json::json!({ "tracks": [{ "uri": track_uri, "positions": [position] }], "snapshot_id": snapshot });
        self.delete(&url, &body).map_err(clarify_playlist_write_error)
    }

    /// One page of the current user's saved ("Liked Songs") tracks, plus the
    /// API's reported `total` — the caller (`SpotifySource`'s `PagedList`)
    /// walks `offset` across repeated calls to load the whole list in the
    /// background instead of blocking one call on the full walk.
    pub fn saved_tracks_page(&self, offset: usize, limit: usize) -> Result<RemotePage<Track>, String> {
        let url = format!("{API}/me/tracks?limit={limit}&offset={offset}");
        let body: SavedTracks = self.get(&url)?.json().map_err(|e| e.to_string())?;
        // `consumed` must be the raw item count, not `hits.len()` — a saved
        // track with no `track` payload (removed/local) or no id (`into_track`
        // needs one) is filtered out below, but the API still counted it
        // toward `offset`/`total`. Using the filtered count here would drift
        // every later page's offset by however many got dropped.
        let consumed = body.items.len();
        Ok(RemotePage {
            total: body.total,
            consumed,
            hits: body
                .items
                .into_iter()
                .filter_map(|item| item.track)
                .filter_map(ApiTrack::into_track)
                .collect(),
        })
    }

    /// `PUT /v1/me/tracks` — add `track_id` (a bare base-62 id, not a URI)
    /// to the current user's Liked Songs.
    pub fn save_track(&self, track_id: &str) -> Result<(), String> {
        let url = format!("{API}/me/tracks");
        self.put(&url, &serde_json::json!({ "ids": [track_id] }))
    }

    /// `DELETE /v1/me/tracks` — remove `track_id` from Liked Songs.
    pub fn remove_saved_track(&self, track_id: &str) -> Result<(), String> {
        let url = format!("{API}/me/tracks");
        self.delete(&url, &serde_json::json!({ "ids": [track_id] }))
    }
}

/// A 403 from the playlist add/remove-track endpoints means either the
/// playlist isn't owned by (or collaborative with) this account, or the
/// cached token predates the `playlist-modify-*` scopes — both surface as
/// the same opaque `{"error": {"status": 403, "message": "Forbidden"}}`
/// body, and there's no cheap way to tell them apart (would need an extra
/// `/me` + playlist-owner lookup), so give one message covering both.
fn clarify_playlist_write_error(e: String) -> String {
    if e.starts_with("HTTP 403") {
        "playlist isn't yours, or your login needs reauthorizing for playlist-modify access (403)"
            .to_string()
    } else {
        e
    }
}

/// Minimal percent-encoding for a query-string value (RFC 3986 unreserved kept).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
