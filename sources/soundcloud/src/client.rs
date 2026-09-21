//! The SoundCloud API client + `Source` / `MediaProvider` impls.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::{
    Bus, BrowseNode, BrowsePage, Error, Media, MediaProvider, PagedList, PagedMap, Quality, RateLimiter,
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

/// `BrowseNode::Path` prefix for a personalized recommendation playlist; the rest is its urn.
const SYSTEM_PREFIX: &str = "system:";

/// Page size for walking `/me/track_likes` — comfortably under any
/// documented api-v2 limit cap.
const LIKED_PAGE_SIZE: usize = 50;

const PLAYLISTS_PAGE_SIZE: usize = 200;

const SHELVES_PAGE_SIZE: usize = 20;

/// Shelves walked on the home feed before stopping.
const SHELF_CAP: usize = 60;

/// Playlist tracks hydrated per page — `/tracks?ids=` takes at most 50.
const PLAYLIST_PAGE_SIZE: usize = 50;

/// What a 401/403 (or 404) from an endpoint means.
#[derive(Clone, Copy)]
enum Denied {
    /// A stale scraped client_id: reset it and retry once.
    StaleClientId,
    /// Not available to this account: no error, just no data.
    Unavailable,
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

/// `next_href` of the list's next page, kept across `PagedList` walk restarts.
#[derive(Clone, Default)]
struct Cursor(Arc<Mutex<Option<String>>>);

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
    cached_id: Arc<Mutex<Option<String>>>,
    /// A user OAuth token. Needed only for `/me/...` endpoints (Liked
    /// Tracks, playlists); search/resolve/play work without it.
    oauth_token: Option<String>,
    bus: Bus,
    liked: PagedList<Track>,
    liked_cursor: Cursor,
    playlists: PagedList<(String, BrowseNode)>,
    playlists_cursor: Cursor,
    /// Playlists on the home feed's shelves.
    shelves: PagedList<(String, BrowseNode)>,
    shelves_cursor: Cursor,
    shelf_seen: Arc<Mutex<HashSet<String>>>,
    /// Playlist contents by node id (numeric or `system:<urn>`).
    playlist_tracks: PagedMap<Track>,
    playlist_docs: Arc<Mutex<HashMap<String, Arc<PlaylistDoc>>>>,
    /// From `[soundcloud] hls` — prefer a higher-bitrate HLS stream over
    /// the 128kbps progressive one when the track offers one.
    hls: bool,
    /// Paces `api_get` and the client-id scrape — no `RateGate`-style 429
    /// cooldown here, api-v2 hasn't been observed to need one.
    limiter: Arc<RateLimiter>,
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
            cached_id: Arc::new(Mutex::new(None)),
            oauth_token: oauth_token.filter(|s| !s.trim().is_empty()),
            bus,
            liked: PagedList::new("soundcloud: liked tracks"),
            liked_cursor: Cursor::default(),
            playlists: PagedList::new("soundcloud: playlists"),
            playlists_cursor: Cursor::default(),
            shelves: PagedList::new("soundcloud: shelf playlists"),
            shelves_cursor: Cursor::default(),
            shelf_seen: Arc::default(),
            playlist_tracks: PagedMap::new("soundcloud: playlist"),
            playlist_docs: Arc::default(),
            hls,
            limiter: Arc::new(RateLimiter::new(Duration::from_millis(200), Duration::from_millis(100))),
        }
    }

    /// The `client_id` query param every endpoint needs. Configured value wins;
    /// otherwise scraped once from the public web bundle and cached.
    fn client_id(&self) -> Result<String> {
        if let Some(id) = self.cached_id.lock().unwrap().clone() {
            return Ok(id);
        }
        let id = match &self.configured_id {
            Some(id) => id.clone(),
            None => self.scrape_client_id()?,
        };
        *self.cached_id.lock().unwrap() = Some(id.clone());
        log::info!("soundcloud: using client_id {}…", &id[..id.len().min(6)]);
        Ok(id)
    }

    fn scrape_client_id(&self) -> Result<String> {
        log::debug!("soundcloud: GET {WEB} (scraping client_id)");
        self.limiter.throttle();
        let resp = self.client.get(WEB).send().map_err(|e| src_err(format!("web player unreachable: {e}")))?;
        log::debug!("soundcloud: GET {WEB} -> {}", resp.status());
        let home = resp
            .error_for_status()
            .map_err(|e| src_err(format!("web player unreachable: {e}")))?
            .text()
            .map_err(|e| src_err(format!("web player unreachable: {e}")))?;

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

        for url in scripts {
            log::debug!("soundcloud: GET {url} (scanning for client_id)");
            self.limiter.throttle();
            let attempt = self.client.get(url).send().and_then(|r| {
                log::debug!("soundcloud: GET {url} -> {}", r.status());
                r.error_for_status().and_then(|r| r.text())
            });
            let Ok(body) = attempt else {
                log::debug!("soundcloud: {url}: {}", attempt.unwrap_err());
                continue;
            };
            if let Some(c) = id_re.captures(&body) {
                return Ok(c.get(1).unwrap().as_str().to_string());
            }
        }
        Err(src_err(
            "could not scrape a client_id; set [soundcloud] client_id in config",
        ))
    }

    /// The oauth token, or an error naming the config key — for endpoints
    /// (Liked Tracks, playlists) that need a logged-in user and have no
    /// meaningful fallback.
    fn require_auth(&self) -> Result<&str> {
        self.oauth_token
            .as_deref()
            .ok_or_else(|| src_err("needs a SoundCloud login — log in from the SoundCloud setup dialog in Settings"))
    }

    /// The logged-in username, or `None` if SoundCloud rejects the token.
    pub(crate) fn me(&self) -> Result<Option<String>> {
        self.require_auth()?;
        let Some(json) = self.send("/me", &[], Denied::StaleClientId)? else {
            return Ok(None);
        };
        let name = ["username", "permalink"].iter().find_map(|k| json.get(k)?.as_str()).unwrap_or("your account");
        Ok(Some(name.to_string()))
    }

    fn api_get(&self, path: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
        self.api_get_with(path, query, Denied::StaleClientId)
    }

    /// `Error::NotFound` when the endpoint denies this account (or, for `Denied::StaleClientId`, keeps rejecting the id).
    fn api_get_with(&self, target: &str, query: &[(&str, &str)], denied: Denied) -> Result<serde_json::Value> {
        self.send(target, query, denied)?.ok_or(Error::NotFound)
    }

    /// GET `target` (an api-v2 path or an absolute `next_href`); `None` when denied.
    fn send(&self, target: &str, query: &[(&str, &str)], denied: Denied) -> Result<Option<serde_json::Value>> {
        let attempts = if matches!(denied, Denied::StaleClientId) && self.configured_id.is_none() { 2 } else { 1 };
        for _ in 0..attempts {
            let url = request_url(target, query, &self.client_id()?)?;
            let path = url.path().to_string();
            log::debug!("soundcloud: GET {path} {query:?}");
            self.limiter.throttle();
            let mut req = self.client.get(url);
            if let Some(token) = &self.oauth_token {
                req = req.header("Authorization", format!("OAuth {token}"));
            }
            let resp = req.send().map_err(|e| src_err(format!("{path}: {e}")))?;
            let status = resp.status();
            if status.is_success() {
                log::debug!("soundcloud: GET {path} -> {status}");
            } else {
                log::warn!("soundcloud: GET {path} -> {status}");
            }
            match classify_status(status, denied) {
                Outcome::Success => {
                    return resp.json().map(Some).map_err(|e| src_err(format!("{path}: bad json: {e}")));
                }
                Outcome::Denied => {
                    if matches!(denied, Denied::StaleClientId) {
                        *self.cached_id.lock().unwrap() = None;
                        log::warn!("soundcloud: client_id rejected ({status}), will re-scrape next call");
                    }
                }
                Outcome::Failed => return Err(src_err(format!("{path}: HTTP {status}"))),
            }
        }
        Ok(None)
    }

    fn track_by_id(&self, id: u64) -> Result<ApiTrack> {
        let v = self.api_get(&format!("/tracks/{id}"), &[])?;
        serde_json::from_value(v).map_err(|e| src_err(format!("track {id}: {e}")))
    }

    /// The samples of a SoundCloud-drawn waveform, if the JSON at `url` has any.
    pub(crate) fn waveform_samples(&self, url: &str) -> Result<Option<Vec<f32>>> {
        self.limiter.throttle();
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

    fn resolve_permalink(&self, url: &str) -> Result<ApiTrack> {
        let v = self.api_get("/resolve", &[("url", url)])?;
        if v.get("kind").and_then(|k| k.as_str()) != Some("track") {
            return Err(src_err("that SoundCloud URL is not a track"));
        }
        serde_json::from_value(v).map_err(|e| src_err(format!("resolve {url}: {e}")))
    }

    fn track_ref(&self, r: &TrackRef) -> Result<ApiTrack> {
        match r {
            TrackRef::Id(id) => self.track_by_id(*id),
            TrackRef::Permalink(url) => self.resolve_permalink(url),
        }
    }

    /// The `collection` array of an api-v2 list endpoint and its `next_href`; empty (and logged) if the response has none.
    fn collection_page(&self, target: &str, query: &[(&str, &str)], denied: Denied) -> Result<(Vec<serde_json::Value>, Option<String>)> {
        self.require_auth()?;
        let mut v = self.api_get_with(target, query, denied)?;
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

    /// One `PagedList` page of a cursor-paged endpoint: `offset` 0 starts at `path`, later offsets follow the remembered `next_href`.
    fn cursor_page(
        &self,
        cursor: &Cursor,
        offset: usize,
        path: &str,
        query: &[(&str, &str)],
        denied: Denied,
    ) -> Result<(Vec<serde_json::Value>, bool)> {
        let (target, query) = if offset == 0 {
            (Some(path.to_string()), query)
        } else {
            (cursor.0.lock().unwrap().clone(), &[][..])
        };
        let Some(target) = target else {
            return Ok((vec![], false));
        };
        let (items, next) = self.collection_page(&target, query, denied)?;
        let more = next.is_some();
        *cursor.0.lock().unwrap() = next;
        Ok((items, more))
    }

    /// One page of the user's own `/me/playlists`.
    fn playlists_page(&self, offset: usize) -> std::result::Result<RemotePage<(String, BrowseNode)>, String> {
        let limit = PLAYLISTS_PAGE_SIZE.to_string();
        let (items, more) = self
            .cursor_page(&self.playlists_cursor, offset, "/me/playlists", &[("limit", &limit)], Denied::StaleClientId)
            .map_err(|e| e.to_string())?;
        let consumed = items.len();
        let hits = items
            .into_iter()
            .filter_map(|item| serde_json::from_value::<ApiPlaylist>(item).ok())
            .map(|p| (p.title, BrowseNode::Path(p.id.to_string())))
            .collect();
        Ok(remote_page(hits, offset, consumed, more))
    }

    /// One page of `/mixed-selections` shelves, as the playlists on them; not available to every account.
    fn shelves_page(&self, offset: usize) -> std::result::Result<RemotePage<(String, BrowseNode)>, String> {
        let limit = SHELVES_PAGE_SIZE.to_string();
        let (shelves, more) = match self.cursor_page(&self.shelves_cursor, offset, "/mixed-selections", &[("limit", &limit)], Denied::Unavailable) {
            Ok(page) => page,
            Err(Error::NotFound) => {
                log::warn!("soundcloud: recommendations not available for this account");
                (vec![], false)
            }
            Err(e) => return Err(e.to_string()),
        };
        let consumed = shelves.len();
        let mut seen = self.shelf_seen.lock().unwrap();
        if offset == 0 {
            seen.clear();
        }
        let hits: Vec<_> = shelves
            .iter()
            .filter_map(|s| s.get("items")?.get("collection")?.as_array())
            .flatten()
            .filter_map(shelf_playlist_id)
            .filter(|(id, _)| seen.insert(id.clone()))
            .map(|(id, title)| (title, BrowseNode::Path(id)))
            .collect();
        if hits.is_empty() {
            log::debug!("soundcloud: /mixed-selections: {consumed} shelf(s), no new playlists");
        }
        Ok(remote_page(hits, offset, consumed, more && offset + consumed < SHELF_CAP))
    }

    /// The playlist object for `id` (a numeric playlist id or `system:<urn>`), fetched at the
    /// list's start and reused for its later pages. `representation=full` inlines full track
    /// objects for the first ones; the rest are id-only stubs, hydrated per page.
    fn playlist_doc(&self, id: &str, offset: usize) -> Result<Arc<PlaylistDoc>> {
        if offset > 0
            && let Some(doc) = self.playlist_docs.lock().unwrap().get(id)
        {
            return Ok(doc.clone());
        }
        let (path, denied) = match id.strip_prefix(SYSTEM_PREFIX) {
            Some(urn) => (format!("/system-playlists/{urn}"), Denied::Unavailable),
            None => (format!("/playlists/{id}"), Denied::StaleClientId),
        };
        let v = self.api_get_with(&path, &[("representation", "full")], denied)?;
        let doc = Arc::new(PlaylistDoc {
            title: v.get("title").and_then(|t| t.as_str()).unwrap_or(id).to_string(),
            items: v.get("tracks").and_then(|t| t.as_array()).cloned().unwrap_or_default(),
        });
        self.playlist_docs.lock().unwrap().insert(id.to_string(), doc.clone());
        Ok(doc)
    }

    fn playlist_page(&self, id: &str, offset: usize) -> std::result::Result<RemotePage<Track>, String> {
        let doc = self.playlist_doc(id, offset).map_err(|e| e.to_string())?;
        let slice: Vec<_> = doc.items.iter().skip(offset).take(PLAYLIST_PAGE_SIZE).cloned().collect();
        let hits = self.hydrate_tracks(&slice).into_iter().filter_map(ApiTrack::into_track).collect();
        Ok(RemotePage { hits, total: doc.items.len(), consumed: slice.len() })
    }

    /// Full tracks in order; id-only stubs are fetched via `/tracks?ids=`, and unavailable ones dropped.
    fn hydrate_tracks(&self, items: &[serde_json::Value]) -> Vec<ApiTrack> {
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
            match self.api_get("/tracks", &[("ids", &ids)]) {
                Ok(v) => {
                    for t in v.as_array().into_iter().flatten() {
                        if let Ok(t) = ApiTrack::deserialize(t) {
                            hydrated.insert(t.id, t);
                        }
                    }
                }
                Err(e) => log::warn!("soundcloud: hydrating {} track stubs failed: {e}", chunk.len()),
            }
        }
        let out: Vec<ApiTrack> = slots
            .into_iter()
            .filter_map(|s| s.or_else(|id| hydrated.get(&id).cloned().ok_or(())).ok())
            .collect();
        if out.len() < total {
            log::debug!("soundcloud: {} of {total} playlist tracks unavailable", total - out.len());
        }
        out
    }

    /// One page of `/me/track_likes`, for `core::PagedList`.
    fn likes_page(&self, offset: usize) -> std::result::Result<RemotePage<Track>, String> {
        let limit = LIKED_PAGE_SIZE.to_string();
        let (items, more) = self
            .cursor_page(&self.liked_cursor, offset, "/me/track_likes", &[("limit", &limit)], Denied::StaleClientId)
            .map_err(|e| e.to_string())?;
        let consumed = items.len();
        let hits = items
            .into_iter()
            .filter_map(|item| serde_json::from_value::<ApiLike>(item).ok())
            .filter_map(|l| l.track)
            .filter_map(ApiTrack::into_track)
            .collect();
        Ok(remote_page(hits, offset, consumed, more))
    }

    /// Best-quality HLS path: resolves the AAC-160k transcoding's signed playlist here, then returns a
    /// stream that appends the init segment and every media segment in order (one fMP4 `symphonia`
    /// decodes like any other). A setup failure is soft — `open` falls back to the progressive stream.
    fn open_hls(&self, track: &ApiTrack) -> Result<Media> {
        let hls = track
            .media
            .full_transcodings()
            .find(|t| t.format.protocol == "hls" && t.format.mime_type.starts_with("audio/mp4"))
            .ok_or_else(|| src_err("no AAC HLS transcoding"))?;

        let id = self.client_id()?;
        log::debug!("soundcloud: GET {} (resolve HLS playlist url, preset {})", hls.url, hls.preset);
        let v: serde_json::Value = self
            .client
            .get(&hls.url)
            .query(&[("client_id", id.as_str())])
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| src_err(format!("hls stream url: {e}")))?
            .json()
            .map_err(|e| src_err(format!("hls stream url: {e}")))?;
        let playlist_url = v
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| src_err("hls stream url: no 'url' in response"))?;

        log::debug!("soundcloud: GET {playlist_url} (HLS playlist, preset {})", hls.preset);
        let playlist_text = self
            .client
            .get(playlist_url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| src_err(format!("hls playlist: {e}")))?
            .text()
            .map_err(|e| src_err(format!("hls playlist: {e}")))?;

        let base = url::Url::parse(playlist_url).map_err(|e| src_err(format!("hls playlist url: {e}")))?;
        let playlist = parse_hls_playlist(&playlist_text, &base)?;
        if playlist.segments.is_empty() {
            return Err(src_err("hls playlist: no media segments"));
        }

        log::info!("soundcloud: playing via HLS (preset {}) instead of 128kbps progressive", hls.preset);
        let client = self.client.clone();
        let urls: Vec<url::Url> = playlist.init.into_iter().chain(playlist.segments).collect();
        Ok(Media::Stream(Box::new(move |mut w| {
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
        })))
    }
}

/// api-v2 reports no totals on cursor-paged lists: report one that is never reached while more pages remain.
fn remote_page<T>(hits: Vec<T>, offset: usize, consumed: usize, more: bool) -> RemotePage<T> {
    RemotePage { hits, total: offset + consumed + usize::from(more), consumed }
}

/// `target` is an api-v2 path or an absolute `next_href`; its own `client_id` is replaced with the current one.
fn request_url(target: &str, query: &[(&str, &str)], client_id: &str) -> Result<url::Url> {
    let full = if target.starts_with('/') { format!("{API}{target}") } else { target.to_string() };
    let mut url = url::Url::parse(&full).map_err(|e| src_err(format!("bad url {target:?}: {e}")))?;
    if !full.starts_with(&format!("{API}/")) {
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
            TrackRef::Id(id) => match self.track_by_id(id) {
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
        self.track_ref(&r)?
            .into_track()
            .ok_or_else(|| src_err("track has no full-length stream (preview-only or unavailable)"))
    }

    fn is_synthetic(&self, node: &BrowseNode) -> bool {
        matches!(node, BrowseNode::Path(id) if id.starts_with(SYSTEM_PREFIX))
    }

    fn retry_browse(&self, node: &BrowseNode) {
        match node {
            BrowseNode::Root => {
                self.playlists.retry();
                self.shelves.retry();
            }
            BrowseNode::Path(id) if id == LIKED_TRACKS => self.liked.retry(),
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
                let (mut partial, mut errored) = (false, false);
                if self.oauth_token.is_some() {
                    folders.push(("Liked Tracks".to_string(), BrowseNode::Path(LIKED_TRACKS.to_string())));
                    let src = self.clone();
                    let (own, own_partial) = self.playlists.snapshot(&self.bus, want, move |o| src.playlists_page(o));
                    let src = self.clone();
                    let (shelf, shelf_partial) = self.shelves.snapshot(&self.bus, want, move |o| src.shelves_page(o));
                    let extra: Vec<_> = shelf.into_iter().filter(|(_, n)| !own.iter().any(|(_, o)| o == n)).collect();
                    folders.extend(own);
                    folders.extend(extra);
                    partial = own_partial || shelf_partial;
                    errored = self.playlists.errored() || self.shelves.errored();
                }
                Ok(BrowsePage { title: "SoundCloud".to_string(), tracks: vec![], folders, partial, errored })
            }
            BrowseNode::Path(id) if id == LIKED_TRACKS => {
                let src = self.clone();
                let (tracks, partial) = self.liked.snapshot(&self.bus, want, move |offset| src.likes_page(offset));
                Ok(BrowsePage {
                    title: "Liked Tracks".to_string(),
                    tracks,
                    folders: vec![],
                    partial,
                    errored: self.liked.errored(),
                })
            }
            // A playlist id: its tracks.
            BrowseNode::Path(id) => {
                let list = self.playlist_tracks.get_or_create(id);
                let src = self.clone();
                let pid = id.clone();
                let (tracks, partial) = list.snapshot(&self.bus, want, move |offset| src.playlist_page(&pid, offset));
                let title = self.playlist_docs.lock().unwrap().get(id).map_or_else(|| id.clone(), |d| d.title.clone());
                Ok(BrowsePage { title, tracks, folders: vec![], partial, errored: list.errored() })
            }
        }
    }
}

impl MediaProvider for SoundcloudSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn open(&self, r: &Rendition, _wanted: &dyn Fn() -> bool) -> Result<Media> {
        let track_ref = TrackRef::parse(&r.uri)
            .ok_or_else(|| src_err(format!("not a SoundCloud track: {:?}", r.uri)))?;
        let track = self.track_ref(&track_ref)?;
        if track.media.full_transcodings().next().is_none() {
            return Err(src_err("only a 30 s preview is available"));
        }

        if self.hls {
            match self.open_hls(&track) {
                Ok(media) => return Ok(media),
                Err(e) => log::debug!("soundcloud: HLS path unavailable, falling back to progressive: {e}"),
            }
        }

        // Pick the progressive (plain-file) transcoding; the player downloads it.
        let prog = track
            .media
            .full_transcodings()
            .find(|t| t.format.protocol == "progressive")
            .ok_or_else(|| src_err("MVP: track has only HLS streams, no progressive"))?;

        let id = self.client_id()?;
        log::debug!("soundcloud: GET {} (resolve progressive stream url)", prog.url);
        let resp = self
            .client
            .get(&prog.url)
            .query(&[("client_id", id.as_str())])
            .send()
            .map_err(|e| src_err(format!("stream url: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            log::debug!("soundcloud: GET {} -> {status}", prog.url);
        } else {
            log::warn!("soundcloud: GET {} -> {status}", prog.url);
        }
        let v: serde_json::Value = resp
            .error_for_status()
            .and_then(|r| r.json())
            .map_err(|e| src_err(format!("stream url: {e}")))?;
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

/// A `/me/playlists` collection entry — id/title only, matching what
/// `browse`'s Root folder listing needs. MVP: some api-v2 responses nest
/// this under a `playlist` key instead of at the top level; untested
/// against a live token.
#[derive(Deserialize)]
struct ApiPlaylist {
    id: u64,
    title: String,
}

/// A `/me/track_likes` collection entry.
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
