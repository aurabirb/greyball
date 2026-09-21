//! The SoundCloud API client + `Source` / `MediaProvider` impls.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{
    Bus, BrowseNode, BrowsePage, CoreEvent, Error, Media, MediaProvider, NodeMeta, PagedList, PagedMap, Quality, RateGate,
    Rendition, RemotePage, Result, Track, SearchQuery, Source, SourceId,
};
use regex::Regex;
use serde::Deserialize;

use crate::uri::TrackRef;

const API: &str = "https://api-v2.soundcloud.com";
const WEB: &str = "https://soundcloud.com/";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// `BrowseNode::Path` sentinel for the synthetic "Liked Tracks" folder — not
/// a real SoundCloud id (those are numeric), so it can't collide with one.
const LIKED_TRACKS: &str = "__liked__";

const READ_ONLY: NodeMeta = NodeMeta { writable: Some(false) };

/// `BrowseNode::Path` prefix of a personalized playlist; the rest is its urn.
const SYSTEM_PREFIX: &str = "system:";

/// Page size for walking a user's `track_likes`.
const LIKED_PAGE_SIZE: usize = 50;

const PLAYLISTS_PAGE_SIZE: usize = 200;

const SHELVES_PAGE_SIZE: usize = 20;

/// Tries per request across rate limits, 5xx, network errors and one client_id re-scrape.
const MAX_ATTEMPTS: u32 = 4;
const BACKOFF_STEP: Duration = Duration::from_secs(1);
/// Ceiling on any one wait, so a `Retry-After` of hours fails the call instead of stalling the source.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long a failed client_id scrape is remembered: briefly for transient errors, longer when the site refuses or holds no id.
const SCRAPE_RETRY_TRANSIENT: Duration = Duration::from_secs(10);
const SCRAPE_RETRY_REFUSED: Duration = Duration::from_secs(60);

/// Shelves walked on the home feed before stopping.
const SHELF_CAP: usize = 60;

/// Playlist tracks hydrated per page — `/tracks?ids=` takes at most 50.
const PLAYLIST_PAGE_SIZE: usize = 50;

/// What a 401/403 (or 404) from an endpoint means.
#[derive(Clone, Copy)]
enum Denied {
    /// A stale scraped client_id (without a token) or an expired login: an error.
    StaleClientId,
    /// Not available to this account: no error, just no data.
    Unavailable,
    /// A write: any rejection is an error carrying the HTTP status.
    Write,
}

enum Outcome {
    Success,
    Denied,
    Failed,
}

fn classify_status(status: reqwest::StatusCode, denied: Denied) -> Outcome {
    match (status.as_u16(), denied) {
        _ if status.is_success() => Outcome::Success,
        (401 | 403, _) | (404, Denied::Unavailable) => Outcome::Denied,
        _ => Outcome::Failed,
    }
}

/// A `PagedList` plus the `next_href` of its next page, kept across walk restarts.
#[derive(Clone)]
struct CursorList<T> {
    list: PagedList<T>,
    next: Arc<Mutex<Option<String>>>,
}

impl<T: Clone + Send + Sync + 'static> CursorList<T> {
    fn new(label: &'static str) -> Self {
        Self { list: PagedList::new(label), next: Arc::default() }
    }

    fn snapshot(&self, src: &SoundcloudSource, want: usize, page: fn(&SoundcloudSource, usize) -> std::result::Result<RemotePage<T>, String>) -> (Vec<T>, bool) {
        let src2 = src.clone();
        self.list.snapshot(&src.bus, want, move |offset| page(&src2, offset))
    }
}

#[derive(Default)]
struct ClientIdCache {
    id: Option<String>,
    failure: Option<(Instant, Duration, String)>,
}

/// A playlist object fetched once; its pages are hydrated from `items`.
struct PlaylistDoc {
    title: String,
    items: Vec<serde_json::Value>,
}

pub(crate) fn source_id() -> SourceId {
    SourceId::from("soundcloud")
}

fn src_err(message: impl Into<String>) -> Error {
    Error::Source {
        src: source_id(),
        message: message.into(),
    }
}

/// Cheap to `Clone` (`Arc`/`Client`/`PagedList` all are) — needed so
/// `browse` can hand `PagedList::snapshot` a `'static` closure that can
/// still make authenticated API calls.
#[derive(Clone)]
pub struct SoundcloudSource {
    client: reqwest::blocking::Client,
    /// From `[soundcloud] client_id`; skips the scrape when present.
    configured_id: Option<String>,
    /// Resolved id (configured or scraped), cached after first success.
    cached_id: Arc<Mutex<ClientIdCache>>,
    /// A user OAuth token. Needed only for the account's endpoints (Liked
    /// Tracks, playlists); search/resolve/play work without it.
    oauth_token: Option<String>,
    /// Set while SoundCloud rejects `oauth_token` (a token-less 401 is a stale client_id instead).
    login_expired: Arc<AtomicBool>,
    user_id: Arc<Mutex<Option<u64>>>,
    bus: Bus,
    liked: Arc<Mutex<CursorList<Track>>>,
    playlists: CursorList<(String, BrowseNode, NodeMeta)>,
    /// Playlists on the home feed's shelves.
    shelves: CursorList<(String, BrowseNode)>,
    shelf_seen: Arc<Mutex<HashSet<String>>>,
    /// Playlist contents by node id (numeric or `system:<urn>`).
    playlist_tracks: PagedMap<Track>,
    playlist_docs: Arc<Mutex<HashMap<String, Arc<PlaylistDoc>>>>,
    /// From `[soundcloud] hls` — prefer a higher-bitrate HLS stream over
    /// the 128kbps progressive one when the track offers one.
    hls: bool,
    /// Paces every request; a 429 or 5xx cools all callers down together.
    gate: Arc<RateGate>,
}

impl SoundcloudSource {
    pub fn new(configured_id: Option<String>, oauth_token: Option<String>, bus: Bus, hls: bool) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(UA)
            .build()
            .unwrap_or_default();
        Self {
            client,
            configured_id: configured_id.filter(|s| !s.trim().is_empty()),
            cached_id: Arc::default(),
            oauth_token: oauth_token.filter(|s| !s.trim().is_empty()),
            login_expired: Arc::default(),
            user_id: Arc::default(),
            bus,
            liked: Arc::new(Mutex::new(CursorList::new("soundcloud: liked tracks"))),
            playlists: CursorList::new("soundcloud: playlists"),
            shelves: CursorList::new("soundcloud: shelf playlists"),
            shelf_seen: Arc::default(),
            playlist_tracks: PagedMap::new("soundcloud: playlist"),
            playlist_docs: Arc::default(),
            hls,
            gate: Arc::new(RateGate::new(Duration::from_millis(200), Duration::from_millis(100))),
        }
    }

    /// A fresh source for `token` on a detached bus, so validating it raises no events.
    pub(crate) fn with_token(&self, token: String) -> Self {
        Self::new(self.configured_id.clone(), Some(token), Bus::new(), self.hls)
    }

    pub(crate) fn with_bus(mut self, bus: Bus) -> Self {
        self.bus = bus;
        self
    }

    pub(crate) fn has_token(&self) -> bool {
        self.oauth_token.is_some()
    }

    /// The `client_id` query param every endpoint needs: configured, else scraped and cached.
    /// The lock is held through the scrape (single-flight); a failed scrape is replayed for its window.
    fn client_id(&self) -> Result<String> {
        let mut cached = self.cached_id.lock().unwrap();
        if let Some(id) = &cached.id {
            return Ok(id.clone());
        }
        if let Some((at, window, message)) = &cached.failure
            && at.elapsed() < *window
        {
            return Err(src_err(message.clone()));
        }
        let id = match &self.configured_id {
            Some(id) => id.clone(),
            None => match self.scrape_client_id() {
                Ok(id) => id,
                Err((window, message)) => {
                    cached.failure = Some((Instant::now(), window, message.clone()));
                    return Err(src_err(message));
                }
            },
        };
        cached.failure = None;
        cached.id = Some(id.clone());
        log::info!("soundcloud: using client_id {}…", &id[..id.len().min(6)]);
        Ok(id)
    }

    /// Drops `used` from the cache unless another thread already replaced it.
    fn reset_client_id(&self, used: &str) {
        let mut cached = self.cached_id.lock().unwrap();
        if cached.id.as_deref() == Some(used) {
            cached.id = None;
        }
    }

    pub(crate) fn login_expired(&self) -> bool {
        self.login_expired.load(Ordering::SeqCst)
    }

    fn set_login_expired(&self, expired: bool) {
        if self.login_expired.swap(expired, Ordering::SeqCst) != expired {
            self.bus.send(CoreEvent::PluginStatusChanged);
        }
    }

    /// The scraped client_id, or how long to remember the failure and its message.
    fn scrape_client_id(&self) -> std::result::Result<String, (Duration, String)> {
        let transient = |e: reqwest::Error| (SCRAPE_RETRY_TRANSIENT, format!("web player unreachable: {}", e.without_url()));
        log::debug!("soundcloud: GET {WEB} (scraping client_id)");
        self.gate.wait_turn();
        let resp = self.client.get(WEB).send().map_err(transient)?;
        let status = resp.status();
        log::debug!("soundcloud: GET {WEB} -> {status}");
        if status.as_u16() == 403 {
            return Err((SCRAPE_RETRY_REFUSED, format!("web player refused the request (HTTP {status}); set [soundcloud] client_id in config")));
        }
        let home = resp.error_for_status().map_err(transient)?.text().map_err(transient)?;

        let script_re = Regex::new(r#"src="(https://a-v2\.sndcdn\.com/assets/[^"]+\.js)""#).unwrap();
        let id_re = Regex::new(r#"client_id\s*[:=]\s*"([0-9A-Za-z-]{16,})""#).unwrap();

        // The client_id lives in one of the last bundles; scan newest-first.
        let mut scripts: Vec<&str> = script_re
            .captures_iter(&home)
            .map(|c| c.get(1).unwrap().as_str())
            .collect();
        scripts.reverse();
        scripts.dedup();
        log::debug!("soundcloud: {} script bundle(s) to scan for client_id", scripts.len());

        let mut last_error = None;
        for url in scripts {
            log::debug!("soundcloud: GET {url} (scanning for client_id)");
            self.gate.wait_turn();
            let attempt = self.client.get(url).send().and_then(|r| {
                log::debug!("soundcloud: GET {url} -> {}", r.status());
                r.error_for_status().and_then(|r| r.text())
            });
            match attempt {
                Ok(body) => {
                    if let Some(c) = id_re.captures(&body) {
                        return Ok(c.get(1).unwrap().as_str().to_string());
                    }
                }
                Err(e) => {
                    let e = e.without_url().to_string();
                    log::debug!("soundcloud: {url}: {e}");
                    last_error = Some(e);
                }
            }
        }
        Err(match last_error {
            Some(e) => (SCRAPE_RETRY_TRANSIENT, format!("web player bundles failed to load, last: {e}")),
            None => (SCRAPE_RETRY_REFUSED, "could not scrape a client_id; set [soundcloud] client_id in config".to_string()),
        })
    }

    /// The oauth token, or an error naming the config key — for endpoints
    /// (Liked Tracks, playlists) that need a logged-in user and have no
    /// meaningful fallback.
    fn require_auth(&self) -> Result<&str> {
        self.oauth_token
            .as_deref()
            .ok_or_else(|| src_err("needs a SoundCloud login — log in from the SoundCloud setup dialog in Settings"))
    }

    /// The logged-in user's document, or `None` if SoundCloud rejects the token.
    fn me_doc(&self) -> Result<Option<serde_json::Value>> {
        self.require_auth()?;
        self.send_while("/me", &[], Denied::StaleClientId, &|| true)
    }

    /// The logged-in username, or `None` if SoundCloud rejects the token.
    pub(crate) fn me(&self) -> Result<Option<String>> {
        let Some(json) = self.me_doc()? else {
            return Ok(None);
        };
        let name = ["username", "permalink"].iter().find_map(|k| json.get(k)?.as_str()).unwrap_or("your account");
        Ok(Some(name.to_string()))
    }

    /// The logged-in user's id, which per-user endpoints (likes) are keyed by.
    fn user_id(&self) -> Result<u64> {
        if let Some(id) = *self.user_id.lock().unwrap() {
            return Ok(id);
        }
        let me = self.me_doc()?.ok_or_else(|| self.denied_error("/me", Denied::StaleClientId))?;
        let id = me.get("id").and_then(|id| id.as_u64()).ok_or_else(|| src_err("no user id in /me"))?;
        *self.user_id.lock().unwrap() = Some(id);
        Ok(id)
    }

    fn api_get(&self, path: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
        self.api_get_with(path, query, Denied::StaleClientId, &|| true)
    }

    /// `Error::NotFound` when an `Unavailable` endpoint denies this account; stops early once `wanted` turns false.
    fn api_get_with(&self, target: &str, query: &[(&str, &str)], denied: Denied, wanted: &dyn Fn() -> bool) -> Result<serde_json::Value> {
        self.send_while(target, query, denied, wanted)?.ok_or_else(|| self.denied_error(target, denied))
    }

    fn denied_error(&self, target: &str, denied: Denied) -> Error {
        match denied {
            _ if self.login_expired() => src_err("SoundCloud login expired — set up again in Settings"),
            Denied::Unavailable => Error::NotFound,
            Denied::Write | Denied::StaleClientId => src_err(format!("{}: denied (client_id or token rejected)", target.split('?').next().unwrap_or(target))),
        }
    }

    fn send_while(&self, target: &str, query: &[(&str, &str)], denied: Denied, wanted: &dyn Fn() -> bool) -> Result<Option<serde_json::Value>> {
        self.send_request(reqwest::Method::GET, target, query, None, denied, wanted)
    }

    /// PUT/DELETE `target` with an optional JSON body, ignoring any response body.
    fn api_write(&self, method: reqwest::Method, target: &str, body: Option<&serde_json::Value>) -> Result<()> {
        self.require_auth()?;
        self.send_request(method, target, &[], body, Denied::Write, &|| true)?.ok_or_else(|| self.denied_error(target, Denied::Write))?;
        Ok(())
    }

    /// `method` on `target`; `None` when denied. Retried with backoff (callers are idempotent); a non-GET empty body is `Null`.
    fn send_request(
        &self,
        method: reqwest::Method,
        target: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
        denied: Denied,
        wanted: &dyn Fn() -> bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rescraped = false;
        for attempt in 0..MAX_ATTEMPTS {
            let client_id = self.client_id()?;
            let url = request_url(target, query, &client_id)?;
            let path = url.path().to_string();
            log::debug!("soundcloud: {method} {path} {query:?}");
            if !self.gate.wait_turn_while(wanted) {
                return Err(src_err("no longer wanted"));
            }
            let mut req = self.client.request(method.clone(), url);
            if let Some(body) = body {
                req = req.json(body);
            }
            if let Some(token) = &self.oauth_token {
                req = req.header("Authorization", format!("OAuth {token}"));
            }
            let last = attempt + 1 == MAX_ATTEMPTS;
            let backoff = BACKOFF_STEP * 2u32.pow(attempt);
            let resp = match req.send() {
                Ok(resp) => resp,
                Err(e) if (e.is_connect() || e.is_timeout()) && !last => {
                    log::warn!("soundcloud: {method} {path}: {}, retrying in {backoff:?}", e.without_url());
                    self.gate.set_cooldown(backoff);
                    continue;
                }
                Err(e) => return Err(src_err(format!("{path}: {}", e.without_url()))),
            };
            let status = resp.status();
            if status.is_success() {
                log::debug!("soundcloud: {method} {path} -> {status}");
            } else {
                log::warn!("soundcloud: {method} {path} -> {status}");
            }
            if status.as_u16() == 429 || status.is_server_error() {
                if last {
                    return Err(src_err(format!("{path}: HTTP {status}, gave up after {MAX_ATTEMPTS} attempts")));
                }
                let asked = resp.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).map(Duration::from_secs);
                let wait = asked.filter(|_| status.as_u16() == 429).unwrap_or(backoff).min(MAX_BACKOFF);
                log::warn!("soundcloud: backing off {wait:?} (attempt {}/{MAX_ATTEMPTS})", attempt + 1);
                self.gate.set_cooldown(wait);
                continue;
            }
            match classify_status(status, denied) {
                Outcome::Success => {
                    if self.oauth_token.is_some() {
                        self.set_login_expired(false);
                    }
                    let text = resp.text().map_err(|e| src_err(format!("{path}: {}", e.without_url())))?;
                    return match serde_json::from_str(&text) {
                        Ok(v) => Ok(Some(v)),
                        Err(_) if method != reqwest::Method::GET => Ok(Some(serde_json::Value::Null)),
                        Err(e) => Err(src_err(format!("{path}: bad json: {e}"))),
                    };
                }
                Outcome::Denied if matches!(denied, Denied::Write) => {
                    if self.oauth_token.is_some() && status.as_u16() == 401 {
                        self.set_login_expired(true);
                        return Ok(None);
                    }
                    let body: String = resp.text().unwrap_or_default().chars().take(200).collect();
                    return Err(src_err(format!("{path}: HTTP {status} {body}")));
                }
                // The token authenticates on its own: with one, a 401 on any endpoint means the login expired.
                Outcome::Denied if self.oauth_token.is_some() => {
                    if status.as_u16() == 401 {
                        self.set_login_expired(true);
                    }
                    return Ok(None);
                }
                Outcome::Denied if matches!(denied, Denied::StaleClientId) && self.configured_id.is_none() && !rescraped => {
                    rescraped = true;
                    self.reset_client_id(&client_id);
                    log::warn!("soundcloud: client_id rejected ({status}), re-scraping");
                }
                Outcome::Denied => return Ok(None),
                Outcome::Failed => {
                    let body: String = if matches!(denied, Denied::Write) { resp.text().unwrap_or_default().chars().take(200).collect() } else { String::new() };
                    return Err(src_err(format!("{path}: HTTP {status} {body}").trim_end().to_string()));
                }
            }
        }
        Ok(None)
    }

    fn track_by_id(&self, id: u64, wanted: &dyn Fn() -> bool) -> Result<ApiTrack> {
        let v = self.api_get_with(&format!("/tracks/{id}"), &[], Denied::StaleClientId, wanted)?;
        serde_json::from_value(v).map_err(|e| src_err(format!("track {id}: {e}")))
    }

    /// The samples of a SoundCloud-drawn waveform, if the JSON at `url` has any.
    pub(crate) fn waveform_samples(&self, url: &str) -> Result<Option<Vec<f32>>> {
        self.gate.wait_turn();
        let json: serde_json::Value = self
            .client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json())
            .map_err(|e| src_err(format!("waveform {url}: {e}")))?;
        let samples = json.get("samples").and_then(|s| s.as_array());
        Ok(samples.map(|a| a.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect()))
    }

    fn resolve_permalink(&self, url: &str, wanted: &dyn Fn() -> bool) -> Result<ApiTrack> {
        let v = self.api_get_with("/resolve", &[("url", url)], Denied::StaleClientId, wanted)?;
        if v.get("kind").and_then(|k| k.as_str()) != Some("track") {
            return Err(src_err("that SoundCloud URL is not a track"));
        }
        serde_json::from_value(v).map_err(|e| src_err(format!("resolve {url}: {e}")))
    }

    fn track_ref(&self, r: &TrackRef, wanted: &dyn Fn() -> bool) -> Result<ApiTrack> {
        match r {
            TrackRef::Id(id) => self.track_by_id(*id, wanted),
            TrackRef::Permalink(url) => self.resolve_permalink(url, wanted),
        }
    }

    /// The `collection` array of an api-v2 list endpoint and its `next_href`; empty (and logged) if the response has none.
    fn collection_page(&self, target: &str, query: &[(&str, &str)], denied: Denied) -> Result<(Vec<serde_json::Value>, Option<String>)> {
        self.require_auth()?;
        let mut v = self.api_get_with(target, query, denied, &|| true)?;
        let next = v.get("next_href").and_then(|n| n.as_str()).filter(|n| !n.is_empty()).map(str::to_string);
        match v.get_mut("collection").map(serde_json::Value::take) {
            Some(serde_json::Value::Array(items)) => Ok((items, next)),
            _ => {
                let keys: Vec<&String> = v.as_object().map(|o| o.keys().collect()).unwrap_or_default();
                log::warn!("soundcloud: {target}: no 'collection' array, top-level keys {keys:?}");
                Ok((vec![], None))
            }
        }
    }

    /// One page of a cursor-paged list: `offset` 0 starts at `path`, later offsets follow the remembered `next_href`.
    fn list_page<T>(
        &self,
        list: &CursorList<T>,
        offset: usize,
        (path, size, denied): (&str, usize, Denied),
        cap: usize,
        map: impl FnOnce(Vec<serde_json::Value>) -> Vec<T>,
    ) -> Result<RemotePage<T>> {
        let size = size.to_string();
        let (target, query) = if offset == 0 {
            (Some(path.to_string()), &[("limit", size.as_str())][..])
        } else {
            (list.next.lock().unwrap().clone(), &[][..])
        };
        let Some(target) = target else {
            return Ok(remote_page(vec![], offset, 0, false));
        };
        let (items, next) = self.collection_page(&target, query, denied)?;
        let (consumed, more) = (items.len(), next.is_some());
        *list.next.lock().unwrap() = next;
        Ok(remote_page(map(items), offset, consumed, more && offset + consumed < cap))
    }

    /// One page of the library's playlists: the user's own and the ones they liked, without albums.
    fn playlists_page(&self, offset: usize) -> std::result::Result<RemotePage<(String, BrowseNode, NodeMeta)>, String> {
        let me = self.user_id().map_err(|e| e.to_string())?;
        self.list_page(&self.playlists, offset, ("/me/library/all", PLAYLISTS_PAGE_SIZE, Denied::StaleClientId), usize::MAX, |items| {
            items
                .into_iter()
                .filter_map(|item| serde_json::from_value::<ApiLibraryItem>(item).ok()?.playlist)
                .filter(|p| p.is_album != Some(true))
                .map(|p| {
                    let writable = p.user.is_some_and(|u| u.id == me);
                    (p.title, BrowseNode::Path(p.id.to_string()), NodeMeta { writable: Some(writable) })
                })
                .collect()
        })
        .map_err(|e| e.to_string())
    }

    /// One page of `/mixed-selections` shelves, as the playlists on them; not available to every account.
    fn shelves_page(&self, offset: usize) -> std::result::Result<RemotePage<(String, BrowseNode)>, String> {
        let page = self.list_page(&self.shelves, offset, ("/mixed-selections", SHELVES_PAGE_SIZE, Denied::Unavailable), SHELF_CAP, |shelves| {
            let mut seen = self.shelf_seen.lock().unwrap();
            if offset == 0 {
                seen.clear();
            }
            shelves
                .iter()
                .filter_map(|s| s.get("items")?.get("collection")?.as_array())
                .flatten()
                .filter_map(shelf_playlist_id)
                .filter(|(id, _)| seen.insert(id.clone()))
                .map(|(id, title)| (title, BrowseNode::Path(id)))
                .collect()
        });
        match page {
            Err(Error::NotFound) => {
                log::warn!("soundcloud: recommendations not available for this account");
                Ok(remote_page(vec![], offset, 0, false))
            }
            page => page.map_err(|e| e.to_string()),
        }
    }

    fn fetch_playlist_doc(&self, id: &str) -> Result<PlaylistDoc> {
        let (path, denied) = match id.strip_prefix(SYSTEM_PREFIX) {
            Some(urn) => (format!("/system-playlists/{urn}"), Denied::Unavailable),
            None => (format!("/playlists/{id}"), Denied::StaleClientId),
        };
        let v = self.api_get_with(&path, &[], denied, &|| true)?;
        Ok(PlaylistDoc {
            title: v.get("title").and_then(|t| t.as_str()).unwrap_or(id).to_string(),
            items: v.get("tracks").and_then(|t| t.as_array()).cloned().unwrap_or_default(),
        })
    }

    /// The playlist object for `id`, fetched at the list's start and reused for later pages.
    fn playlist_doc(&self, id: &str, offset: usize) -> Result<Arc<PlaylistDoc>> {
        if offset > 0
            && let Some(doc) = self.playlist_docs.lock().unwrap().get(id)
        {
            return Ok(doc.clone());
        }
        let doc = Arc::new(self.fetch_playlist_doc(id)?);
        self.playlist_docs.lock().unwrap().insert(id.to_string(), doc.clone());
        Ok(doc)
    }

    fn playlist_page(&self, id: &str, offset: usize) -> std::result::Result<RemotePage<Track>, String> {
        let doc = self.playlist_doc(id, offset).map_err(|e| e.to_string())?;
        let slice: Vec<_> = doc.items.iter().skip(offset).take(PLAYLIST_PAGE_SIZE).cloned().collect();
        let hits = self.hydrate_tracks(&slice).map_err(|e| e.to_string())?.into_iter().filter_map(ApiTrack::into_track).collect();
        Ok(RemotePage { hits, total: doc.items.len(), consumed: slice.len() })
    }

    /// Full tracks in order; id-only stubs are fetched via `/tracks?ids=`, and unavailable ones dropped.
    fn hydrate_tracks(&self, items: &[serde_json::Value]) -> Result<Vec<ApiTrack>> {
        let total = items.len();
        let mut slots: Vec<std::result::Result<ApiTrack, u64>> = vec![];
        for item in items {
            match ApiTrack::deserialize(item) {
                Ok(t) => slots.push(Ok(t)),
                Err(_) => slots.extend(item.get("id").and_then(|i| i.as_u64()).map(Err)),
            }
        }
        let mut stubs: Vec<u64> = slots.iter().filter_map(|s| s.as_ref().err().copied()).collect();
        stubs.sort_unstable();
        stubs.dedup();
        let mut hydrated = HashMap::new();
        for chunk in stubs.chunks(50) {
            let ids = chunk.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
            let v = self.api_get("/tracks", &[("ids", &ids)])?;
            for t in v.as_array().into_iter().flatten() {
                if let Ok(t) = ApiTrack::deserialize(t) {
                    hydrated.insert(t.id, t);
                }
            }
        }
        let out: Vec<ApiTrack> = slots
            .into_iter()
            .filter_map(|s| s.or_else(|id| hydrated.get(&id).cloned().ok_or(())).ok())
            .collect();
        if out.len() < total {
            log::debug!("soundcloud: {} of {total} playlist tracks unavailable", total - out.len());
        }
        Ok(out)
    }

    fn track_id(uri: &str) -> Result<u64> {
        match TrackRef::parse(uri) {
            Some(TrackRef::Id(id)) => Ok(id),
            _ => Err(src_err(format!("not a SoundCloud track id: {uri}"))),
        }
    }

    fn set_like(&self, uri: &str, like: bool) -> Result<()> {
        let target = format!("/users/{}/track_likes/{}", self.user_id()?, Self::track_id(uri)?);
        self.api_write(if like { reqwest::Method::PUT } else { reqwest::Method::DELETE }, &target, None)?;
        *self.liked.lock().unwrap() = CursorList::new("soundcloud: liked tracks");
        Ok(())
    }

    fn liked(&self) -> CursorList<Track> {
        self.liked.lock().unwrap().clone()
    }

    /// Rewrites playlist `id`'s full track list through `edit`; cached listings are dropped afterwards.
    fn edit_playlist(&self, id: &str, edit: impl FnOnce(&mut Vec<u64>) -> Result<()>) -> Result<()> {
        if id.starts_with(SYSTEM_PREFIX) {
            return Err(Error::Unsupported("playlist writes: personalized playlists are read-only"));
        }
        let mut ids: Vec<u64> = self.fetch_playlist_doc(id)?.items.iter().filter_map(|t| t.get("id")?.as_u64()).collect();
        edit(&mut ids)?;
        let tracks: Vec<_> = ids.iter().map(|id| serde_json::json!({ "id": id })).collect();
        let result = self.api_write(reqwest::Method::PUT, &format!("/playlists/{id}"), Some(&serde_json::json!({ "playlist": { "tracks": tracks } })));
        self.forget_playlist(&BrowseNode::Path(id.to_string()));
        result
    }

    /// One page of the user's `track_likes`.
    fn likes_page(&self, offset: usize) -> std::result::Result<RemotePage<Track>, String> {
        let path = format!("/users/{}/track_likes", self.user_id().map_err(|e| e.to_string())?);
        self.list_page(&self.liked(), offset, (&path, LIKED_PAGE_SIZE, Denied::StaleClientId), usize::MAX, |items| {
            items
                .into_iter()
                .filter_map(|item| serde_json::from_value::<ApiLike>(item).ok())
                .filter_map(|l| l.track)
                .filter_map(ApiTrack::into_track)
                .collect()
        })
        .map_err(|e| e.to_string())
    }

    /// Best-quality HLS path: resolves the AAC-160k transcoding's signed playlist here, then returns a
    /// stream that appends the init segment and every media segment in order (one fMP4 `symphonia`
    /// decodes like any other). `None` when the track has no HLS transcoding or its playlist is unusable;
    /// any error also means `open` falls back to progressive, unless it was cancelled.
    fn open_hls(&self, track: &ApiTrack, wanted: &dyn Fn() -> bool) -> Result<Option<Media>> {
        let Some(hls) = track
            .media
            .full_transcodings()
            .find(|t| t.format.protocol == "hls" && t.format.mime_type.starts_with("audio/mp4"))
        else {
            return Ok(None);
        };

        let v = self.api_get_with(&hls.url, &[], Denied::StaleClientId, wanted)?;
        let Some(playlist_url) = v.get("url").and_then(|u| u.as_str()) else {
            log::debug!("soundcloud: hls stream url: no 'url' in response");
            return Ok(None);
        };
        let playlist = match self.hls_playlist(playlist_url, hls.preset.as_str()) {
            Ok(p) => p,
            Err(e) => {
                log::debug!("soundcloud: HLS path unavailable, falling back to progressive: {e}");
                return Ok(None);
            }
        };

        log::info!("soundcloud: playing via HLS (preset {}) instead of 128kbps progressive", hls.preset);
        let client = self.client.clone();
        let urls: Vec<url::Url> = playlist.init.into_iter().chain(playlist.segments).collect();
        Ok(Some(Media::Stream(Box::new(move |mut w| {
            let (start, mut offset) = w.checkpoint();
            for (i, url) in urls.iter().enumerate().skip(start) {
                let bytes = match core::stream_retry(&w, "hls fetch", || fetch_segment(&client, url)) {
                    Ok(b) => b,
                    Err(e) => return w.fail(format!("hls fetch {url}: {e}")),
                };
                if w.write_at(offset, &bytes).is_err() {
                    return;
                }
                offset += bytes.len() as u64;
                w.set_checkpoint(i + 1);
                w.set_progress((i + 1) as f32 / urls.len() as f32);
            }
            w.finish();
        }))))
    }

    fn hls_playlist(&self, playlist_url: &str, preset: &str) -> Result<HlsPlaylist> {
        log::debug!("soundcloud: GET {playlist_url} (HLS playlist, preset {preset})");
        let text = self
            .client
            .get(playlist_url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| src_err(format!("hls playlist: {}", e.without_url())))?
            .text()
            .map_err(|e| src_err(format!("hls playlist: {}", e.without_url())))?;
        let base = url::Url::parse(playlist_url).map_err(|e| src_err(format!("hls playlist url: {e}")))?;
        let playlist = parse_hls_playlist(&text, &base)?;
        if playlist.segments.is_empty() {
            return Err(src_err("hls playlist: no media segments"));
        }
        Ok(playlist)
    }
}

/// api-v2 reports no totals on cursor-paged lists: report one that is never reached while more pages remain.
fn remote_page<T>(hits: Vec<T>, offset: usize, consumed: usize, more: bool) -> RemotePage<T> {
    RemotePage { hits, total: offset + consumed + usize::from(more && consumed > 0), consumed }
}

/// `target` is an api-v2 path or an absolute `next_href`; its own `client_id` is replaced with the current one.
fn request_url(target: &str, query: &[(&str, &str)], client_id: &str) -> Result<url::Url> {
    let full = if target.starts_with('/') { format!("{API}{target}") } else { target.to_string() };
    let mut url = url::Url::parse(&full).map_err(|e| src_err(format!("bad url {target:?}: {e}")))?;
    let api = url::Url::parse(API).unwrap();
    if url.scheme() != api.scheme() || url.host_str() != api.host_str() || url.port().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(src_err(format!("refusing non-api-v2 url {target:?}")));
    }
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "client_id")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.extend(query.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    pairs.push(("client_id".to_string(), client_id.to_string()));
    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url)
}

/// `(node id, title)` of a home-feed shelf item that is a playlist: `system:<urn>` for personalized
/// ones, the numeric id otherwise. Stations, tracks, users and albums are not playlists here.
fn shelf_playlist_id(item: &serde_json::Value) -> Option<(String, String)> {
    let str_of = |k: &str| item.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let urn = str_of("urn").unwrap_or("");
    if item.get("is_album").and_then(|a| a.as_bool()) == Some(true) {
        return None;
    }
    let id = if urn.starts_with("soundcloud:system-playlists:") {
        format!("{SYSTEM_PREFIX}{urn}")
    } else if urn.starts_with("soundcloud:playlists:") || str_of("kind") == Some("playlist") {
        let numeric = item.get("id").and_then(|i| i.as_u64());
        let from_urn = urn.strip_prefix("soundcloud:playlists:").and_then(|n| n.parse().ok());
        numeric.or(from_urn)?.to_string()
    } else {
        return None;
    };
    let title = ["title", "short_title"].iter().find_map(|k| str_of(k)).map(str::to_string);
    let title = title.unwrap_or_else(|| format!("Playlist {}", id.rsplit(':').next().unwrap_or(&id)));
    Some((id, title))
}

/// GETs an init or media segment — no `client_id` needed, the URLs are already presigned.
fn fetch_segment(client: &reqwest::blocking::Client, url: &url::Url) -> std::io::Result<Vec<u8>> {
    let resp = client.get(url.clone()).send().and_then(|r| r.error_for_status()).map_err(std::io::Error::other)?;
    Ok(resp.bytes().map_err(std::io::Error::other)?.to_vec())
}

/// The bits `open_hls` needs out of a media-playlist `.m3u8`: an optional
/// fMP4 init segment (`#EXT-X-MAP`) and the ordered media segment URLs
/// (each a non-`#` line following an `#EXTINF`). Hand-rolled rather than a
/// full HLS crate — this is the whole grammar this MVP needs.
struct HlsPlaylist {
    init: Option<url::Url>,
    segments: Vec<url::Url>,
}

fn parse_hls_playlist(text: &str, base: &url::Url) -> Result<HlsPlaylist> {
    let mut init = None;
    let mut segments = Vec::new();
    let mut expect_segment = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = ext_x_map_uri(attrs) {
                init = Some(resolve_hls_url(base, &uri)?);
            }
        } else if line.starts_with("#EXTINF") {
            expect_segment = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            if expect_segment {
                segments.push(resolve_hls_url(base, line)?);
            }
            expect_segment = false;
        }
    }
    Ok(HlsPlaylist { init, segments })
}

/// Pulls `URI="..."` out of an `#EXT-X-MAP:URI="...",...` attribute list.
fn ext_x_map_uri(attrs: &str) -> Option<String> {
    attrs
        .split(',')
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix("URI=\"")?.strip_suffix('"'))
        .map(str::to_string)
}

fn resolve_hls_url(base: &url::Url, uri: &str) -> Result<url::Url> {
    base.join(uri).map_err(|e| src_err(format!("hls playlist: bad url {uri:?}: {e}")))
}

impl Source for SoundcloudSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn recognizes(&self, uri: &str) -> bool {
        crate::uri::recognizes(uri)
    }

    fn share_url(&self, uri: &str) -> Option<String> {
        match TrackRef::parse(uri)? {
            TrackRef::Permalink(url) => Some(url),
            TrackRef::Id(id) => match self.track_by_id(id, &|| true) {
                Ok(t) => t.permalink_url,
                Err(e) => {
                    log::warn!("soundcloud: share_url: track {id}: {e}");
                    None
                }
            },
        }
    }

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(Track)) -> Result<()> {
        let text = q.text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let limit = if q.limit == 0 { 50 } else { q.limit.min(200) };
        let limit_s = limit.to_string();
        let v = self.api_get("/search/tracks", &[("q", text), ("limit", &limit_s)])?;
        let Some(collection) = v.get("collection").and_then(|c| c.as_array()) else {
            return Ok(());
        };
        for item in collection {
            let Ok(t) = serde_json::from_value::<ApiTrack>(item.clone()) else {
                continue;
            };
            if let Some(hit) = t.into_track() {
                sink(hit);
            }
        }
        Ok(())
    }

    fn resolve(&self, uri: &str) -> Result<Track> {
        let r = TrackRef::parse(uri).ok_or_else(|| src_err(format!("not a SoundCloud track: {uri:?}")))?;
        self.track_ref(&r, &|| true)?
            .into_track()
            .ok_or_else(|| src_err("track has no full-length stream (preview-only or unavailable)"))
    }

    fn is_synthetic(&self, node: &BrowseNode) -> bool {
        matches!(node, BrowseNode::Path(id) if id == LIKED_TRACKS || id.starts_with(SYSTEM_PREFIX))
    }

    fn liked_songs_node(&self) -> Option<BrowseNode> {
        Some(BrowseNode::Path(LIKED_TRACKS.to_string()))
    }

    fn adds_first(&self, node: &BrowseNode) -> bool {
        matches!(node, BrowseNode::Path(id) if id == LIKED_TRACKS)
    }

    fn add_to_playlist(&self, node: &BrowseNode, track_uri: &str) -> Result<()> {
        match node {
            BrowseNode::Path(id) if id == LIKED_TRACKS => self.set_like(track_uri, true),
            BrowseNode::Path(id) => {
                let track = Self::track_id(track_uri)?;
                self.edit_playlist(id, |ids| {
                    ids.push(track);
                    Ok(())
                })
            }
            BrowseNode::Root => Err(Error::Unsupported("add_to_playlist: not a playlist")),
        }
    }

    fn remove_from_playlist(&self, node: &BrowseNode, track_uri: &str, position: Option<usize>) -> Result<()> {
        match node {
            BrowseNode::Path(id) if id == LIKED_TRACKS => self.set_like(track_uri, false),
            BrowseNode::Path(id) => {
                let track = Self::track_id(track_uri)?;
                self.edit_playlist(id, |ids| {
                    // Listing rows skip unavailable tracks, so `position` only disambiguates duplicates.
                    let hits: Vec<usize> = (0..ids.len()).filter(|&i| ids[i] == track).collect();
                    match (hits.as_slice(), position) {
                        ([at], _) => {
                            ids.remove(*at);
                        }
                        ([], _) => return Err(src_err("the playlist changed: that track is no longer in it")),
                        (_, Some(at)) if ids.get(at) == Some(&track) => {
                            ids.remove(at);
                        }
                        (_, Some(_)) => return Err(src_err("the playlist changed: that row is no longer this track")),
                        (_, None) => ids.retain(|id| *id != track),
                    }
                    Ok(())
                })
            }
            BrowseNode::Root => Err(Error::Unsupported("remove_from_playlist: not a playlist")),
        }
    }

    fn retry_browse(&self, node: &BrowseNode) {
        match node {
            BrowseNode::Root => {
                self.playlists.list.retry();
                self.shelves.list.retry();
            }
            BrowseNode::Path(id) if id == LIKED_TRACKS => self.liked().list.retry(),
            BrowseNode::Path(id) => {
                if let Some(list) = self.playlist_tracks.get(id) {
                    list.retry();
                }
            }
        }
    }

    fn forget_playlist(&self, node: &BrowseNode) {
        if let BrowseNode::Path(id) = node {
            self.playlist_tracks.remove(id);
            self.playlist_docs.lock().unwrap().remove(id);
        }
    }

    fn browse(&self, node: &BrowseNode, want: usize) -> Result<BrowsePage> {
        match node {
            // Root: "Liked Tracks", the user's own playlists, then the shelf playlists — all need a
            // login, so no folders without one.
            BrowseNode::Root => {
                let mut folders = vec![];
                let mut node_meta = vec![];
                let (mut partial, mut errored) = (false, false);
                if self.oauth_token.is_some() {
                    let liked = BrowseNode::Path(LIKED_TRACKS.to_string());
                    node_meta.push((liked.clone(), READ_ONLY));
                    folders.push(("Liked Tracks".to_string(), liked));
                    let (own, own_partial) = self.playlists.snapshot(self, want, Self::playlists_page);
                    let (shelf, shelf_partial) = self.shelves.snapshot(self, want, Self::shelves_page);
                    let own_nodes: HashSet<&BrowseNode> = own.iter().map(|(_, n, _)| n).collect();
                    let extra: Vec<_> = shelf.iter().filter(|(_, n)| !own_nodes.contains(n)).cloned().collect();
                    node_meta.extend(own.iter().map(|(_, n, meta)| (n.clone(), meta.clone())));
                    node_meta.extend(extra.iter().map(|(_, n)| (n.clone(), READ_ONLY)));
                    folders.extend(own.iter().map(|(name, n, _)| (name.clone(), n.clone())));
                    folders.extend(extra);
                    partial = own_partial || shelf_partial;
                    errored = self.playlists.list.errored() || self.shelves.list.errored();
                }
                Ok(BrowsePage { title: "SoundCloud".to_string(), tracks: vec![], folders, node_meta, partial, errored })
            }
            BrowseNode::Path(id) if id == LIKED_TRACKS => {
                let liked = self.liked();
                let (tracks, partial) = liked.snapshot(self, want, Self::likes_page);
                Ok(BrowsePage {
                    title: "Liked Tracks".to_string(),
                    tracks,
                    folders: vec![],
                    node_meta: vec![],
                    partial,
                    errored: liked.list.errored(),
                })
            }
            // A playlist id: its tracks.
            BrowseNode::Path(id) => {
                let list = self.playlist_tracks.get_or_create(id);
                let src = self.clone();
                let pid = id.clone();
                let (tracks, partial) = list.snapshot(&self.bus, want, move |offset| src.playlist_page(&pid, offset));
                let title = self.playlist_docs.lock().unwrap().get(id).map_or_else(|| id.clone(), |d| d.title.clone());
                Ok(BrowsePage { title, tracks, folders: vec![], node_meta: vec![], partial, errored: list.errored() })
            }
        }
    }
}

impl MediaProvider for SoundcloudSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn open(&self, r: &Rendition, wanted: &dyn Fn() -> bool) -> Result<Media> {
        let track_ref = TrackRef::parse(&r.uri)
            .ok_or_else(|| src_err(format!("not a SoundCloud track: {:?}", r.uri)))?;
        let track = self.track_ref(&track_ref, wanted)?;
        if track.media.full_transcodings().next().is_none() {
            return Err(src_err("only a 30 s preview is available"));
        }

        if self.hls {
            match self.open_hls(&track, wanted) {
                Ok(Some(media)) => return Ok(media),
                Ok(None) => {}
                Err(e) if !wanted() => return Err(e),
                Err(e) => log::debug!("soundcloud: HLS path unavailable, falling back to progressive: {e}"),
            }
        }

        // Pick the progressive (plain-file) transcoding; the player downloads it.
        let prog = track
            .media
            .full_transcodings()
            .find(|t| t.format.protocol == "progressive")
            .ok_or_else(|| src_err("MVP: track has only HLS streams, no progressive"))?;

        let v = self.api_get_with(&prog.url, &[], Denied::StaleClientId, wanted)?;
        let cdn = v
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| src_err("stream url: no 'url' in response"))?;
        Ok(Media::Url(cdn.to_string()))
    }
}

// ---- API JSON (only the fields we use) ----

#[derive(Deserialize, Clone)]
struct ApiTrack {
    id: u64,
    title: String,
    #[serde(default)]
    permalink_url: Option<String>,
    #[serde(default)]
    duration: u32, // ms; only the 30 s snippet's length on snipped tracks
    #[serde(default)]
    full_duration: Option<u32>,
    #[serde(default)]
    user: Option<ApiUser>,
    #[serde(default)]
    publisher_metadata: Option<ApiPublisher>,
    #[serde(default)]
    media: ApiMedia,
    #[serde(default)]
    waveform_url: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
struct ApiUser {
    #[serde(default)]
    username: String,
}

/// A `/me/library/all` entry; only the playlist-like kinds carry a `playlist`.
#[derive(Deserialize)]
struct ApiLibraryItem {
    #[serde(default)]
    playlist: Option<ApiPlaylist>,
}

#[derive(Deserialize)]
struct ApiPlaylist {
    id: u64,
    title: String,
    #[serde(default)]
    is_album: Option<bool>,
    #[serde(default)]
    user: Option<ApiOwner>,
}

#[derive(Deserialize)]
struct ApiOwner {
    id: u64,
}

/// A `track_likes` collection entry.
#[derive(Deserialize)]
struct ApiLike {
    #[serde(default)]
    track: Option<ApiTrack>,
}

#[derive(Deserialize, Clone)]
struct ApiPublisher {
    #[serde(default)]
    isrc: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
struct ApiMedia {
    #[serde(default)]
    transcodings: Vec<ApiTranscoding>,
}

impl ApiMedia {
    /// A snipped transcoding is a 30 s preview, never the full track.
    fn full_transcodings(&self) -> impl Iterator<Item = &ApiTranscoding> {
        self.transcodings.iter().filter(|t| !t.snipped)
    }
}

#[derive(Deserialize, Clone)]
struct ApiTranscoding {
    url: String,
    format: ApiFormat,
    #[serde(default)]
    preset: String,
    #[serde(default)]
    snipped: bool,
}

#[derive(Deserialize, Default, Clone)]
struct ApiFormat {
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    mime_type: String,
}

/// `Track::attrs` key holding the track's waveform JSON URL, set at import.
pub(crate) const WAVEFORM_URL_ATTR: &str = "soundcloud_waveform_url";

impl ApiTrack {
    fn into_track(self) -> Option<Track> {
        if self.media.full_transcodings().next().is_none() {
            log::debug!("soundcloud: skipping preview-only track {:?} (soundcloud:track:{})", self.title, self.id);
            return None;
        }
        let (mut artists, title) = core::parse_artist_title(&self.title);
        if artists.is_empty()
            && let Some(u) = &self.user
            && !u.username.is_empty()
        {
            artists.push(u.username.clone());
        }
        let isrc = self
            .publisher_metadata
            .and_then(|p| p.isrc)
            .filter(|s| !s.trim().is_empty());
        let full = self.full_duration.filter(|&d| d > 0).unwrap_or(self.duration);
        let rendition = Rendition::fresh(source_id(), format!("soundcloud:track:{}", self.id), full, Quality::Lossy { kbps: None });
        let mut attrs = std::collections::BTreeMap::new();
        if let Some(u) = self.waveform_url.filter(|u| !u.trim().is_empty()) {
            attrs.insert(WAVEFORM_URL_ATTR.to_string(), u);
        }
        Some(Track { isrc, attrs, ..Track::fresh(title, artists, rendition) })
    }
}
