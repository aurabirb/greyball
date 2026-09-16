//! `Session`'s UI-facing view state: search results and remote-playlist
//! browsing, both cached so the UI's render path (called every redraw/
//! keypress) never re-hits the store or a source's API just to draw the
//! same list again. Kept as its own type so `Session` stays data-access and
//! wiring, not view state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::catalog::Catalog;
use crate::event::{Bus, CoreEvent};
use crate::traits::{BrowseNode, BrowsePage, Source, Store};
use crate::types::{SourceId, Track, TrackId};

/// Cache entry behind `ViewCache::remote_playlist_track_ids`/`_len`/`_window`.
/// `tracks` (already resolved from the store, not just ids — see
/// `ViewCache::ensure_remote_playlist_tracks`) is always a prefix of the real
/// list in the same order the source returns it in (append-only — sources
/// backing a `partial` `BrowsePage` must not reorder or replace earlier rows
/// across calls), so a later call only needs to resolve the tail past
/// `tracks.len()`.
struct RemotePlaylistTracks {
    tracks: Vec<Track>,
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
}

impl RemotePlaylistTracks {
    fn empty() -> Self {
        Self {
            tracks: Vec::new(),
            ingesting: false,
            browsing: false,
            partial: true,
            revalidated: true,
            persisted_len: 0,
            errored: false,
            consecutive_failures: 0,
        }
    }
}

/// See `RemotePlaylistTracks::consecutive_failures` — matches the retry-bound
/// precedent set for Spotify's session-reconnect streak
/// (`sources/spotify/src/player.rs`'s `session_died_streak`), though this is a
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
    cache: Arc<Mutex<HashMap<(SourceId, BrowseNode), RemotePlaylistTracks>>>,
}

#[derive(Default)]
pub(crate) struct ViewCache {
    results: Vec<TrackId>,
    /// `results`, already resolved to `Track` — kept in lockstep by
    /// `push_result` so a redraw never re-resolves the whole list against
    /// the store just to render it again.
    results_cache: Vec<Track>,

    /// A source's top-level playlist folders (e.g. Spotify's `/me/playlists`),
    /// fetched once per session and cached: the Playlists screen calls
    /// `remote_playlists` on every redraw, so a cache miss must only ever
    /// happen the first time a given source is looked at, not once a frame.
    remote_playlists: Mutex<HashMap<SourceId, Vec<(String, BrowseNode)>>>,
    /// A remote playlist's ingested tracks, cached per `(source, node)` —
    /// same "safe to call every redraw" requirement as `remote_playlists`.
    /// While `partial` a source may still be loading more in the background
    /// (see `BrowsePage::partial`), so the entry isn't trusted as final:
    /// every redraw re-`browse`s (cheap) and, if there's a newly-landed
    /// tail, kicks off a background thread to fold it into the catalog —
    /// `Arc`-wrapped so that thread can hold its own handle to the map
    /// (`ensure_remote_playlist_tracks`) — until a call comes back
    /// non-partial with nothing left to ingest and the entry freezes.
    remote_playlist_tracks: Arc<Mutex<HashMap<(SourceId, BrowseNode), RemotePlaylistTracks>>>,
}

impl ViewCache {
    pub fn clear_results(&mut self) {
        self.results.clear();
        self.results_cache.clear();
    }

    /// All result ids, cheap (no store hits) — for cursor bounds and
    /// `Command::PlayContext`, which needs the whole list, not just what's
    /// currently visible.
    pub fn results_ids(&self) -> Vec<TrackId> {
        self.results.clone()
    }

    /// Cheap count of `results_cache`, with no clone — for the Search
    /// screen's "no results for X" check and title.
    pub fn results_len(&self) -> usize {
        self.results_cache.len()
    }

    /// A window of the resolved search results (`offset..offset+limit`) —
    /// for rendering just the visible slice instead of cloning the whole
    /// (already in-memory, but still O(n)) `results_cache` every redraw.
    pub fn results_window(&self, offset: usize, limit: usize) -> Vec<Track> {
        self.results_cache.iter().skip(offset).take(limit).cloned().collect()
    }

    /// Append `tid` to the search results, deduped, resolving it against the
    /// store once here rather than leaving a redraw to re-resolve the whole
    /// accumulated list every time.
    pub fn push_result(&mut self, tid: TrackId, store: &Arc<dyn Store>) {
        if self.results.contains(&tid) {
            return;
        }
        self.results.push(tid);
        if let Ok(Some(t)) = store.get_track(tid) {
            self.results_cache.push(t);
        }
    }

    /// `results_cache` and `remote_playlist_tracks`'s entries hold resolved
    /// `Track`s precisely so redraws don't re-hit the store — but that means
    /// a `TrackUpdated` (linking/unlinking, BPM analysis, ...) needs to
    /// patch any copy already sitting in one of those caches, or the UI
    /// would keep showing stale data for it until the list is reloaded from
    /// scratch.
    pub fn refresh_cached_track(&mut self, id: TrackId, store: &Arc<dyn Store>) {
        let Ok(Some(fresh)) = store.get_track(id) else {
            return;
        };
        if let Some(t) = self.results_cache.iter_mut().find(|t| t.id == id) {
            *t = fresh.clone();
        }
        for entry in self.remote_playlist_tracks.lock().unwrap().values_mut() {
            if let Some(t) = entry.tracks.iter_mut().find(|t| t.id == id) {
                *t = fresh.clone();
            }
        }
    }

    /// A source's top-level playlist folders (e.g. Spotify's own playlists,
    /// via `Source::browse(Root)`). Fetched once per session and cached —
    /// safe to call on every redraw of the Playlists screen without
    /// re-hitting the source's API each frame.
    pub fn remote_playlists(&self, source: &SourceId, ctx: RemoteCtx) -> Vec<(String, BrowseNode)> {
        if let Some(cached) = self.remote_playlists.lock().unwrap().get(source) {
            return cached.clone();
        }
        let folders = match ctx.sources.get(source).map(|s| s.browse(&BrowseNode::Root, 0)) {
            Some(Ok(page)) => page.folders,
            Some(Err(e)) => {
                ctx.bus.send(CoreEvent::SourceError {
                    source: source.clone(),
                    message: e.to_string(),
                });
                vec![]
            }
            None => vec![],
        };
        self.remote_playlists
            .lock()
            .unwrap()
            .insert(source.clone(), folders.clone());
        folders
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
        self.remote_playlist_cached(source, node, |e| e.tracks.iter().map(|t| t.id).collect())
            .unwrap_or_default()
    }

    /// Cheap count of a remote playlist's ingested tracks so far.
    pub fn remote_playlist_len(&self, source: &SourceId, node: &BrowseNode, ctx: RemoteCtx) -> usize {
        self.ensure_remote_playlist_tracks(source, node, 0, ctx);
        self.remote_playlist_cached(source, node, |e| e.tracks.len())
            .unwrap_or(0)
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
        self.remote_playlist_cached(source, node, |e| {
            e.tracks.iter().skip(offset).take(limit).cloned().collect()
        })
        .unwrap_or_default()
    }

    /// `(source, node)`'s ingested track ids exactly as far as they've
    /// already loaded, read-only — unlike `remote_playlist_track_ids`, this
    /// never calls `ensure_remote_playlist_tracks`, so it can't itself kick
    /// off a fetch. `None` if nothing has ever touched this entry (not
    /// browsed this session, no persisted cache from a previous one) — for
    /// a caller (e.g. the hotkey-playlists column) that only wants to reuse
    /// whatever's already sitting in the cache as playlists load normally
    /// in the background, never to force-load one nobody has browsed to.
    pub fn remote_playlist_cached_track_ids(
        &self,
        source: &SourceId,
        node: &BrowseNode,
    ) -> Option<Vec<TrackId>> {
        self.remote_playlist_cached(source, node, |e| e.tracks.iter().map(|t| t.id).collect())
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
    fn ensure_remote_playlist_tracks(
        &self,
        source: &SourceId,
        node: &BrowseNode,
        want: usize,
        ctx: RemoteCtx,
    ) {
        let key = (source.clone(), node.clone());
        let cache_key = remote_playlist_cache_key(source, node);
        let (cached_len, needs_revalidation, retrying) = {
            let mut cache = self.remote_playlist_tracks.lock().unwrap();
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
                        tracks,
                        ingesting: false,
                        browsing: true,
                        partial: true,
                        revalidated: len == 0,
                        persisted_len: len,
                        errored: false,
                        consecutive_failures: 0,
                    };
                    cache.insert(key.clone(), entry);
                    (len, len > 0, false)
                }
            }
        };

        let Some(source_handle) = ctx.sources.get(source).cloned() else {
            if let Some(entry) = self.remote_playlist_tracks.lock().unwrap().get_mut(&key) {
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
            cache: self.remote_playlist_tracks.clone(),
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
                    }
                    deps.bus.send(CoreEvent::SourceError {
                        source: key.0.clone(),
                        message: e.to_string(),
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
                }
                deps.bus.send(CoreEvent::SourceError {
                    source: key.0.clone(),
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
                            !cached[i].renditions.iter().any(|r| r.source == key.0 && r.uri == hit.uri)
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
                if skip_len == 0 {
                    entry.tracks = new_tracks;
                } else {
                    entry.tracks.extend(new_tracks);
                }
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
