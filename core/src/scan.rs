//! Background scan plugins: a shared driver walks the library and lets
//! registered plugins compute per-track metadata (bpm, later genre/mood/...)
//! into the generic `Track::attrs` map.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::media_cache::MediaCache;
use crate::stream::{Claim, Intent, StreamEngine, StreamHandle, StreamState};
use crate::traits::{Error, Store};
use crate::types::{Rendition, SourceId, Track, TrackId};

/// One background-scan extension point (bpm, later genre-ml, ...). A new
/// plugin means a new `attrs` key, never a driver or `Track` change.
pub trait ScanPlugin: Send + Sync {
    /// Stable id, e.g. "bpm", "genre-ml". Used in config keys and logs.
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    /// Cheap, no I/O: does this track still need this plugin's data? The
    /// driver calls this to skip already-scanned tracks on every walk.
    fn needs(&self, track: &Track) -> bool;

    /// Blocking analysis of one track. `audio` opens the track's stream (a cached file is a complete
    /// one, a download in progress is read as it grows); `Err` is the outcome to return when there is
    /// none. A stream that fails or is cancelled under the decode is `Outcome::Retry`, never `Skip`.
    fn analyze(&self, track: &Track, audio: &dyn Fn() -> Result<StreamHandle, Outcome>) -> Outcome;

    /// Minimum spacing between this plugin's own background fetches — keeps
    /// a slow/rate-limited source from being hammered.
    fn min_interval(&self) -> Duration {
        Duration::from_secs(15)
    }
}

pub enum Outcome {
    /// Store this. `attrs` are merged (inserted key-by-key) into
    /// `Track::attrs`, not a wholesale replacement — two plugins writing
    /// different keys to the same track must not clobber each other.
    Done(TrackMeta),
    /// Not this plugin's job for this track — don't retry this session.
    Skip,
    /// Transient failure (network, rate limit) — retry on a later walk.
    Retry,
}

#[derive(Default)]
pub struct TrackMeta {
    pub attrs: BTreeMap<String, String>,
}

/// Live per-(plugin, track) scan state, for a UI polling "what's happening
/// with this track right now" (there's no "waiting" variant here — that's
/// just the absence of an entry, see `ScanDriver::status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStatus {
    /// `analyze` is running for this track right now.
    Downloading,
    /// Last attempt returned `Outcome::Retry`.
    Error,
    /// Last attempt returned `Outcome::Skip`.
    Skipped,
}

/// How an attempt may reach a track's audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    /// Start a download if the track is not cached (the driver keeps it alive to `Done`).
    Fetch,
    /// Only a cached file or a stream that is already running; never starts a fetch.
    Peek,
}

/// The driver's overall on/off/how-hard state, cycled by `B`/`:togglescan`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMode {
    /// Start downloads for tracks that are not cached.
    Active,
    /// Walk the library but only ever read what is already cached or downloading — never start a fetch.
    CacheOnly,
    /// Walk thread does nothing.
    Disabled,
}

impl ScanMode {
    /// `B`/`:togglescan`'s toggle — Active and CacheOnly only. `Disabled` is
    /// reachable solely via `scan.bpm.enabled` in the config file (or, once
    /// built, the Settings pane's toggle); `B` leaves it alone rather than
    /// ever entering or leaving it, so fully turning the plugin off/on stays
    /// a deliberate action distinct from the two-mode day-to-day toggle.
    pub fn cycle(self) -> Self {
        match self {
            ScanMode::Active => ScanMode::CacheOnly,
            ScanMode::CacheOnly => ScanMode::Active,
            ScanMode::Disabled => ScanMode::Disabled,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            ScanMode::Active => 0,
            ScanMode::CacheOnly => 1,
            ScanMode::Disabled => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => ScanMode::Active,
            1 => ScanMode::CacheOnly,
            _ => ScanMode::Disabled,
        }
    }
}

/// How often the walk thread wakes.
const TICK: Duration = Duration::from_millis(400);

/// A track that has failed (`Outcome::Retry`) this many times in a row for
/// the same plugin gets put on `FAILURE_COOLDOWN` instead of being retried
/// on every walk — distinct from a plugin's flat `min_interval`, which isn't
/// failure-count-aware and would otherwise keep spending a walk slot on a
/// permanently-broken track (e.g. no fetchable audio) forever.
const FAILURE_THRESHOLD: u32 = 3;
const FAILURE_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// Once a full walk pass finds nothing to scan, back off to this interval
/// instead of re-checking the same fully-analyzed list every `TICK`.
const IDLE_BACKOFF: Duration = Duration::from_secs(10);

/// How long the now-playing fast path (`ScanDriver::prioritize`) keeps retrying a track every tick
/// before the general walk's own (idle-backed-off) cadence takes over. Bounds the cost of a track
/// whose stream is not running (not playing, not cached), which a `Peek` cannot open.
const PRIORITY_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a stream may take to deliver its first byte before the attempt is a `Retry`.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(60);

/// After a failed fetch a source is left alone for `SOURCE_BACKOFF << (failures - 1)`, capped at
/// `SOURCE_BACKOFF_MAX`, so a run of bad tracks cannot hammer it (Spotify recycles its session after
/// two failed opens in a row, which would interrupt playback).
const SOURCE_BACKOFF: Duration = Duration::from_secs(60);
const SOURCE_BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// The order + position the UI's current view reports via
/// `ScanDriver::follow_view` — the walk's primary traversal order once set.
#[derive(Clone, PartialEq, Eq)]
struct ViewOrder {
    tracks: Vec<TrackId>,
    highlighted: usize,
}

struct Inner {
    /// Grows via `ScanDriver::register_plugin` as sources finish async
    /// setup. Cloned fresh each tick rather than locked across `analyze`.
    plugins: Mutex<Vec<Arc<dyn ScanPlugin>>>,
    mode: AtomicU8,
    /// Track ids that jumped the queue (currently-playing), oldest first,
    /// paired with when each was first prioritized — see
    /// `PRIORITY_RETRY_TIMEOUT`.
    priority: Mutex<VecDeque<(TrackId, Instant)>>,
    last_run: Mutex<HashMap<&'static str, Instant>>,
    /// Per-(plugin, track) live status, for `ScanDriver::status` — cleared on
    /// `Outcome::Done` since a resolved value already lives in `Track::attrs`
    /// at that point and needs no separate "done" bookkeeping here.
    status: Mutex<HashMap<(&'static str, TrackId), ScanStatus>>,
    /// The UI's current view/selection, see `ScanDriver::follow_view`. `None`
    /// while no view has reported one yet, or the last-reported view had no
    /// tracks — the walk falls back to `store.all_tracks()` in that case.
    view: Mutex<Option<ViewOrder>>,
    /// Set once a full pass over the current walk list (whichever list
    /// `resolve_walk_list` returned) found nothing to scan; cleared as soon
    /// as the list being walked changes. Guards against busy-looping a
    /// fully-analyzed list every `TICK`.
    idle_since: Mutex<Option<(Vec<TrackId>, Instant)>>,
    /// Consecutive `Outcome::Retry` count per (plugin, track), for the
    /// failure cooldown — reset to 0 on `Outcome::Done`.
    failures: Mutex<HashMap<(&'static str, TrackId), u32>>,
    /// Tracks currently serving out a `FAILURE_COOLDOWN`, and when it ends.
    failure_cooldown: Mutex<HashMap<(&'static str, TrackId), Instant>>,
    /// Background fetches the driver keeps alive until they finish, so a walk download runs to `Done`.
    held: Mutex<Vec<(StreamHandle, Claim)>>,
    /// Per source: consecutive failed fetches and when it may be fetched from again.
    source_backoff: Mutex<HashMap<SourceId, (u32, Instant)>>,
}

/// One background thread, shared by every registered plugin.
pub struct ScanDriver {
    inner: Arc<Inner>,
}

impl ScanDriver {
    pub fn new(plugins: Vec<Arc<dyn ScanPlugin>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                plugins: Mutex::new(plugins),
                mode: AtomicU8::new(ScanMode::Active.to_u8()),
                priority: Mutex::new(VecDeque::new()),
                last_run: Mutex::new(HashMap::new()),
                status: Mutex::new(HashMap::new()),
                view: Mutex::new(None),
                idle_since: Mutex::new(None),
                failures: Mutex::new(HashMap::new()),
                failure_cooldown: Mutex::new(HashMap::new()),
                held: Mutex::new(Vec::new()),
                source_backoff: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Spawns the walk thread. Writes go through `catalog` (`Catalog::patch`,
    /// see the lock on `Catalog` itself) rather than `Store` directly, so a
    /// scan result can never race a concurrent UI-thread `ingest`/`patch`
    /// (M3U import, `link`/`unlink`, ...) into a lost update — both paths
    /// serialize on the same lock. `store` is still passed separately since
    /// only reads (`all_tracks`/`get_track`) are needed off it; those don't
    /// need to be under the lock, they just pick which track to look at
    /// next, and `patch`'s own read (inside the lock) is what actually
    /// decides what gets merged.
    pub fn spawn(
        &self,
        catalog: Arc<Catalog>,
        store: Arc<dyn Store>,
        engine: StreamEngine,
        media_cache: Arc<MediaCache>,
    ) {
        let inner = self.inner.clone();
        thread::Builder::new()
            .name("scan-driver".into())
            .spawn(move || run(inner, catalog, store, engine, media_cache))
            .expect("failed to spawn scan-driver thread");
    }

    /// Adds a plugin to the already-running driver, for one whose
    /// availability depends on setup finishing rather than being known at
    /// construction time.
    pub fn register_plugin(&self, plugin: Arc<dyn ScanPlugin>) {
        self.inner.plugins.lock().unwrap().push(plugin);
    }

    /// The currently-playing track jumps the walk order for every plugin
    /// that still `needs()` it. Call from wherever `Session` reacts to
    /// `PlayerEvent::Playing`.
    pub fn prioritize(&self, track: TrackId) {
        let mut q = self.inner.priority.lock().unwrap();
        // A fresh call (e.g. `Stream{Done}` firing after
        // `Playing` already queued it) resets the retry window rather than
        // being a no-op, since it's new evidence there's something worth
        // trying again for.
        q.retain(|(t, _)| *t != track);
        q.push_front((track, Instant::now()));
    }

    /// Reports "the list the user is currently looking at, and which row is
    /// highlighted" — the walk then proceeds from `highlighted`, forward,
    /// wrapping to the start, instead of `store.all_tracks()`'s arbitrary
    /// order. Call whenever the UI's active screen/selection changes (or
    /// every redraw, which is cheap — this just replaces a small shared
    /// value). An empty `tracks` clears it, falling back to the full-library
    /// walk (e.g. a screen with no track list, like Playlists' own folder
    /// view).
    pub fn follow_view(&self, tracks: Vec<TrackId>, highlighted: usize) {
        let mut view = self.inner.view.lock().unwrap();
        *view = if tracks.is_empty() { None } else { Some(ViewOrder { tracks, highlighted }) };
    }

    /// Sets the driver's mode (`B` / `:togglescan` cycles it). Switching to
    /// `Disabled` drains `priority` and clears any in-flight `Downloading`
    /// status so it fully inhibits the scanner rather than just skipping
    /// the next tick.
    pub fn set_mode(&self, mode: ScanMode) {
        self.inner.mode.store(mode.to_u8(), Ordering::Relaxed);
        if mode == ScanMode::Disabled {
            self.inner.priority.lock().unwrap().clear();
            self.inner.status.lock().unwrap().retain(|_, v| *v != ScanStatus::Downloading);
        }
    }

    pub fn mode(&self) -> ScanMode {
        ScanMode::from_u8(self.inner.mode.load(Ordering::Relaxed))
    }

    /// Live status of `plugin_id`'s last/current attempt at `track`. `None`
    /// means "waiting" — never attempted yet, or the previous attempt
    /// succeeded (in which case the result is in `Track::attrs` instead).
    pub fn status(&self, plugin_id: &'static str, track: TrackId) -> Option<ScanStatus> {
        self.inner.status.lock().unwrap().get(&(plugin_id, track)).copied()
    }
}

/// The per-track-independent resources `scan_one` needs, bundled so its own
/// signature stays under clippy's argument-count limit.
struct ScanCtx<'a> {
    engine: &'a StreamEngine,
    media_cache: &'a MediaCache,
}

fn run(inner: Arc<Inner>, catalog: Arc<Catalog>, store: Arc<dyn Store>, engine: StreamEngine, media_cache: Arc<MediaCache>) {
    loop {
        thread::sleep(TICK);
        let mode = ScanMode::from_u8(inner.mode.load(Ordering::Relaxed));
        reap_held(&inner, mode);
        if mode == ScanMode::Disabled {
            continue;
        }

        let plugins = inner.plugins.lock().unwrap().clone();
        let ctx = ScanCtx { engine: &engine, media_cache: &media_cache };

        let prio = inner.priority.lock().unwrap().pop_front();
        if let Some((id, started)) = prio {
            match store.get_track(id) {
                Ok(Some(track)) => {
                    // Peeks the running or cached stream, so it never waits on a plugin's `min_interval`.
                    // Requeued while a plugin still needs the track, but only for `PRIORITY_RETRY_TIMEOUT`.
                    let attempted = scan_one(&inner, &plugins, &catalog, &ctx, &track, true, mode);
                    if any_plugin_needs(&inner, &plugins, &track) {
                        if started.elapsed() < PRIORITY_RETRY_TIMEOUT {
                            inner.priority.lock().unwrap().push_front((id, started));
                        } else {
                            log::debug!(
                                "scan: priority track {id:?} still unresolved after {PRIORITY_RETRY_TIMEOUT:?}, \
                                 falling back to the general walk's own cadence"
                            );
                        }
                    } else {
                        log::debug!("scan: priority track {id:?} has nothing left to do");
                    }
                    if attempted {
                        continue;
                    }
                }
                Ok(None) => log::debug!("scan: priority track {id:?} not found in store"),
                Err(e) => log::warn!("scan: priority lookup of {id:?} failed: {e}"),
            }
        }

        // MVP: an O(list) walk every tick rather than a persisted cursor —
        // fine at MVP scale, and `needs()` makes an already-fully-scanned
        // pass cheap (no I/O). `track` here is only ever used to decide
        // *whether* and *what* to scan — the actual write
        // (`scan_one` -> `Catalog::patch`) re-reads under its own lock, so a
        // stale snapshot from this walk can't clobber a concurrent
        // UI-thread edit made after the list was read.
        //
        // Every track is tried, not just the first that needs work: a
        // track a plugin can never handle (permanent `Skip`/`Retry`, e.g.
        // no fetchable audio) must not starve every track after it in the
        // walk order — each plugin's own `min_interval` cooldown (plus the
        // per-track failure cooldown) already bounds how much real I/O this
        // does per tick.
        let Some(tracks) = resolve_walk_list(&inner, &store, &plugins, &media_cache, mode) else {
            continue;
        };
        let ids: Vec<TrackId> = tracks.iter().map(|t| t.id).collect();
        {
            let idle = inner.idle_since.lock().unwrap();
            if let Some((idle_ids, since)) = idle.as_ref()
                && *idle_ids == ids
                && since.elapsed() < IDLE_BACKOFF
            {
                continue;
            }
        }
        let mut scanned = 0usize;
        for track in &tracks {
            if scan_one(&inner, &plugins, &catalog, &ctx, track, false, mode) {
                scanned += 1;
            }
        }
        if scanned > 0 {
            log::debug!("scan: walk tick scanned {scanned}/{} track(s)", tracks.len());
            inner.idle_since.lock().unwrap().take();
        } else {
            inner.idle_since.lock().unwrap().replace((ids, Instant::now()));
        }
    }
}

/// The list of tracks (and their order) `run`'s walk should try this tick:
/// the UI-reported view (`ScanDriver::follow_view`), starting at its
/// highlighted row and wrapping, if one is set; otherwise the full library
/// in whatever order `store.all_tracks()` returns it. `None` only on a store
/// read failure.
///
/// Under `CacheOnly`, also drops any track that is not cached (or that no plugin still `needs()`):
/// only the priority path peeks at a stream that is still downloading.
fn resolve_walk_list(
    inner: &Inner,
    store: &Arc<dyn Store>,
    plugins: &[Arc<dyn ScanPlugin>],
    media_cache: &MediaCache,
    mode: ScanMode,
) -> Option<Vec<Track>> {
    let tracks = if let Some(view) = inner.view.lock().unwrap().clone() {
        let n = view.tracks.len();
        let start = view.highlighted % n;
        let mut out = Vec::with_capacity(n);
        for offset in 0..n {
            let id = view.tracks[(start + offset) % n];
            if let Ok(Some(track)) = store.get_track(id) {
                out.push(track);
            }
        }
        out
    } else {
        store.all_tracks().ok()?
    };

    if mode != ScanMode::CacheOnly {
        return Some(tracks);
    }
    Some(
        tracks
            .into_iter()
            .filter(|t| {
                any_plugin_needs(inner, plugins, t)
                    && t.renditions.iter().any(|r| media_cache.cached_path(&r.source, &r.uri).is_some())
            })
            .collect(),
    )
}

/// Whether any registered plugin still `needs()` `track` and hasn't
/// permanently given up on it (`Outcome::Skip`) — used to decide whether a
/// priority track that couldn't be scanned this tick is worth requeuing.
fn any_plugin_needs(inner: &Inner, plugins: &[Arc<dyn ScanPlugin>], track: &Track) -> bool {
    plugins.iter().any(|p| {
        p.needs(track)
            && inner.status.lock().unwrap().get(&(p.id(), track.id)) != Some(&ScanStatus::Skipped)
    })
}

/// Whether a fetch for `track` may start now: it is cached or already downloading, or no download
/// is being kept alive (one at a time) and one of its sources is not backed off.
fn fetchable(inner: &Inner, ctx: &ScanCtx, track: &Track) -> bool {
    let cached = |r: &Rendition| ctx.media_cache.cached_path(&r.source, &r.uri).is_some();
    let held = inner.held.lock().unwrap();
    if track.renditions.iter().any(|r| cached(r) || held.iter().any(|(h, _)| *h.key() == (r.source.clone(), r.uri.clone()))) {
        return true;
    }
    let backoff = inner.source_backoff.lock().unwrap();
    held.is_empty() && track.renditions.iter().any(|r| backoff.get(&r.source).is_none_or(|(_, until)| Instant::now() >= *until))
}

/// Opens `track`'s audio as a stream: a cached rendition first, else per `access`. Records the stream in `used`.
fn open_stream(
    inner: &Inner,
    ctx: &ScanCtx,
    track: &Track,
    access: Access,
    used: &RefCell<Option<StreamHandle>>,
) -> Result<StreamHandle, Outcome> {
    let mut renditions: Vec<&Rendition> = track.renditions.iter().collect();
    renditions.sort_by_key(|r| ctx.media_cache.cached_path(&r.source, &r.uri).is_none());
    for r in renditions {
        let intent = if access == Access::Fetch { Intent::Fetch } else { Intent::Peek };
        let (handle, claim) = match ctx.engine.open(r, intent) {
            Ok(opened) => opened,
            Err(Error::NotFound) => continue,
            Err(e) => {
                log::warn!("scan: {} couldn't open {}: {e}", r.source, r.uri);
                continue;
            }
        };
        *used.borrow_mut() = Some(handle.clone());
        if intent == Intent::Fetch && handle.info().state != StreamState::Done {
            inner.held.lock().unwrap().push((handle.clone(), claim));
        }
        if !handle.wait_range(0..1, FIRST_BYTE_TIMEOUT) {
            inner.held.lock().unwrap().retain(|(h, _)| h.key() != handle.key());
            return Err(Outcome::Retry);
        }
        return Ok(handle);
    }
    Err(Outcome::Skip)
}

/// A failed fetch backs its source off; a finished one clears it.
fn note_source(inner: &Inner, source: &SourceId, failed: bool) {
    let mut backoff = inner.source_backoff.lock().unwrap();
    if !failed {
        backoff.remove(source);
        return;
    }
    let count = backoff.get(source).map_or(0, |(n, _)| *n) + 1;
    let wait = (SOURCE_BACKOFF * 2u32.saturating_pow(count - 1)).min(SOURCE_BACKOFF_MAX);
    log::debug!("scan: {source} fetch failed {count} time(s) in a row, leaving it alone for {wait:?}");
    backoff.insert(source.clone(), (count, Instant::now() + wait));
}

/// Run the first plugin that both `needs()` `track` and is off its own
/// `min_interval` cooldown. Returns whether a plugin was actually invoked.
/// `priority`: the currently-playing-track fast path, which only peeks (never starts a fetch).
fn scan_one(
    inner: &Inner,
    plugins: &[Arc<dyn ScanPlugin>],
    catalog: &Arc<Catalog>,
    ctx: &ScanCtx,
    track: &Track,
    priority: bool,
    driver_mode: ScanMode,
) -> bool {
    for plugin in plugins {
        if !plugin.needs(track) {
            continue;
        }
        // A prior `Outcome::Skip` means "not this plugin's job" for the rest of this run.
        if inner.status.lock().unwrap().get(&(plugin.id(), track.id)) == Some(&ScanStatus::Skipped)
        {
            continue;
        }
        {
            let mut cooldown = inner.failure_cooldown.lock().unwrap();
            match cooldown.get(&(plugin.id(), track.id)) {
                Some(since) if since.elapsed() < FAILURE_COOLDOWN => continue,
                Some(_) => {
                    cooldown.remove(&(plugin.id(), track.id));
                }
                None => {}
            }
        }

        let access = if priority || driver_mode == ScanMode::CacheOnly { Access::Peek } else { Access::Fetch };
        // Left "waiting" (no status): the track stays queued for a later walk.
        if access == Access::Fetch && !fetchable(inner, ctx, track) {
            return false;
        }

        // `min_interval` only throttles fetches; a peek never starts one.
        if access == Access::Fetch {
            let mut last_run = inner.last_run.lock().unwrap();
            let ready = last_run
                .get(plugin.id())
                .is_none_or(|t| t.elapsed() >= plugin.min_interval());
            if !ready {
                continue;
            }
            last_run.insert(plugin.id(), Instant::now());
        }

        inner.status.lock().unwrap().insert((plugin.id(), track.id), ScanStatus::Downloading);

        log::debug!(
            "scan[{}]: attempting \"{}\" ({:?}, {access:?}{})",
            plugin.id(),
            track.title,
            track.id,
            if priority { ", priority" } else { "" }
        );
        let used: RefCell<Option<StreamHandle>> = RefCell::new(None);
        let open_audio = || open_stream(inner, ctx, track, access, &used);
        let outcome = plugin.analyze(track, &open_audio);
        let stream = used.into_inner();
        let source = stream.as_ref().map(|h| h.key().0.clone());
        match outcome {
            Outcome::Done(meta) => {
                log::debug!(
                    "scan[{}]: done with \"{}\" ({:?}): {:?}",
                    plugin.id(),
                    track.title,
                    track.id,
                    meta.attrs
                );
                // `patch` re-reads the track under `Catalog`'s lock and merges into whatever's current.
                let _ = catalog.patch(track.id, |t| t.attrs.extend(meta.attrs));
                inner.status.lock().unwrap().remove(&(plugin.id(), track.id));
                inner.failures.lock().unwrap().remove(&(plugin.id(), track.id));
                if let Some(source) = &source {
                    note_source(inner, source, false);
                }
            }
            Outcome::Skip if stream.is_none() && access == Access::Peek => {
                // No stream to peek at (not playing, not cached): not a verdict, and nothing was accomplished,
                // so the walk's idle backoff still engages.
                log::debug!(
                    "scan[{}]: \"{}\" ({:?}) has no cached or running stream yet",
                    plugin.id(),
                    track.title,
                    track.id
                );
                inner.status.lock().unwrap().remove(&(plugin.id(), track.id));
                return false;
            }
            Outcome::Skip => {
                log::debug!(
                    "scan[{}]: skipping \"{}\" ({:?}) — not applicable",
                    plugin.id(),
                    track.title,
                    track.id
                );
                inner.status.lock().unwrap().insert((plugin.id(), track.id), ScanStatus::Skipped);
                // A real fetch may have cached audio even with nothing to report — announce it.
                catalog.announce(track.id);
            }
            Outcome::Retry if stream.as_ref().is_some_and(|h| h.info().state == StreamState::Cancelled) => {
                // Skipped or paused mid-analysis: not a failure, tried again later.
                log::debug!("scan[{}]: \"{}\" ({:?}) stream cancelled, trying again later", plugin.id(), track.title, track.id);
                inner.status.lock().unwrap().remove(&(plugin.id(), track.id));
            }
            Outcome::Retry => {
                log::warn!(
                    "scan[{}]: retrying \"{}\" ({:?}) later — transient failure",
                    plugin.id(),
                    track.title,
                    track.id
                );
                inner.status.lock().unwrap().insert((plugin.id(), track.id), ScanStatus::Error);
                if access == Access::Fetch
                    && let Some(source) = &source
                {
                    note_source(inner, source, true);
                }
                let mut failures = inner.failures.lock().unwrap();
                let count = failures.entry((plugin.id(), track.id)).or_insert(0);
                *count += 1;
                if *count >= FAILURE_THRESHOLD {
                    // No track name, so every track that gives up shares one warnings row.
                    catalog.warn(plugin.id(), "scan keeps failing for some tracks — see :log".to_string());
                    inner
                        .failure_cooldown
                        .lock()
                        .unwrap()
                        .insert((plugin.id(), track.id), Instant::now());
                }
            }
        }
        return true;
    }
    false
}

/// Drops the claims of finished fetches, and all of them once the driver stops fetching.
fn reap_held(inner: &Inner, mode: ScanMode) {
    let mut held = inner.held.lock().unwrap();
    held.retain(|(h, _)| mode == ScanMode::Active && matches!(h.info().state, StreamState::Connecting | StreamState::Fetching | StreamState::Buffering));
}
