//! `SpotifySource` — a `core::Source` over the Spotify Web API.
//!
//! Not a `MediaProvider`: Spotify audio is a decrypted librespot stream, not a
//! fetchable file or URL, so playback is owned entirely by
//! [`crate::SpotifyPlayer`] and nothing is registered in `app`'s `media` map
//! for `"spotify"` (`Media` has no "the player handles it" variant).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use core::{
    BrowseNode, BrowsePage, Bus, Error, PagedList, RemotePage, Result, SearchHit, SearchQuery, Source,
    SourceId,
};

use crate::uri::{ItemKind, SpotifyRef};
use crate::webapi::WebApi;

/// `BrowseNode::Path` sentinel for the synthetic "Liked Songs" folder — not a
/// real Spotify id (those are base-62 alphanumeric), so it can't collide with
/// one.
const LIKED_SONGS: &str = "__liked__";

/// Page size for walking `/v1/me/tracks` — the API's max for this endpoint.
const LIKED_PAGE_SIZE: usize = 50;

/// Page size for walking a playlist's `/items` — the API's max for this
/// endpoint.
const PLAYLIST_PAGE_SIZE: usize = 100;

/// Page size for walking an album's `/tracks` — capped at 50, the max id
/// count accepted by the several-tracks lookup `WebApi::album_tracks_page`
/// uses to fill in each simplified track's full metadata (album, ISRC).
const ALBUM_PAGE_SIZE: usize = 50;

/// Page size for walking `/me/playlists` — the API's max for this endpoint.
const PLAYLISTS_PAGE_SIZE: usize = 50;

/// `BrowseNode::Path` prefix marking an album id, distinguishing it from a
/// bare playlist id in the same `Path` variant — safe because real Spotify
/// ids are base-62 alphanumeric and never contain `:`.
const ALBUM_PREFIX: &str = "album:";

pub struct SpotifySource {
    api: WebApi,
    bus: Bus,
    /// See `core::PagedList`.
    liked: PagedList<SearchHit>,
    /// One `PagedList` per playlist id browsed so far, created on first
    /// `browse` of that id — a playlist can exceed `PLAYLIST_PAGE_SIZE`
    /// tracks just like Liked Songs can exceed `LIKED_PAGE_SIZE`. Keyed by
    /// id rather than a single field because, unlike Liked Songs, there are
    /// many playlists and any of them may be browsed.
    playlists: Mutex<HashMap<String, PagedList<SearchHit>>>,
    /// One `PagedList` per album id browsed so far, keyed by the bare album
    /// id (without `ALBUM_PREFIX`) — same rationale as `playlists`.
    albums: Mutex<HashMap<String, PagedList<SearchHit>>>,
    /// The current user's own `/me/playlists` folder list, paged in the
    /// background the same way `liked`/`albums` are — a large library can
    /// have hundreds of playlists, and this walk runs under the same
    /// `RemoteCtx`-driven `browse` call the UI's Playlists screen makes on
    /// every redraw, so it must never block.
    folders: PagedList<(String, BrowseNode)>,
}

impl SpotifySource {
    pub fn new(access_token: impl Into<String>, cache_dir: PathBuf, bus: Bus) -> Self {
        Self {
            api: WebApi::new(access_token, cache_dir),
            bus,
            liked: PagedList::new("spotify: liked songs"),
            playlists: Mutex::new(HashMap::new()),
            albums: Mutex::new(HashMap::new()),
            folders: PagedList::new("spotify: playlists"),
        }
    }
}

fn src_err(message: String) -> Error {
    Error::Source {
        src: crate::source_id(),
        message,
    }
}

/// Bare base-62 track id out of a `spotify:track:...` rendition uri, for the
/// `/v1/me/tracks` (Liked Songs) endpoints, which take raw ids rather than URIs.
fn track_id_from_uri(track_uri: &str) -> Result<String> {
    crate::uri::SpotifyRef::parse(track_uri)
        .map(|r| r.id)
        .ok_or_else(|| src_err(format!("not a spotify track uri: {track_uri}")))
}

impl Source for SpotifySource {
    fn id(&self) -> SourceId {
        crate::source_id()
    }

    fn recognizes(&self, uri: &str) -> bool {
        crate::uri::recognizes(uri)
    }

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(SearchHit)) -> Result<()> {
        if q.text.trim().is_empty() {
            return Ok(());
        }
        let limit = if q.limit == 0 { 20 } else { q.limit };
        let hits = self.api.search_tracks(&q.text, limit).map_err(src_err)?;
        for hit in hits {
            sink(hit);
        }
        Ok(())
    }

    fn resolve(&self, uri: &str) -> Result<SearchHit> {
        let r = SpotifyRef::parse(uri).ok_or_else(|| src_err(format!("not a spotify uri: {uri}")))?;
        if r.kind != ItemKind::Track {
            // MVP: only track URIs resolve to a playable hit.
            return Err(src_err(format!("unsupported spotify item kind: {:?}", r.kind)));
        }
        self.api.track(&r.id).map_err(src_err)
    }

    fn browse_uri(&self, uri: &str) -> Option<BrowseNode> {
        let r = SpotifyRef::parse(uri)?;
        match r.kind {
            // Any playlist id works here, not just ones under `/me/playlists`
            // — `GET /v1/playlists/{id}/items` (see `WebApi::playlist_tracks_page`)
            // succeeds for a public playlist with any valid bearer token, no
            // extra scope or separate app-only credential needed.
            ItemKind::Playlist => Some(BrowseNode::Path(r.id)),
            // Same story for `GET /v1/albums/{id}/tracks` — any album id, no
            // extra scope needed.
            ItemKind::Album => Some(BrowseNode::Path(format!("{ALBUM_PREFIX}{}", r.id))),
            _ => None,
        }
    }

    fn retry_browse(&self, node: &BrowseNode) {
        match node {
            BrowseNode::Path(id) if id == LIKED_SONGS => self.liked.retry(),
            BrowseNode::Path(id) => match id.strip_prefix(ALBUM_PREFIX) {
                Some(album_id) => {
                    if let Some(list) = self.albums.lock().unwrap().get(album_id) {
                        list.retry();
                    }
                }
                None => {
                    if let Some(list) = self.playlists.lock().unwrap().get(id) {
                        list.retry();
                    }
                }
            },
            BrowseNode::Root => self.folders.retry(),
        }
    }

    fn add_to_playlist(&self, node: &BrowseNode, track_uri: &str) -> Result<()> {
        match node {
            // Liked Songs isn't a real playlist id — it's `/v1/me/tracks`, a
            // different endpoint pair (PUT/DELETE, raw track ids not URIs).
            BrowseNode::Path(id) if id == LIKED_SONGS => {
                self.api.save_track(&track_id_from_uri(track_uri)?).map_err(src_err)
            }
            BrowseNode::Path(id) if id.starts_with(ALBUM_PREFIX) => {
                Err(Error::Unsupported("add_to_playlist: albums are read-only"))
            }
            BrowseNode::Path(id) => self.api.add_playlist_track(id, track_uri).map_err(src_err),
            BrowseNode::Root => Err(Error::Unsupported("add_to_playlist: not a playlist")),
        }
    }

    fn remove_from_playlist(&self, node: &BrowseNode, track_uri: &str) -> Result<()> {
        match node {
            BrowseNode::Path(id) if id == LIKED_SONGS => {
                self.api.remove_saved_track(&track_id_from_uri(track_uri)?).map_err(src_err)
            }
            BrowseNode::Path(id) if id.starts_with(ALBUM_PREFIX) => {
                Err(Error::Unsupported("remove_from_playlist: albums are read-only"))
            }
            BrowseNode::Path(id) => self.api.remove_playlist_track(id, track_uri).map_err(src_err),
            BrowseNode::Root => Err(Error::Unsupported("remove_from_playlist: not a playlist")),
        }
    }

    fn is_synthetic(&self, node: &BrowseNode) -> bool {
        matches!(node, BrowseNode::Path(id) if id == LIKED_SONGS)
    }

    fn liked_songs_node(&self) -> Option<BrowseNode> {
        Some(BrowseNode::Path(LIKED_SONGS.to_string()))
    }

    fn browse(&self, node: &BrowseNode, want: usize) -> Result<BrowsePage> {
        match node {
            // Root: "Liked Songs" (synthetic, prepended outside the paged
            // walk) first, then the current user's own playlists, paged in
            // the background the same way Liked Songs' tracks are.
            BrowseNode::Root => {
                let api = self.api.clone();
                let (playlists, partial) = self.folders.snapshot(&self.bus, want, move |offset| {
                    api.playlists_page(offset, PLAYLISTS_PAGE_SIZE).map(|page| RemotePage {
                        total: page.total,
                        consumed: page.consumed,
                        hits: page
                            .hits
                            .into_iter()
                            .map(|(id, name)| (name, BrowseNode::Path(id)))
                            .collect(),
                    })
                });
                let mut folders = vec![(
                    "Liked Songs".to_string(),
                    BrowseNode::Path(LIKED_SONGS.to_string()),
                )];
                folders.extend(playlists);
                Ok(BrowsePage {
                    title: "spotify".to_string(),
                    tracks: vec![],
                    folders,
                    partial,
                    errored: self.folders.errored(),
                })
            }
            BrowseNode::Path(id) if id == LIKED_SONGS => {
                let api = self.api.clone();
                let (tracks, partial) = self.liked.snapshot(&self.bus, want, move |offset| {
                    api.saved_tracks_page(offset, LIKED_PAGE_SIZE)
                });
                Ok(BrowsePage {
                    title: "Liked Songs".to_string(),
                    tracks,
                    folders: vec![],
                    partial,
                    errored: self.liked.errored(),
                })
            }
            // An album id: its tracks.
            BrowseNode::Path(id) if id.starts_with(ALBUM_PREFIX) => {
                let album_id = id.strip_prefix(ALBUM_PREFIX).unwrap().to_string();
                let list = self
                    .albums
                    .lock()
                    .unwrap()
                    .entry(album_id.clone())
                    .or_insert_with(|| PagedList::new(format!("spotify: album {album_id}")))
                    .clone();
                let api = self.api.clone();
                let (tracks, partial) = list.snapshot(&self.bus, want, move |offset| {
                    api.album_tracks_page(&album_id, offset, ALBUM_PAGE_SIZE)
                });
                Ok(BrowsePage {
                    title: id.clone(),
                    tracks,
                    folders: vec![],
                    partial,
                    errored: list.errored(),
                })
            }
            // A playlist id: its tracks.
            BrowseNode::Path(id) => {
                let list = self
                    .playlists
                    .lock()
                    .unwrap()
                    .entry(id.clone())
                    .or_insert_with(|| PagedList::new(format!("spotify: playlist {id}")))
                    .clone();
                let api = self.api.clone();
                let playlist_id = id.clone();
                let (tracks, partial) = list.snapshot(&self.bus, want, move |offset| {
                    api.playlist_tracks_page(&playlist_id, offset, PLAYLIST_PAGE_SIZE)
                });
                Ok(BrowsePage {
                    title: id.clone(),
                    tracks,
                    folders: vec![],
                    partial,
                    errored: list.errored(),
                })
            }
        }
    }
}
