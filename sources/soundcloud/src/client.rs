//! The SoundCloud API client + `Source` / `MediaProvider` impls.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::{
    Bus, BrowseNode, BrowsePage, Error, Media, MediaProvider, PagedList, Quality, RateLimiter,
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

/// Page size for walking `/me/track_likes` — comfortably under any
/// documented api-v2 limit cap.
const LIKED_PAGE_SIZE: usize = 50;

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
    /// Liked Tracks is the only paginated-in-the-background node so far —
    /// mirrors `sources_spotify::SpotifySource::liked`.
    liked: PagedList<Track>,
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
            .ok_or_else(|| src_err("needs a SoundCloud login — set [soundcloud] oauth_token in config"))
    }

    fn api_get(&self, path: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
        let id = self.client_id()?;
        let url = format!("{API}{path}");
        log::debug!("soundcloud: GET {url} {query:?}");
        self.limiter.throttle();
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
    fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let v = self.api_get(&format!("/playlists/{id}"), &[("representation", "full")])?;
        let playlist: ApiPlaylistDetail =
            serde_json::from_value(v).map_err(|e| src_err(format!("playlist {id}: {e}")))?;
        Ok(playlist.tracks.into_iter().filter_map(ApiTrack::into_track).collect())
    }

    /// One page of `/me/track_likes`, for `core::PagedList`.
    /// MVP: offset/limit paging, not verified against a live token — the
    /// api-v2 collection shape (`collection` + `next_href`) is shared with
    /// `/search/tracks` above, which *is* verified live.
    fn likes_page(&self, offset: usize, limit: usize) -> std::result::Result<RemotePage<Track>, String> {
        self.require_auth().map_err(|e| e.to_string())?;
        let offset_s = offset.to_string();
        let limit_s = limit.to_string();
        let v = self
            .api_get("/me/track_likes", &[("offset", &offset_s), ("limit", &limit_s)])
            .map_err(|e| e.to_string())?;
        let collection = v.get("collection").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        let consumed = collection.len();
        let hits: Vec<Track> = collection
            .into_iter()
            .filter_map(|item| serde_json::from_value::<ApiLike>(item).ok())
            .filter_map(|l| l.track)
            .filter_map(ApiTrack::into_track)
            .collect();
        // api-v2 doesn't report a total on this endpoint the way Spotify's
        // `/me/tracks` does — an empty page is the only "done" signal, so
        // report a total that's never reached until then.
        let total = if consumed == 0 { offset } else { offset + consumed + 1 };
        Ok(RemotePage { hits, total, consumed })
    }

    /// Best-quality HLS path: resolves the AAC-160k transcoding's signed playlist here, then returns a
    /// stream that appends the init segment and every media segment in order (one fMP4 `symphonia`
    /// decodes like any other). A setup failure is soft — `open` falls back to the progressive stream.
    fn open_hls(&self, track: &ApiTrack) -> Result<Media> {
        let hls = track
            .media
            .transcodings
            .iter()
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

    fn open(&self, r: &Rendition, _wanted: &dyn Fn() -> bool) -> Result<Media> {
        let track_ref = TrackRef::parse(&r.uri)
            .ok_or_else(|| src_err(format!("not a SoundCloud track: {:?}", r.uri)))?;
        let track = self.track_ref(&track_ref)?;

        if self.hls {
            match self.open_hls(&track) {
                Ok(media) => return Ok(media),
                Err(e) => log::debug!("soundcloud: HLS path unavailable, falling back to progressive: {e}"),
            }
        }

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
    permalink_url: Option<String>,
    #[serde(default)]
    duration: u32, // ms
    #[serde(default)]
    user: Option<ApiUser>,
    #[serde(default)]
    publisher_metadata: Option<ApiPublisher>,
    #[serde(default)]
    media: ApiMedia,
    #[serde(default)]
    waveform_url: Option<String>,
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
    #[serde(default)]
    preset: String,
}

#[derive(Deserialize, Default)]
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
        let rendition = Rendition::fresh(source_id(), format!("soundcloud:track:{}", self.id), self.duration, Quality::Lossy { kbps: None });
        let attrs = self
            .waveform_url
            .filter(|u| !u.is_empty())
            .map(|u| (WAVEFORM_URL_ATTR.to_string(), u))
            .into_iter()
            .collect();
        Some(Track { isrc, attrs, ..Track::fresh(title, artists, rendition) })
    }
}
