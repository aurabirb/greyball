//! `Session`'s UI-facing view state: search results and remote-playlist
//! browsing, both cached so the UI's render path (called every redraw/
//! keypress) never re-hits the store or a source's API just to draw the
//! same list again. Kept as its own type so `Session` stays data-access and
//! wiring, not view state.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::event::{Bus, CoreEvent, MembershipOutcome};
use crate::traits::{BrowseNode, BrowsePage, Error, Source, Store};
use crate::types::{ItemKind, SourceId, Track, TrackId};

/// Cache entry behind `ViewCache::remote_playlist_track_ids`/`_len`/`_window`.
/// `tracks` (already resolved from the store, not just ids — see
/// `ViewCache::ensure_remote_playlist_tracks`) is always a prefix of the real
/// list in the same order the source returns it in (append-only — sources
/// backing a `partial` `BrowsePage` must not reorder or replace earlier rows
/// across calls), so a later call only needs to resolve the tail past
/// `tracks.len()`.
struct RemotePlaylistTracks {
    tracks: Vec<Track>,
    /// `tracks`' ids, for cheap membership lookups.
    ids: HashSet<TrackId>,
    /// Bumped by `edit_tracks`, the one way `tracks`' membership or order changes.
    generation: u64,
    /// A background thread is already folding newly-landed raw hits into
    /// the catalog for this `(source, node)` — guards against starting a
    /// second one while `browse()` keeps handing back more in the
    /// meantime. `ensure_remote_playlist_tracks` picks up whatever's still
    /// unfolded the next time it's called after this clears.
    ingesting: bool,
    /// A background thread is already running `Source::browse` for this
    /// key — guards against spawning a second one (and a second blocking
    /// network call) while a redraw re-checks the cache before the first
    /// has landed. Cleared as soon as `browse()` returns, before the
    /// (possibly slower) ingest step.
    browsing: bool,
    partial: bool,
    /// `false` only for an entry hydrated from `Store::remote_playlist_ids`
    /// (a previous session's cache) that hasn't yet been checked against a
    /// live `browse()` — see `ensure_remote_playlist_tracks`. A freshly
    /// built entry needs no check, it's already live.
    revalidated: bool,
    /// `tracks.len()` as of the last `Store::set_remote_playlist_ids` write
    /// — persisting after every landed page would be one full-list rewrite
    /// per page (~82 for a 4100-track list); only persist once this drifts
    /// far enough, or the walk finishes.
    persisted_len: usize,
    /// `true` when the walk froze (`partial` went `false`) because a page
    /// fetch failed rather than because it reached the real end of the
    /// list — `tracks` may be a truncated prefix. While set, scrolling past
    /// the loaded tail (a `want` beyond `tracks.len()`) reopens the entry
    /// and asks the source to retry, instead of the demand going nowhere —
    /// unless `consecutive_failures` has already hit the bound below.
    errored: bool,
    /// Consecutive page-fetch failures, reset to 0 on any page that lands
    /// successfully. Once this reaches `MAX_CONSECUTIVE_PAGE_FAILURES`, a
    /// scroll-to-the-tail retry is no longer offered — a persistent `403`
    /// (e.g. no stored credential pair has access) isn't going to resolve
    /// itself, so retrying it on every redraw would just hammer the source
    /// and spam the log forever.
    consecutive_failures: u32,
    /// When the walk last froze on a failure, for `PLAYLISTS_RETRY_FLOOR`.
    failed_at: Option<Instant>,
}

impl RemotePlaylistTracks {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            ids: HashSet::new(),
            generation: 0,
            ingesting: false,
            browsing: false,
            partial: true,
            revalidated: true,
            persisted_len: 0,
            errored: false,
            consecutive_failures: 0,
            failed_at: None,
        }
    }

    fn edit_tracks(&mut self, edit: impl FnOnce(&mut Vec<Track>)) {
        edit(&mut self.tracks);
        self.ids = self.tracks.iter().map(|t| t.id).collect();
        self.generation += 1;
    }
}

/// One track's membership in one playlist, as a hotkey toggle sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Membership {
    Member,
    NotMember,
    PendingAdd,
    PendingRemove,
}

/// An in-flight remote add/remove; `add` starts as a guess off the loaded prefix.
struct PendingChange {
    track: Track,
    add: bool,
    /// The one row a removal targets; `None` is every occurrence.
    position: Option<usize>,
    /// An add lands at the head of the list, not the tail (`Source::adds_first`).
    front: bool,
}

/// What `set_remote_membership` does to a track: add it, or remove one row of it or every occurrence.
#[derive(Clone, Copy)]
pub enum Change {
    Add,
    Remove(Option<usize>),
}

/// What is dimmed on a remote playlist's rows: every occurrence of a track, or single rows.
#[derive(Default)]
pub struct PendingRows {
    pub tracks: HashSet<TrackId>,
    pub rows: HashSet<usize>,
}

impl PendingRows {
    pub fn has(&self, id: TrackId, row: usize) -> bool {
        self.tracks.contains(&id) || self.rows.contains(&row)
    }
}

type RemoteKey = (SourceId, BrowseNode);
type TracksCache = Arc<Mutex<HashMap<RemoteKey, RemotePlaylistTracks>>>;
type PendingChanges = Arc<Mutex<HashMap<RemoteKey, Vec<PendingChange>>>>;

/// How often a pending toggle re-checks whether its playlist finished loading.
/// Least gap between attempts to fetch a source's top-level playlists, so a dead endpoint isn't hammered.
const PLAYLISTS_RETRY_FLOOR: Duration = Duration::from_secs(30);
/// How often a partly landed top-level list is re-read while its source pages the rest in.
const PLAYLISTS_POLL: Duration = Duration::from_millis(300);
const MEMBERSHIP_POLL: Duration = Duration::from_millis(250);
/// A toggle gives up rather than wait forever on a playlist that never finishes loading.
const MEMBERSHIP_LOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// See `RemotePlaylistTracks::consecutive_failures` — matches the retry-bound
/// precedent set for Spotify's session-reconnect streak
/// (`sources/spotify/src/player.rs`'s `Link::died_streak`), though this is a
/// hard stop rather than a backoff: a `403` isn't going to change without
/// different credentials, so there's nothing to wait out.
const MAX_CONSECUTIVE_PAGE_FAILURES: u32 = 4;

/// How many un-persisted ids may accumulate before a page-landed write is
/// forced — bounds cache-staleness-on-crash against write count.
const PERSIST_BATCH: usize = 200;

/// Opaque `Store::remote_playlist_ids` key for a `(source, node)` pair.
fn remote_playlist_cache_key(source: &SourceId, node: &BrowseNode) -> String {
    match node {
        BrowseNode::Root => source.as_str().to_string(),
        BrowseNode::Path(id) => format!("{}:{id}", source.as_str()),
    }
}

/// Cache entry behind `ViewCache::remote_playlists`.
#[derive(Default)]
struct RemotePlaylistsEntry {
    folders: Vec<(String, BrowseNode)>,
    /// Bumped by `set_folders`, the one way `folders` changes.
    generation: u64,
    /// A background `Source::browse(Root, want)` for this source is in flight.
    browsing: bool,
    /// The last fetch ended cleanly with at least one real playlist; anything else is retried.
    settled: bool,
    /// When the last fetch started, for `PLAYLISTS_RETRY_FLOOR`.
    last_attempt: Option<Instant>,
}

impl RemotePlaylistsEntry {
    fn set_folders(&mut self, folders: Vec<(String, BrowseNode)>) {
        self.folders = folders;
        self.generation += 1;
    }
}

/// Borrowed handles the remote-playlist accessors need from `Session` —
/// bundled so their signatures stay short. Cheap to pass by value: every
/// field is itself a reference.
#[derive(Clone, Copy)]
pub(crate) struct RemoteCtx<'a> {
    pub sources: &'a HashMap<SourceId, Arc<dyn Source>>,
    pub store: &'a Arc<dyn Store>,
    pub catalog: &'a Arc<Catalog>,
    pub bus: &'a Bus,
}

/// Owned handles `ensure_remote_playlist_tracks`'s background thread and
/// `spawn_remote_playlist_ingest` both need — bundled so their signatures
/// stay short.
#[derive(Clone)]
struct RemotePlaylistDeps {
    catalog: Arc<Catalog>,
    store: Arc<dyn Store>,
    bus: Bus,
    cache: TracksCache,
}

/// Store-key suffix that keeps a source's saved albums apart from its playlist folders.
const ALBUMS_KEY_SUFFIX: &str = "#albums";

/// One folder-list fetch: log label, store key, the source call, and whether it is the `Root` browse (which resumes a frozen walk and settles only on a real playlist).
struct FolderWalk {
    what: &'static str,
    key: String,
    fetch: fn(&dyn Source) -> crate::Result<BrowsePage>,
    walks_root: bool,
}

/// What one search found, by the id `Search::run` gave it; tracks resolved once so a redraw never re-hits the store.
#[derive(Default)]
pub struct ResultSet {
    query: String,
    tracks: Vec<Track>,
    collections: Vec<(SourceId, ItemKind, String, BrowseNode)>,
    /// Bumped whenever `tracks` or `collections` gain an entry.
    generation: u64,
}

impl ResultSet {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn track_ids(&self) -> Vec<TrackId> {
        self.tracks.iter().map(|t| t.id).collect()
    }

    pub fn collections(&self) -> &[(SourceId, ItemKind, String, BrowseNode)] {
        &self.collections
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.collections.is_empty()
    }
}

#[derive(Default)]
pub(crate) struct ViewCache {
    /// One per live search; a window's set is dropped when it searches again or closes.
    results: HashMap<u64, ResultSet>,

    /// A source's top-level playlist folders, loaded in the background —
    /// see `ensure_remote_playlists`.
    remote_playlists: Arc<Mutex<HashMap<SourceId, RemotePlaylistsEntry>>>,
    /// A source's saved albums, fetched and persisted like `remote_playlists`.
    remote_albums: Arc<Mutex<HashMap<SourceId, RemotePlaylistsEntry>>>,
    /// A remote playlist's ingested tracks, cached per `(source, node)` —
    /// same "safe to call every redraw" requirement as `remote_playlists`.
    /// While `partial` a source may still be loading more in the background
    /// (see `BrowsePage::partial`), so the entry isn't trusted as final:
    /// every redraw re-`browse`s (cheap) and, if there's a newly-landed
    /// tail, kicks off a background thread to fold it into the catalog —
    /// `Arc`-wrapped so that thread can hold its own handle to the map
    /// (`ensure_remote_playlist_tracks`) — until a call comes back
    /// non-partial with nothing left to ingest and the entry freezes.
    remote_playlist_tracks: TracksCache,
    /// In-flight hotkey toggles per remote playlist — see `set_remote_membership`.
    pending: PendingChanges,
}

impl ViewCache {
    /// An empty set for the search `id`, to be filled by its events.
    pub fn begin_search(&mut self, id: u64, query: String) {
        self.results.insert(id, ResultSet { query, ..ResultSet::default() });
    }

    pub fn forget_search(&mut self, id: u64) {
        self.results.remove(&id);
    }

    pub fn results(&self, id: u64) -> Option<&ResultSet> {
        self.results.get(&id)
    }

    /// A remote playlist's track-list generation; 0 until first touched.
    pub fn remote_playlist_gen(&self, source: &SourceId, node: &BrowseNode) -> u64 {
        let cache = self.remote_playlist_tracks.lock().unwrap();
        cache.get(&(source.clone(), node.clone())).map_or(0, |e| e.generation)
    }

    /// Moves whenever any source's top-level playlist or saved-album folders do.
    pub fn remote_playlists_gen(&self) -> u64 {
        [&self.remote_playlists, &self.remote_albums]
            .iter()
            .map(|m| m.lock().unwrap().values().map(|e| e.generation).sum::<u64>())
            .sum()
    }

    /// False for a search no longer wanted: its hit is dropped.
    pub fn push_collection_result(&mut self, id: u64, source: SourceId, kind: ItemKind, name: String, node: BrowseNode) -> bool {
        let Some(set) = self.results.get_mut(&id) else { return false };
        set.collections.push((source, kind, name, node));
        set.generation += 1;
        true
    }

    /// Appends `tid` to search `id`'s tracks, deduped and resolved once; false for a search no longer wanted.
    pub fn push_result(&mut self, id: u64, tid: TrackId, store: &Arc<dyn Store>) -> bool {
        let Some(set) = self.results.get_mut(&id) else { return false };
        if set.tracks.iter().any(|t| t.id == tid) {
            return true;
        }
        if let Ok(Some(t)) = store.get_track(tid) {
            set.tracks.push(t);
            set.generation += 1;
        }
        true
    }

    /// Result sets and `remote_playlist_tracks` hold resolved `Track`s, so a `TrackUpdated` must patch every copy.
    pub fn refresh_cached_track(&mut self, id: TrackId, store: &Arc<dyn Store>) {
        let Ok(Some(fresh)) = store.get_track(id) else {
            return;
        };
        for t in self.results.values_mut().flat_map(|set| set.tracks.iter_mut()).filter(|t| t.id == id) {
            *t = fresh.clone();
        }
        for entry in self.remote_playlist_tracks.lock().unwrap().values_mut() {
            if let Some(t) = entry.tracks.iter_mut().find(|t| t.id == id) {
                *t = fresh.clone();
            }
        }
    }

    /// A source's landed top-level playlist folders — a pure read; `ensure_remote_playlists` fetches.
    pub fn remote_playlists(&self, source: &SourceId) -> Vec<(String, BrowseNode)> {
        self.remote_playlists
            .lock()
            .unwrap()
            .get(source)
            .map(|e| e.folders.clone())
            .unwrap_or_default()
    }

    /// A source's landed saved albums — a pure read; `ensure_remote_playlists` fetches.
    pub fn remote_albums(&self, source: &SourceId) -> Vec<(String, BrowseNode)> {
        self.remote_albums.lock().unwrap().get(source).map(|e| e.folders.clone()).unwrap_or_default()
    }

    /// Lets the next `ensure_remote_playlists` retry an unsettled fetch at once — the source's token or wiring changed.
    pub(crate) fn invalidate_remote_playlists(&self, source: &SourceId) {
        for map in [&self.remote_playlists, &self.remote_albums] {
            if let Some(entry) = map.lock().unwrap().get_mut(source) {
                entry.last_attempt = None;
            }
        }
    }

    /// Single-flight background `browse(Root)` refresh — no ingestion
    /// needed, a folder is just a name + `BrowseNode`, but a clean list is
    /// persisted so a restart shows it immediately (see
    /// `Store::remote_playlist_folders`). A failed or empty fetch stays
    /// retriable, at most once per `PLAYLISTS_RETRY_FLOOR`, and never
    /// replaces a longer list already known.
    pub(crate) fn ensure_remote_playlists(&self, source: &SourceId, ctx: RemoteCtx) {
        let playlists = FolderWalk {
            what: "playlists",
            key: source.to_string(),
            fetch: |src| src.browse(&BrowseNode::Root, usize::MAX),
            walks_root: true,
        };
        self.ensure_folders(&self.remote_playlists, source, ctx, playlists);
        if ctx.sources.get(source).is_some_and(|src| src.has_saved_albums()) {
            let albums = FolderWalk {
                what: "albums",
                key: format!("{source}{ALBUMS_KEY_SUFFIX}"),
                fetch: |src| src.saved_albums(usize::MAX),
                walks_root: false,
            };
            self.ensure_folders(&self.remote_albums, source, ctx, albums);
        }
    }

    /// The single-flight fetch behind `ensure_remote_playlists`, one walk per folder list.
    fn ensure_folders(
        &self,
        map: &Arc<Mutex<HashMap<SourceId, RemotePlaylistsEntry>>>,
        source: &SourceId,
        ctx: RemoteCtx,
        walk: FolderWalk,
    ) {
        let FolderWalk { what, key, fetch, walks_root } = walk;
        let Some(source_handle) = ctx.sources.get(source).cloned() else {
            return;
        };
        {
            let mut cache = map.lock().unwrap();
            let entry = cache.entry(source.clone()).or_insert_with(|| {
                // First touch this session — hydrate from whatever a
                // previous session persisted instead of starting empty.
                let folders = ctx
                    .store
                    .remote_playlist_folders(&key)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, id)| (name, BrowseNode::Path(id)))
                    .collect();
                let mut entry = RemotePlaylistsEntry::default();
                entry.set_folders(folders);
                entry
            });
            if entry.browsing
                || entry.settled
                || entry.last_attempt.is_some_and(|at| at.elapsed() < PLAYLISTS_RETRY_FLOOR)
            {
                return;
            }
            entry.browsing = true;
            entry.last_attempt = Some(Instant::now());
        }
        let source = source.clone();
        let bus = ctx.bus.clone();
        let cache = map.clone();
        let store = ctx.store.clone();

        std::thread::spawn(move || {
            if walks_root {
                // A previous failed walk froze itself; resume it.
                source_handle.retry_browse(&BrowseNode::Root);
            }
            let failure = loop {
                // Small list, no scroll position to pace against — load it all.
                let page = match fetch(source_handle.as_ref()) {
                    Ok(page) => page,
                    Err(e) => break Some(e.to_string()),
                };
                let clean = !page.partial && !page.errored;
                {
                    let mut cache = cache.lock().unwrap();
                    let entry = cache.entry(source.clone()).or_default();
                    if clean || page.folders.len() > entry.folders.len() {
                        entry.set_folders(page.folders.clone());
                    }
                    if clean {
                        entry.settled = !walks_root || page.folders.iter().any(|(_, node)| !source_handle.is_synthetic(node));
                    }
                }
                bus.send(CoreEvent::PlaylistsChanged);
                if clean {
                    let folders: Vec<(String, String)> = page
                        .folders
                        .iter()
                        .filter_map(|(name, node)| match node {
                            BrowseNode::Path(id) => Some((name.clone(), id.clone())),
                            BrowseNode::Root => None,
                        })
                        .collect();
                    if let Err(e) = store.set_remote_playlist_folders(&key, &folders) {
                        log::warn!("{source}: failed to persist {what} folders: {e}");
                    }
                    break None;
                }
                if page.errored {
                    break Some("fetch failed, will retry".to_string());
                }
                std::thread::sleep(PLAYLISTS_POLL);
            };
            cache.lock().unwrap().entry(source.clone()).or_default().browsing = false;
            if let Some(message) = failure {
                bus.send(CoreEvent::BackgroundFailure { context: source.to_string(), message: format!("{what}: {message}") });
            }
            bus.send(CoreEvent::PlaylistsChanged);
        });
    }

    /// All of a remote playlist's ingested track ids, cheap (reads straight
    /// off the already-resolved cache, no store hits) — for cursor bounds
    /// and `Command::PlayContext`.
    pub fn remote_playlist_track_ids(
        &self,
        source: &SourceId,
        node: &BrowseNode,
        ctx: RemoteCtx,
    ) -> Vec<TrackId> {
        // want=0: called on every keypress for cursor bounds, not just
        // Command::PlayContext — mustn't force full-speed loading itself.
        self.ensure_remote_playlist_tracks(source, node, 0, ctx);
        let (front, back) = self.pending_adds(source, node);
        let mut ids: Vec<TrackId> = front.iter().map(|t| t.id).collect();
        ids.extend(self.remote_playlist_cached(source, node, |e| e.tracks.iter().map(|t| t.id).collect::<Vec<_>>()).unwrap_or_default());
        ids.extend(back.iter().map(|t| t.id));
        ids
    }

    /// The walked ids of `(source, node)`, empty until a hydrated entry is revalidated; no pending adds, so indices are stable.
    pub fn remote_playlist_confirmed_ids(&self, source: &SourceId, node: &BrowseNode, ctx: RemoteCtx) -> Vec<TrackId> {
        self.ensure_remote_playlist_tracks(source, node, 0, ctx);
        self.remote_playlist_cached(source, node, |e| if e.revalidated { e.tracks.iter().map(|t| t.id).collect() } else { Vec::new() }).unwrap_or_default()
    }

    /// Cheap count of a remote playlist's ingested tracks so far.
    pub fn remote_playlist_len(&self, source: &SourceId, node: &BrowseNode, ctx: RemoteCtx) -> usize {
        self.ensure_remote_playlist_tracks(source, node, 0, ctx);
        let (front, back) = self.pending_adds(source, node);
        self.remote_playlist_cached(source, node, |e| e.tracks.len()).unwrap_or(0) + front.len() + back.len()
    }

    /// A window of a remote playlist's ingested tracks (`offset..offset+limit`)
    /// — only that slice is cloned out of the cache, never the whole thing.
    pub fn remote_playlist_window(
        &self,
        source: &SourceId,
        node: &BrowseNode,
        offset: usize,
        limit: usize,
        ctx: RemoteCtx,
    ) -> Vec<Track> {
        // Lookahead past what's visible so fast on-demand fetching kicks in
        // slightly before the user scrolls past the loaded edge.
        const WINDOW_MARGIN: usize = 50;
        self.ensure_remote_playlist_tracks(source, node, offset + limit + WINDOW_MARGIN, ctx);
        let (front, back) = self.pending_adds(source, node);
        let mut window: Vec<Track> = front.iter().skip(offset).take(limit).cloned().collect();
        let settled_skip = offset.saturating_sub(front.len());
        let (settled, settled_len): (Vec<Track>, usize) = self
            .remote_playlist_cached(source, node, |e| {
                (e.tracks.iter().skip(settled_skip).take(limit - window.len()).cloned().collect(), e.tracks.len())
            })
            .unwrap_or_default();
        window.extend(settled);
        let room = limit.saturating_sub(window.len());
        if room > 0 {
            let skip = offset.saturating_sub(front.len() + settled_len);
            window.extend(back.into_iter().skip(skip).take(room));
        }
        window
    }

    /// Tracks mid-add to `(source, node)` that the loaded list doesn't hold yet, as the rows
    /// pending ahead of the loaded list and those pending after it.
    fn pending_adds(&self, source: &SourceId, node: &BrowseNode) -> (Vec<Track>, Vec<Track>) {
        let key = (source.clone(), node.clone());
        let adds: Vec<(Track, bool)> = match self.pending.lock().unwrap().get(&key) {
            Some(changes) => changes.iter().filter(|c| c.add).map(|c| (c.track.clone(), c.front)).collect(),
            None => return (Vec::new(), Vec::new()),
        };
        let cache = self.remote_playlist_tracks.lock().unwrap();
        let loaded = cache.get(&key).map(|e| &e.ids);
        let (front, back): (Vec<_>, Vec<_>) = adds
            .into_iter()
            .filter(|(t, _)| !loaded.is_some_and(|ids| ids.contains(&t.id)))
            .partition(|(_, front)| *front);
        let tracks = |v: Vec<(Track, bool)>| v.into_iter().map(|(t, _)| t).collect();
        (tracks(front), tracks(back))
    }

    /// Tracks with an add or all-occurrence remove still in flight on `(source, node)`.
    pub fn remote_pending_ids(&self, source: &SourceId, node: &BrowseNode) -> Vec<TrackId> {
        self.remote_pending_rows(source, node).tracks.into_iter().collect()
    }

    /// Everything with a change still in flight on `(source, node)`: whole tracks, and rows a position removal targets.
    pub fn remote_pending_rows(&self, source: &SourceId, node: &BrowseNode) -> PendingRows {
        let key = (source.clone(), node.clone());
        let mut out = PendingRows::default();
        for change in self.pending.lock().unwrap().get(&key).into_iter().flatten() {
            match change.position {
                Some(row) => out.rows.insert(row),
                None => out.tracks.insert(change.track.id),
            };
        }
        out
    }

    /// How many times `track` is in `(source, node)` as far as loaded, 0 while a change to it is in flight.
    pub fn remote_occurrences(&self, source: &SourceId, node: &BrowseNode, track: TrackId) -> usize {
        if self.remote_membership(source, node, track) != Membership::Member {
            return 0;
        }
        self.remote_playlist_cached(source, node, |e| e.tracks.iter().filter(|t| t.id == track).count()).unwrap_or(0)
    }

    /// Pending state wins over the loaded list, so a press mid-request never reads as settled.
    fn remote_membership(&self, source: &SourceId, node: &BrowseNode, track: TrackId) -> Membership {
        let key = (source.clone(), node.clone());
        if let Some(change) =
            self.pending.lock().unwrap().get(&key).and_then(|c| c.iter().find(|c| c.track.id == track))
        {
            return if change.add { Membership::PendingAdd } else { Membership::PendingRemove };
        }
        let member = self
            .remote_playlist_cached(source, node, |e| e.ids.contains(&track))
            .unwrap_or(false);
        if member { Membership::Member } else { Membership::NotMember }
    }

    /// How `track` stands in the liked list `(source, node)`: `None` when absent, `Some(pending)`
    /// when it is in the list or a like/unlike of it is in flight.
    pub fn liked_mark(&self, source: &SourceId, node: &BrowseNode, track: TrackId) -> Option<bool> {
        let key = (source.clone(), node.clone());
        if self.pending.lock().unwrap().get(&key).is_some_and(|c| c.iter().any(|c| c.track.id == track)) {
            return Some(true);
        }
        self.remote_playlist_cached(source, node, |e| e.ids.contains(&track)).unwrap_or(false).then_some(false)
    }

    /// Starts (or resumes) loading `(source, node)` without demanding any of it; a walk frozen on
    /// a failure is reopened once `PLAYLISTS_RETRY_FLOOR` has passed.
    pub fn ensure_remote_playlist_loading(&self, source: &SourceId, node: &BrowseNode, ctx: RemoteCtx) {
        let retry_want = self
            .remote_playlist_cached(source, node, |e| {
                (e.errored && !e.partial && e.failed_at.is_some_and(|at| at.elapsed() >= PLAYLISTS_RETRY_FLOOR))
                    .then_some(e.tracks.len() + 1)
            })
            .flatten();
        self.ensure_remote_playlist_tracks(source, node, retry_want.unwrap_or(0), ctx);
    }

    /// Is a fetch of `source`'s top-level playlist-folder list in flight?
    pub fn remote_playlists_loading(&self, source: &SourceId) -> bool {
        [&self.remote_playlists, &self.remote_albums].iter().any(|m| m.lock().unwrap().get(source).is_some_and(|e| e.browsing))
    }

    /// Is more of `(source, node)` still loading? `true` before anything has landed.
    pub fn remote_playlist_loading(&self, source: &SourceId, node: &BrowseNode) -> bool {
        self.remote_playlist_cached(source, node, |e| e.partial).unwrap_or(true)
    }

    /// Did the walk of `(source, node)` freeze on a failed page?
    pub fn remote_playlist_errored(&self, source: &SourceId, node: &BrowseNode) -> bool {
        self.remote_playlist_cached(source, node, |e| e.errored && !e.partial).unwrap_or(false)
    }

    /// Shared by the `remote_playlist_*` accessors above: look up the cache
    /// entry for `(source, node)` and, if present, run `f` over it.
    fn remote_playlist_cached<T>(
        &self,
        source: &SourceId,
        node: &BrowseNode,
        f: impl FnOnce(&RemotePlaylistTracks) -> T,
    ) -> Option<T> {
        let key = (source.clone(), node.clone());
        self.remote_playlist_tracks.lock().unwrap().get(&key).map(f)
    }

    /// Change `track`'s membership of `(source, node)` on a background thread, once the whole list
    /// has loaded; a change that already holds sends nothing. `false` (nothing sent) when a change
    /// for this `(playlist, track)` is already in flight. A removal at a position is one at a time per playlist.
    pub fn set_remote_membership(
        &self,
        track: Track,
        uri: String,
        source: Arc<dyn Source>,
        node: BrowseNode,
        change: Change,
        ctx: RemoteCtx,
    ) -> bool {
        let key = (source.id(), node.clone());
        let front = source.adds_first(&node);
        let liked = source.liked_songs_node().as_ref() == Some(&node);
        let (want_add, position) = match change {
            Change::Add => (true, None),
            Change::Remove(position) => (false, position),
        };
        {
            let mut pending = self.pending.lock().unwrap();
            let changes = pending.entry(key.clone()).or_default();
            let clash = |c: &PendingChange| {
                (c.track.id == track.id && (c.position.is_none() || position.is_none()))
                    || (c.position.is_some() && position.is_some())
            };
            if changes.iter().any(clash) {
                return false;
            }
            changes.push(PendingChange { track: track.clone(), add: want_add, position, front });
        }
        let deps = RemotePlaylistDeps {
            catalog: ctx.catalog.clone(),
            store: ctx.store.clone(),
            bus: ctx.bus.clone(),
            cache: self.remote_playlist_tracks.clone(),
        };
        let pending = self.pending.clone();
        deps.bus.send(CoreEvent::PlaylistsChanged);

        std::thread::spawn(move || {
            let name = track.display_name();
            let outcome = Self::await_loaded_membership(&deps, &source, &key, track.id).and_then(|member| {
                let add = want_add;
                let result = match (add, member) {
                    (true, true) | (false, false) => Ok(()),
                    (true, false) => source.add_to_playlist(&node, &uri),
                    (false, true) if Self::row_holds(&deps, &key, position, track.id) => {
                        source.remove_from_playlist(&node, &uri, position)
                    }
                    (false, true) => Err(Error::Other("the playlist changed under that row".to_string())),
                };
                result.map(|()| add)
            });
            let mut reload = position.is_some() && matches!(&outcome, Err(e) if !matches!(e, Error::Unsupported(_)));
            let ids: Option<Vec<TrackId>> = {
                // Both locks held so no redraw sees the track neither pending nor settled.
                let mut pending = pending.lock().unwrap();
                let mut cache = deps.cache.lock().unwrap();
                if let Some(changes) = pending.get_mut(&key) {
                    changes.retain(|c| !(c.track.id == track.id && c.position == position));
                }
                match (&outcome, cache.get_mut(&key)) {
                    (Ok(added), Some(entry)) => {
                        entry.edit_tracks(|tracks| {
                            if !*added {
                                match position {
                                    Some(row) if tracks.get(row).is_some_and(|t| t.id == track.id) => {
                                        tracks.remove(row);
                                    }
                                    Some(_) => reload = true,
                                    None => tracks.retain(|t| t.id != track.id),
                                }
                            } else if !tracks.iter().any(|t| t.id == track.id) {
                                if front {
                                    tracks.insert(0, track.clone());
                                } else {
                                    tracks.push(track.clone());
                                }
                            }
                        });
                        entry.persisted_len = entry.tracks.len();
                        Some(entry.tracks.iter().map(|t| t.id).collect())
                    }
                    _ => None,
                }
            };
            let cache_key = remote_playlist_cache_key(&key.0, &key.1);
            if reload {
                deps.cache.lock().unwrap().remove(&key);
                source.forget_playlist(&node);
                if let Err(e) = deps.store.set_remote_playlist_ids(&cache_key, &[]) {
                    log::warn!("remote playlist cache: failed to clear {cache_key}: {e}");
                }
                deps.catalog.warn(key.0.as_str(), format!("{cache_key}: the playlist changed, reloading it"));
            } else if let Some(ids) = ids
                && let Err(e) = deps.store.set_remote_playlist_ids(&cache_key, &ids)
            {
                log::warn!("remote playlist cache: failed to persist {cache_key}: {e}");
            }
            deps.bus.send(CoreEvent::PlaylistsChanged);
            let platform = key.0.label();
            let outcome = match outcome {
                Ok(true) if liked => MembershipOutcome::Changed(format!("Liked {name:?} on {platform}")),
                Ok(false) if liked => MembershipOutcome::Changed(format!("Removed {name:?} from Liked Songs")),
                Ok(true) => MembershipOutcome::Changed(format!("Added {name:?} to playlist")),
                Ok(false) => MembershipOutcome::Changed(format!("Removed {name:?} from playlist")),
                Err(e) => {
                    log::error!("set_remote_membership[{cache_key}]: {e}");
                    let what = match (liked, want_add) {
                        (false, _) => format!("Can't toggle {name:?}"),
                        (true, true) => format!("Couldn't like {name:?} on {platform}"),
                        (true, _) => format!("Couldn't unlike {name:?} on {platform}"),
                    };
                    MembershipOutcome::of_error(what, &e)
                }
            };
            deps.bus.send(if liked {
                CoreEvent::LikeResult { track: track.id, like: want_add, outcome }
            } else {
                CoreEvent::MembershipResult(outcome)
            });
        });
        true
    }

    /// Whether row `position` of `key`'s loaded list is `track` (always true without a position).
    fn row_holds(deps: &RemotePlaylistDeps, key: &RemoteKey, position: Option<usize>, track: TrackId) -> bool {
        let Some(row) = position else { return true };
        deps.cache.lock().unwrap().get(key).is_some_and(|e| e.tracks.get(row).is_some_and(|t| t.id == track))
    }

    /// Blocks (background threads only) until `key`'s list is fully loaded, then says whether it holds `track`.
    fn await_loaded_membership(
        deps: &RemotePlaylistDeps,
        source: &Arc<dyn Source>,
        key: &RemoteKey,
        track: TrackId,
    ) -> Result<bool, Error> {
        let sources = HashMap::from([(key.0.clone(), source.clone())]);
        let ctx = RemoteCtx { sources: &sources, store: &deps.store, catalog: &deps.catalog, bus: &deps.bus };
        let deadline = Instant::now() + MEMBERSHIP_LOAD_TIMEOUT;
        loop {
            Self::ensure_tracks(&deps.cache, &key.0, &key.1, usize::MAX, ctx);
            let loaded = deps.cache.lock().unwrap().get(key).and_then(|e| {
                (!e.partial && !e.browsing && !e.ingesting)
                    .then(|| (e.errored, e.tracks.iter().any(|t| t.id == track)))
            });
            match loaded {
                Some((false, member)) => return Ok(member),
                Some((true, _)) => return Err(Error::Other("the playlist didn't load completely".to_string())),
                None if Instant::now() >= deadline => {
                    return Err(Error::Other("timed out waiting for the playlist to load".to_string()));
                }
                None => std::thread::sleep(MEMBERSHIP_POLL),
            }
        }
    }

    fn ensure_remote_playlist_tracks(&self, source: &SourceId, node: &BrowseNode, want: usize, ctx: RemoteCtx) {
        Self::ensure_tracks(&self.remote_playlist_tracks, source, node, want, ctx);
    }

    /// Ensure `remote_playlist_tracks`'s cache entry for `(source, node)`
    /// reflects the latest `browse()` call. Cheap and safe to call on every
    /// redraw: the actual `Source::browse` call — which for a plain
    /// playlist is a single blocking HTTP request, and can otherwise stall
    /// for as long as a rate-limit cooldown — happens on a background
    /// thread (guarded by `browsing`, one at a time per key), never on the
    /// caller's thread (typically the UI's). Folding a newly-landed tail
    /// into the catalog (`Catalog::ingest`, like any other search hit, so it
    /// carries a real `TrackId` and play/queue/link all work on it
    /// normally) is likewise backgrounded (guarded by `ingesting`): `ingest`'s
    /// fuzzy-match fallback scans the *entire* library, so for a
    /// many-thousand-track list like Spotify Liked Songs this genuinely gets
    /// slower as more of it loads. This function only ever kicks the browse
    /// off and returns; the accessors below just read whatever's already in
    /// the cache, however far the background work has gotten — a landed
    /// change is announced via `CoreEvent::PlaylistsChanged` for the
    /// front-end to redraw and re-check.
    ///
    /// Only *frozen* (no more `browse`s) once a call comes back with
    /// `partial: false` *and* nothing is left to ingest, or with an error
    /// (a permanent failure, e.g. a 403 on a playlist we're not allowed to
    /// see, must not be retried on every redraw either).
    fn ensure_tracks(
        tracks_cache: &TracksCache,
        source: &SourceId,
        node: &BrowseNode,
        want: usize,
        ctx: RemoteCtx,
    ) {
        let key = (source.clone(), node.clone());
        let cache_key = remote_playlist_cache_key(source, node);
        let (cached_len, needs_revalidation, retrying) = {
            let mut cache = tracks_cache.lock().unwrap();
            match cache.get_mut(&key) {
                Some(entry) if entry.ingesting || entry.browsing => return,
                // Frozen on a genuine end-of-list: nothing more to fetch.
                Some(entry) if !entry.partial && !entry.errored => return,
                // Frozen on a fetch error, and either the caller doesn't
                // need more than what's already cached (no scroll past the
                // tail yet), or it's already failed
                // `MAX_CONSECUTIVE_PAGE_FAILURES` times in a row — stay
                // frozen rather than hammer the source every redraw.
                Some(entry)
                    if !entry.partial
                        && entry.errored
                        && (want <= entry.tracks.len()
                            || entry.consecutive_failures >= MAX_CONSECUTIVE_PAGE_FAILURES) =>
                {
                    return;
                }
                Some(entry) => {
                    // A scroll-to-the-tail demand just asked for more than
                    // an errored freeze had — reopen it and retry.
                    let retrying = !entry.partial && entry.errored;
                    if retrying {
                        entry.partial = true;
                        entry.errored = false;
                    }
                    entry.browsing = true;
                    (entry.tracks.len(), !entry.revalidated, retrying)
                }
                None => {
                    // First touch this session — hydrate from whatever a
                    // previous session persisted instead of starting empty.
                    let persisted = ctx.store.remote_playlist_ids(&cache_key).unwrap_or_default();
                    let tracks = ctx.store.get_tracks(&persisted).unwrap_or_default();
                    let len = tracks.len();
                    let entry = RemotePlaylistTracks {
                        ids: tracks.iter().map(|t| t.id).collect(),
                        tracks,
                        generation: 1,
                        ingesting: false,
                        browsing: true,
                        partial: true,
                        revalidated: len == 0,
                        persisted_len: len,
                        errored: false,
                        consecutive_failures: 0,
                        failed_at: None,
                    };
                    cache.insert(key.clone(), entry);
                    (len, len > 0, false)
                }
            }
        };

        let Some(source_handle) = ctx.sources.get(source).cloned() else {
            if let Some(entry) = tracks_cache.lock().unwrap().get_mut(&key) {
                entry.browsing = false;
            }
            return;
        };
        // The underlying paged walk froze itself on the failed page too
        // (see `PagedList::errored`) — reset it so it resumes from where it
        // left off instead of handing back the same stale error forever.
        if retrying {
            source_handle.retry_browse(node);
        }
        let node = node.clone();
        let deps = RemotePlaylistDeps {
            catalog: ctx.catalog.clone(),
            store: ctx.store.clone(),
            bus: ctx.bus.clone(),
            cache: tracks_cache.clone(),
        };

        std::thread::spawn(move || {
            let result = source_handle.browse(&node, want);
            if let Some(entry) = deps.cache.lock().unwrap().get_mut(&key) {
                entry.browsing = false;
            }
            let page = match result {
                Ok(page) => page,
                Err(e) => {
                    // Freeze the entry so a permanent failure (e.g. a 403 on
                    // a playlist we're not allowed to see) doesn't get
                    // retried on every redraw — but mark it `errored` so a
                    // later scroll-to-the-tail demand can still ask for a
                    // retry (see the `want`-vs-`tracks.len()` guard above),
                    // up to `MAX_CONSECUTIVE_PAGE_FAILURES`.
                    if let Some(entry) = deps.cache.lock().unwrap().get_mut(&key) {
                        entry.partial = false;
                        entry.errored = true;
                        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                        entry.failed_at = Some(Instant::now());
                    }
                    deps.bus.send(CoreEvent::BackgroundFailure {
                        context: key.0.to_string(),
                        message: format!("{cache_key}: {e}"),
                    });
                    deps.bus.send(CoreEvent::PlaylistsChanged);
                    return;
                }
            };

            if page.errored {
                // The page walk stopped short on a fetch error rather than
                // reaching the real end of the list — `page.tracks` is a
                // truncated prefix even though `page.partial` says "done".
                // Freeze the entry (so this isn't retried every redraw) but
                // leave `entry.tracks` and the persisted store exactly as
                // they were: a truncated result must never replace a good
                // cached copy.
                log::warn!("{cache_key}: page walk failed partway through, keeping existing cache");
                if let Some(entry) = deps.cache.lock().unwrap().get_mut(&key) {
                    entry.partial = false;
                    entry.errored = true;
                    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                    entry.failed_at = Some(Instant::now());
                }
                deps.bus.send(CoreEvent::BackgroundFailure {
                    context: key.0.to_string(),
                    message: format!("{cache_key}: playlist fetch failed partway through"),
                });
                deps.bus.send(CoreEvent::PlaylistsChanged);
                return;
            }

            // A page landed cleanly — any earlier failure streak no longer
            // applies.
            if let Some(entry) = deps.cache.lock().unwrap().get_mut(&key) {
                entry.consecutive_failures = 0;
            }

            if needs_revalidation {
                // Not enough live data yet to check the whole persisted
                // prefix (and the source isn't done either) — wait for more
                // pages rather than trust an unconfirmed cache.
                if page.tracks.len() < cached_len && page.partial {
                    return;
                }
                let compare_len = cached_len.min(page.tracks.len());
                let stale = compare_len < cached_len
                    || {
                        let cache_guard = deps.cache.lock().unwrap();
                        let cached = &cache_guard.get(&key).expect("just inserted/read above").tracks;
                        page.tracks.iter().take(compare_len).enumerate().any(|(i, hit)| {
                            !cached[i].renditions.iter().any(|r| r.source == key.0 && r.uri == hit.rendition().uri)
                        })
                    };
                if stale {
                    log::info!("{cache_key}: persisted cache stale, re-syncing");
                    let _ = deps.store.set_remote_playlist_ids(&cache_key, &[]);
                    if let Some(entry) = deps.cache.lock().unwrap().get_mut(&key) {
                        entry.persisted_len = 0;
                    }
                    Self::spawn_remote_playlist_ingest(deps, key, cache_key, page, 0);
                    return;
                }
                if let Some(entry) = deps.cache.lock().unwrap().get_mut(&key) {
                    entry.revalidated = true;
                }
            }

            if page.tracks.len() <= cached_len {
                // Nothing new to ingest — just keep `partial` honest so this
                // freezes once the source itself is done. If the walk just
                // finished, flush anything a batched persist left un-written.
                let (changed, finished, all_ids): (bool, bool, Option<Vec<TrackId>>) = {
                    let mut cache_guard = deps.cache.lock().unwrap();
                    let entry = cache_guard.entry(key).or_insert_with(RemotePlaylistTracks::empty);
                    let changed = entry.partial != page.partial;
                    entry.partial = page.partial;
                    let finished = !page.partial && entry.persisted_len < entry.tracks.len();
                    let ids = finished.then(|| {
                        entry.persisted_len = entry.tracks.len();
                        entry.tracks.iter().map(|t| t.id).collect()
                    });
                    (changed, finished, ids)
                };
                if finished
                    && let Some(ids) = all_ids
                    && let Err(e) = deps.store.set_remote_playlist_ids(&cache_key, &ids)
                {
                    log::warn!("remote playlist cache: failed to persist {cache_key}: {e}");
                }
                // A still-in-flight background fetch (page.partial unchanged,
                // nothing new) is polled again at the UI's own baseline
                // redraw rate — sending an event here too would wake it
                // immediately instead, so this poll and the next redraw's
                // poll chase each other as fast as the scheduler allows,
                // with no actual progress between them (this is what made a
                // page stuck behind a persistent 403 feel like an unbounded
                // loop rather than a bounded, quiet retry).
                if changed || finished {
                    deps.bus.send(CoreEvent::PlaylistsChanged);
                }
                return;
            }
            Self::spawn_remote_playlist_ingest(deps, key, cache_key, page, cached_len);
        });
    }

    /// Fold `page.tracks[skip_len..]` into the catalog and the cache entry
    /// on a background thread, then persist the resulting id list.
    /// `skip_len == 0` replaces the entry's tracks outright (the
    /// re-sync-from-scratch path); otherwise it appends. Takes its
    /// dependencies explicitly (rather than `&self`) so it can be called
    /// either from the UI-triggering thread or from the background thread
    /// `ensure_remote_playlist_tracks` itself spawns for `browse()`.
    fn spawn_remote_playlist_ingest(
        deps: RemotePlaylistDeps,
        key: (SourceId, BrowseNode),
        cache_key: String,
        page: BrowsePage,
        skip_len: usize,
    ) {
        {
            let mut cache = deps.cache.lock().unwrap();
            let entry = cache.entry(key.clone()).or_insert_with(RemotePlaylistTracks::empty);
            entry.ingesting = true;
        }

        let partial = page.partial;
        std::thread::spawn(move || {
            let RemotePlaylistDeps { catalog, store, bus, cache } = deps;
            let new_tracks: Vec<Track> = page
                .tracks
                .into_iter()
                .skip(skip_len)
                .filter_map(|h| catalog.ingest(h).ok())
                .filter_map(|id| store.get_track(id).ok().flatten())
                .collect();
            let (all_ids, should_persist): (Vec<TrackId>, bool) = {
                let mut cache = cache.lock().unwrap();
                let entry = cache.entry(key).or_insert_with(RemotePlaylistTracks::empty);
                entry.edit_tracks(|tracks| {
                    if skip_len == 0 {
                        *tracks = new_tracks;
                    } else {
                        tracks.extend(new_tracks);
                    }
                });
                entry.partial = partial;
                entry.ingesting = false;
                entry.revalidated = true;
                let all_ids: Vec<TrackId> = entry.tracks.iter().map(|t| t.id).collect();
                let should_persist =
                    !partial || all_ids.len().saturating_sub(entry.persisted_len) >= PERSIST_BATCH;
                if should_persist {
                    entry.persisted_len = all_ids.len();
                }
                (all_ids, should_persist)
            };
            // Best-effort: a failed write just means a slower cold start
            // next time, not lost data.
            if should_persist && let Err(e) = store.set_remote_playlist_ids(&cache_key, &all_ids) {
                log::warn!("remote playlist cache: failed to persist {cache_key}: {e}");
            }
            // The cache just grew (or `partial` flipped to false) —
            // `CoreEvent::PlaylistsChanged` is the front-end's cue to
            // redraw and re-`browse`, matching `sources_spotify`'s own use
            // of it when a background-fetched page lands.
            bus.send(CoreEvent::PlaylistsChanged);
        });
    }
}
