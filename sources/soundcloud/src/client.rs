//! The SoundCloud API client + `Source` / `MediaProvider` impls.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::{
    Bus, BrowseNode, BrowsePage, Error, Media, MediaProvider, PagedList, Quality, Rendition,
    RemotePage, Result, SearchHit, SearchQuery, Source, SourceId,
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

/// Page size for walking `/me/track_likes` — comfortably under any
/// documented api-v2 limit cap.
const LIKED_PAGE_SIZE: usize = 50;

fn source_id() -> SourceId {
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
    /// Liked Tracks is the only paginated-in-the-background node so far —
    /// mirrors `sources_spotify::SpotifySource::liked`.
    liked: PagedList,
}

impl SoundcloudSource {
    pub fn new(configured_id: Option<String>, oauth_token: Option<String>, bus: Bus) -> Self {
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
            .ok_or_else(|| src_err("needs a SoundCloud login — set [soundcloud] oauth_token in config"))
    }

    fn api_get(&self, path: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
        let id = self.client_id()?;
        let url = format!("{API}{path}");
        log::debug!("soundcloud: GET {url} {query:?}");
        let mut req = self
            .client
            .get(&url)
            .query(query)
            .query(&[("client_id", id.as_str())]);
        if let Some(token) = &self.oauth_token {
            req = req.header("Authorization", format!("OAuth {token}"));
        }
        let resp = req.send().map_err(|e| src_err(format!("{path}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            log::debug!("soundcloud: GET {url} -> {status}");
        } else {
            // Every failing request logs here, not just the one summary
            // `core::search` prints once the whole `Source::search` call
            // returns — so a mid-search 429/500/etc. is visible immediately,
            // with the path that actually hit it.
            log::warn!("soundcloud: GET {url} -> {status}");
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // A stale scraped id — drop it so the next call re-scrapes.
            *self.cached_id.lock().unwrap() = None;
            log::warn!("soundcloud: client_id rejected ({status}), will re-scrape next call");
            return Err(src_err(format!("{path}: {status} (client_id rejected)")));
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| src_err(format!("{path}: {e}")))?;
        resp.json().map_err(|e| src_err(format!("{path}: bad json: {e}")))
    }

    fn track_by_id(&self, id: u64) -> Result<ApiTrack> {
        let v = self.api_get(&format!("/tracks/{id}"), &[])?;
        serde_json::from_value(v).map_err(|e| src_err(format!("track {id}: {e}")))
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

    /// The current user's own playlists, as `(id, title)`.
    /// MVP: first page only (up to 200) — mirrors `SpotifyApi::playlists`.
    fn playlists(&self) -> Result<Vec<(String, String)>> {
        self.require_auth()?;
        let v = self.api_get("/me/playlists", &[("limit", "200")])?;
        let Some(collection) = v.get("collection").and_then(|c| c.as_array()) else {
            return Ok(vec![]);
        };
        Ok(collection
            .iter()
            .filter_map(|item| serde_json::from_value::<ApiPlaylist>(item.clone()).ok())
            .map(|p| (p.id.to_string(), p.title))
            .collect())
    }

    /// A playlist's tracks. `representation=full` asks the API for full
    /// track objects inline instead of the truncated stubs a plain
    /// `/playlists/{id}` returns for large playlists.
    fn playlist_tracks(&self, id: &str) -> Result<Vec<SearchHit>> {
        let v = self.api_get(&format!("/playlists/{id}"), &[("representation", "full")])?;
        let playlist: ApiPlaylistDetail =
            serde_json::from_value(v).map_err(|e| src_err(format!("playlist {id}: {e}")))?;
        Ok(playlist.tracks.into_iter().filter_map(ApiTrack::into_hit).collect())
    }

    /// One page of `/me/track_likes`, for `core::PagedList`.
    /// MVP: offset/limit paging, not verified against a live token — the
    /// api-v2 collection shape (`collection` + `next_href`) is shared with
    /// `/search/tracks` above, which *is* verified live.
    fn likes_page(&self, offset: usize, limit: usize) -> std::result::Result<RemotePage, String> {
        self.require_auth().map_err(|e| e.to_string())?;
        let offset_s = offset.to_string();
        let limit_s = limit.to_string();
        let v = self
            .api_get("/me/track_likes", &[("offset", &offset_s), ("limit", &limit_s)])
            .map_err(|e| e.to_string())?;
        let collection = v.get("collection").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        let consumed = collection.len();
        let hits: Vec<SearchHit> = collection
            .into_iter()
            .filter_map(|item| serde_json::from_value::<ApiLike>(item).ok())
            .filter_map(|l| l.track)
            .filter_map(ApiTrack::into_hit)
            .collect();
        // api-v2 doesn't report a total on this endpoint the way Spotify's
        // `/me/tracks` does — an empty page is the only "done" signal, so
        // report a total that's never reached until then.
        let total = if consumed == 0 { offset } else { offset + consumed + 1 };
        Ok(RemotePage { hits, total, consumed })
    }
}

impl Source for SoundcloudSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn recognizes(&self, uri: &str) -> bool {
        crate::uri::recognizes(uri)
    }

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(SearchHit)) -> Result<()> {
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
            if let Some(hit) = t.into_hit() {
                sink(hit);
            }
        }
        Ok(())
    }

    fn resolve(&self, uri: &str) -> Result<SearchHit> {
        let r = TrackRef::parse(uri).ok_or_else(|| src_err(format!("not a SoundCloud track: {uri:?}")))?;
        self.track_ref(&r)?
            .into_hit()
            .ok_or_else(|| src_err("track has no playable stream"))
    }

    fn retry_browse(&self, node: &BrowseNode) {
        if matches!(node, BrowseNode::Path(id) if id == LIKED_TRACKS) {
            self.liked.retry();
        }
    }

    fn browse(&self, node: &BrowseNode, want: usize) -> Result<BrowsePage> {
        match node {
            // Root: "Liked Tracks" + the user's own playlists, both of
            // which need a login — no folders at all without one.
            BrowseNode::Root => {
                let mut folders = vec![];
                if self.oauth_token.is_some() {
                    folders.push((
                        "Liked Tracks".to_string(),
                        BrowseNode::Path(LIKED_TRACKS.to_string()),
                    ));
                    let playlists = self.playlists()?;
                    folders.extend(playlists.into_iter().map(|(id, name)| (name, BrowseNode::Path(id))));
                }
                Ok(BrowsePage {
                    title: "SoundCloud".to_string(),
                    tracks: vec![],
                    folders,
                    partial: false,
                    errored: false,
                })
            }
            BrowseNode::Path(id) if id == LIKED_TRACKS => {
                let src = self.clone();
                let (tracks, partial) = self.liked.snapshot(&self.bus, want, move |offset| {
                    src.likes_page(offset, LIKED_PAGE_SIZE)
                });
                Ok(BrowsePage {
                    title: "Liked Tracks".to_string(),
                    tracks,
                    folders: vec![],
                    partial,
                    errored: self.liked.errored(),
                })
            }
            // A playlist id: its tracks. MVP: no user/charts browsing yet.
            BrowseNode::Path(id) => {
                let tracks = self.playlist_tracks(id)?;
                Ok(BrowsePage {
                    title: id.clone(),
                    tracks,
                    folders: vec![],
                    partial: false,
                    errored: false,
                })
            }
        }
    }
}

impl MediaProvider for SoundcloudSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn open(&self, r: &Rendition) -> Result<Media> {
        let track_ref = TrackRef::parse(&r.uri)
            .ok_or_else(|| src_err(format!("not a SoundCloud track: {:?}", r.uri)))?;
        let track = self.track_ref(&track_ref)?;

        // Pick the progressive (plain-file) transcoding; the player downloads it.
        let prog = track
            .media
            .transcodings
            .iter()
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

#[derive(Deserialize)]
struct ApiTrack {
    id: u64,
    title: String,
    #[serde(default)]
    duration: u32, // ms
    #[serde(default)]
    user: Option<ApiUser>,
    #[serde(default)]
    publisher_metadata: Option<ApiPublisher>,
    #[serde(default)]
    media: ApiMedia,
}

#[derive(Deserialize, Default)]
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

/// `GET /playlists/{id}?representation=full` — full track objects inline.
#[derive(Deserialize)]
struct ApiPlaylistDetail {
    #[serde(default)]
    tracks: Vec<ApiTrack>,
}

/// A `/me/track_likes` collection entry.
#[derive(Deserialize)]
struct ApiLike {
    #[serde(default)]
    track: Option<ApiTrack>,
}

#[derive(Deserialize)]
struct ApiPublisher {
    #[serde(default)]
    isrc: Option<String>,
}

#[derive(Deserialize, Default)]
struct ApiMedia {
    #[serde(default)]
    transcodings: Vec<ApiTranscoding>,
}

#[derive(Deserialize)]
struct ApiTranscoding {
    url: String,
    format: ApiFormat,
}

#[derive(Deserialize)]
struct ApiFormat {
    #[serde(default)]
    protocol: String,
}

impl ApiTrack {
    fn into_hit(self) -> Option<SearchHit> {
        if self.media.transcodings.is_empty() {
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
        Some(SearchHit {
            source: source_id(),
            uri: format!("soundcloud:track:{}", self.id),
            title,
            artists,
            duration_ms: self.duration,
            isrc,
            album: None,
            quality: Quality::Lossy { kbps: None },
        })
    }
}
