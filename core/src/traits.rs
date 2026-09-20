//! Core traits and error type.

use std::io::{Read, Seek};

use crate::types::{
    ItemKind, Playlist, PlaylistId, Rendition, SearchQuery, SourceId, Track, TrackId,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("store error: {0}")]
    Store(String),
    #[error("source error [{src}]: {message}")]
    Source { src: SourceId, message: String },
    #[error("no source for uri: {0}")]
    NoSource(String),
    #[error("resolve gap: {0}")]
    Gap(String),
    #[error("{0}")]
    Other(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BrowseNode {
    Root,
    Path(String),
}

#[derive(Clone, Debug)]
pub struct BrowsePage {
    pub title: String,
    pub tracks: Vec<Track>,
    pub folders: Vec<(String, BrowseNode)>,
    /// `true` if `tracks` is a prefix of the real list and more may still
    /// land on a later `browse` call (a background fetch in progress) —
    /// `Session::remote_playlist_tracks` keeps re-browsing (and only
    /// ingesting the new tail) while this is set, and only freezes its cache
    /// once a call comes back `false`. Sources that always return the whole
    /// list in one call (the common case) just set this to `false`.
    pub partial: bool,
    /// `true` if the underlying page walk stopped because a page fetch
    /// failed (rate limit, network) rather than because it reached the real
    /// end of the list — `tracks` is then a truncated prefix even though
    /// `partial` is `false` (so callers stop re-`browse`-ing it). Callers
    /// holding a previously cached longer copy must not let this truncated
    /// result replace it. Sources that can't fail partway through (or
    /// always return the whole list in one call) just set this to `false`.
    pub errored: bool,
}

pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;
    /// True if this source can handle a pasted URI/URL.
    fn recognizes(&self, uri: &str) -> bool;
    /// Blocking. Push hits into `sink` as they are found; return when done.
    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(Track)) -> Result<()>;
    /// Blocking. Push albums/playlists of `kind` matching `q` as (display name, node); default finds none.
    fn search_collections(
        &self,
        _q: &SearchQuery,
        _kind: ItemKind,
        _sink: &mut dyn FnMut(String, BrowseNode),
    ) -> Result<()> {
        Ok(())
    }
    /// Blocking, paced by `want` like `browse`: the user's saved albums as `BrowsePage.folders`; default has none.
    fn saved_albums(&self, _want: usize) -> Result<BrowsePage> {
        Ok(BrowsePage { title: String::new(), tracks: vec![], folders: vec![], partial: false, errored: false })
    }
    fn resolve(&self, uri: &str) -> Result<Track>;
    /// `want`: how many rows the caller needs ready now (paginated sources
    /// use it to pace background fetching — see `core::PagedList`); ignore
    /// if not paginated.
    fn browse(&self, node: &BrowseNode, want: usize) -> Result<BrowsePage>;
    /// Turn a recognized URI into a `BrowseNode` for `browse`, for URIs that
    /// point at a browsable collection rather than a single track (e.g. a
    /// pasted playlist link) — `resolve` can't represent those, since it
    /// returns exactly one `Track`. Default: no such mapping.
    fn browse_uri(&self, _uri: &str) -> Option<BrowseNode> {
        None
    }
    /// Reset any internal paged-fetch state for `node` that froze on a
    /// fetch error (see `BrowsePage::errored`), so the next `browse` call
    /// resumes the walk instead of handing back the same stale error —
    /// called when a caller's demand (e.g. scrolling to the loaded tail)
    /// asks for more than a frozen, truncated result has. No-op for a
    /// source with no such state (the common case).
    fn retry_browse(&self, _node: &BrowseNode) {}

    /// Add `track_uri` (this source's own rendition URI for the track) to
    /// playlist `node`. Blocking, like `browse` — callers must not run this
    /// on the UI thread. Default: unsupported, mirroring `Player::
    /// open_for_scan`'s default.
    fn add_to_playlist(&self, _node: &BrowseNode, _track_uri: &str) -> Result<()> {
        Err(Error::Unsupported("add_to_playlist"))
    }

    /// Remove `track_uri` from playlist `node`: only the occurrence at `position` when given (it
    /// must still be that track, else an error), every occurrence otherwise. Blocking, like
    /// `browse`. Default: unsupported.
    fn remove_from_playlist(&self, _node: &BrowseNode, _track_uri: &str, _position: Option<usize>) -> Result<()> {
        Err(Error::Unsupported("remove_from_playlist"))
    }

    /// Drop any cached copy of playlist `node`, so the next `browse` fetches it afresh. Default: nothing cached.
    fn forget_playlist(&self, _node: &BrowseNode) {}

    /// True for a source-specific synthetic browse folder that isn't a real
    /// user playlist (e.g. Spotify's "Liked Songs", backed by `/me/tracks`
    /// rather than a playlist id) — features that only make sense for real
    /// playlists (e.g. the hotkey-playlists column) exclude these. Default:
    /// no synthetic folders.
    fn is_synthetic(&self, _node: &BrowseNode) -> bool {
        false
    }

    /// The `BrowseNode` for this source's "liked songs"/favorites synthetic
    /// playlist (e.g. Spotify's `LIKED_SONGS`), if it has one — `l`/`L`'s
    /// hardwired add/remove target, discovered per-source rather than bound
    /// to a user hotkey (see `Session::liked_targets`). Default: none.
    fn liked_songs_node(&self) -> Option<BrowseNode> {
        None
    }

    /// Whether a track added to `node` shows up at the head of its listing rather than the tail
    /// (Spotify's Liked Songs lists newest first). Default: appended.
    fn adds_first(&self, _node: &BrowseNode) -> bool {
        false
    }
}

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

pub enum Media {
    Path(std::path::PathBuf),
    Url(String),
    Reader(Box<dyn ReadSeek + Send>),
}

pub trait MediaProvider: Send + Sync {
    fn id(&self) -> SourceId;
    fn open(&self, r: &Rendition) -> Result<Media>;

    /// Ensure `r`'s audio is fetched and return it as a readable stream,
    /// without indicating playback — the scan "prefetch" primitive
    /// (`ScanFetchMode::Full`/`Partial`) a background walk uses to get raw
    /// bytes to decode. Never call this in a mode meant to avoid a live
    /// fetch (`ScanFetchMode::CacheOnly`) — it always resolves via `open`,
    /// so a `Media::Url` result always originates a real GET.
    ///
    /// Default: resolves via `open` and, for `Media::Url`, does a plain
    /// one-shot GET (`crate::http_fetch`) — covers every provider that has
    /// no cheaper way to materialize its own bytes. A provider that already
    /// owns a client for its own API calls (e.g. an authenticated CDN) may
    /// override this to reuse it instead.
    fn materialize(&self, r: &Rendition) -> Result<Box<dyn ReadSeek + Send>> {
        match self.open(r)? {
            Media::Path(p) => {
                std::fs::File::open(&p).map(|f| Box::new(f) as Box<dyn ReadSeek + Send>).map_err(|e| {
                    Error::Other(format!("materialize: local path {} unreadable: {e}", p.display()))
                })
            }
            Media::Url(url) => crate::http_fetch::fetch_url_bytes(&url)
                .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn ReadSeek + Send>)
                .map_err(|e| Error::Other(format!("materialize: fetch of {url} failed: {e}"))),
            Media::Reader(r) => Ok(r),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug)]
pub struct PlayerStatus {
    pub state: PlayerState,
    pub position_ms: u32,
    pub duration_ms: u32,
    /// 0.0..=1.0
    pub volume: f32,
}

impl Default for PlayerStatus {
    fn default() -> Self {
        Self {
            state: PlayerState::Stopped,
            position_ms: 0,
            duration_ms: 0,
            volume: 1.0,
        }
    }
}

/// A Player is routed to by `Rendition::source` and works purely in terms of the
/// rendition. It has no notion of a logical `TrackId`.
pub trait Player: Send + Sync {
    fn accepts(&self, r: &Rendition) -> bool;
    /// `cache`: whether this play may populate `MediaCache`. Unused today —
    /// every caller passes `true`.
    fn load(&self, r: &Rendition, start_paused: bool, position_ms: u32, cache: bool);
    /// Warm up `r` so a following `load` of it starts without a gap.
    fn preload(&self, _r: &Rendition) {}
    fn toggle(&self);
    fn seek(&self, position_ms: u32);
    /// clamp 0.0..=1.0
    fn set_volume(&self, v: f32);
    fn stop(&self);
    fn status(&self) -> PlayerStatus;
    /// 5-band magnitude of whatever's actually playing right now, roughly
    /// `0.0..=1.0` (bass..treble) — real signal for `:vis 2`, not a volume
    /// proxy. Default: silence, for players with no tap on the raw audio
    /// (`NullPlayer`, and Spotify's librespot pipeline).
    fn levels(&self) -> [f32; 5] {
        [0.0; 5]
    }

    /// True while a live `open_for_scan` fetch would compete with this player's own playback.
    fn scan_fetch_paused(&self) -> bool {
        false
    }

    /// Best-effort seekable audio for offline analysis, independent of the
    /// live playback pipeline (starting a scan must never interrupt or race
    /// what's currently playing). Default: unsupported. Spotify implements
    /// this by porting the bpm-audio-dump branch's cache-first /
    /// CDN-fallback / decrypt path.
    ///
    /// `mode`: see `crate::scan::ScanFetchMode`. `Full` drains the rest of
    /// the track after the plugin reads it so the backend commits the
    /// *entire* file to its local cache rather than just the bytes actually
    /// consumed; `Partial` doesn't force that drain. `open_scan_audio` never
    /// calls this with `CacheOnly` — that mode only ever reads `MediaCache`
    /// (see its doc) — but a source may still call this itself with
    /// `CacheOnly` outside that path, e.g. to populate `MediaCache` from
    /// its own already-local bytes without risking a fetch.
    fn open_for_scan(
        &self,
        _r: &Rendition,
        _mode: crate::scan::ScanFetchMode,
    ) -> Result<Box<dyn ReadSeek + Send>> {
        Err(Error::Unsupported("open_for_scan"))
    }
}

pub trait Store: Send + Sync {
    fn upsert_track(&self, t: &Track) -> Result<()>;
    fn get_track(&self, id: TrackId) -> Result<Option<Track>>;
    fn all_tracks(&self) -> Result<Vec<Track>>;
    fn track_by_isrc(&self, isrc: &str) -> Result<Option<Track>>;
    fn track_by_rendition(&self, source: &SourceId, uri: &str) -> Result<Option<Track>>;
    /// Tracks whose title, after `Matcher::norm`, equals `norm_title` — the
    /// candidate set for `Catalog::ingest`'s fuzzy-match fallback. Indexed,
    /// so this stays cheap (a handful of same-titled tracks) regardless of
    /// library size, unlike scanning `all_tracks()`.
    fn tracks_by_title_norm(&self, norm_title: &str) -> Result<Vec<Track>>;
    fn delete_track(&self, id: TrackId) -> Result<()>;

    fn upsert_playlist(&self, p: &Playlist) -> Result<()>;
    fn get_playlist(&self, id: PlaylistId) -> Result<Option<Playlist>>;
    fn all_playlists(&self) -> Result<Vec<Playlist>>;
    fn delete_playlist(&self, id: PlaylistId) -> Result<()>;

    /// `get_track` for many ids in one go — one transaction instead of one
    /// per id. Missing ids are silently skipped (not an error): callers
    /// (e.g. hydrating a remote playlist's cache from a persisted id list)
    /// already treat "not in the store" as "drop it", same as `get_track`
    /// returning `None`.
    fn get_tracks(&self, ids: &[TrackId]) -> Result<Vec<Track>> {
        Ok(ids.iter().filter_map(|id| self.get_track(*id).ok().flatten()).collect())
    }

    /// Ordered track ids for a remote browse node (e.g. Spotify Liked
    /// Songs), so a slow paginated walk doesn't need to restart from
    /// scratch every session just to redisplay what's already known —
    /// see `Session::ensure_remote_playlist_tracks`. `key` is an opaque
    /// string built from `(SourceId, BrowseNode)`.
    fn remote_playlist_ids(&self, key: &str) -> Result<Vec<TrackId>>;
    fn set_remote_playlist_ids(&self, key: &str, ids: &[TrackId]) -> Result<()>;

    /// A source's top-level playlist-folder list (name, path id), so
    /// reopening the Playlists screen shows what's already known instead of
    /// an empty list while `ViewCache::ensure_remote_playlists`'s background
    /// `browse(Root)` refresh is in flight. `BrowseNode` has no
    /// `Serialize`/`Deserialize` derive, and a folder is always
    /// `BrowseNode::Path` in practice, so the id is stored raw and
    /// reconstructed into `BrowseNode::Path` on read.
    fn remote_playlist_folders(&self, source: &str) -> Result<Vec<(String, String)>>;
    fn set_remote_playlist_folders(&self, source: &str, folders: &[(String, String)]) -> Result<()>;
}
