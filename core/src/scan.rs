//! Background scan plugins: a shared driver walks the library and lets
//! registered plugins compute per-track metadata (bpm, later genre/mood/...)
//! into the generic `Track::attrs` map.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::media_cache::MediaCache;
use crate::traits::{MediaProvider, Player, ReadSeek, Store};
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

    /// Blocking analysis of one track. `audio` lazily opens this track's raw
    /// bytes via `open_scan_audio` (paired with whichever `Rendition` they
    /// actually came from) — call it only on a `media_cache` miss, since
    /// invoking it is what actually fetches/decrypts over the network; it
    /// returns `None` when nothing could supply bytes for this track's
    /// source, in which case return `Outcome::Skip` rather than erroring,
    /// since that isn't this plugin's fault. `media_cache` is the shared
    /// decoded-audio cache: a plugin that decodes `audio` itself should
    /// populate it (`audio_decode::decode_and_cache`) so a later analyzer,
    /// and playback itself, never re-fetch/re-decode the same bytes.
    fn analyze(
        &self,
        track: &Track,
        audio: &dyn Fn() -> Option<(Rendition, Box<dyn ReadSeek + Send>)>,
        media_cache: &MediaCache,
    ) -> Outcome;

    /// Minimum spacing between this plugin's own background fetches — keeps
    /// a slow/rate-limited source from being hammered.
    fn min_interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    /// Whether this plugin wants the whole track's audio rather than just
    /// whatever it happens to read, so `scan_one` should fetch with
    /// `ScanFetchMode::Full` on the background walk regardless of
    /// `ScanConfig::cache_full`. Default: no.
    fn wants_full_audio(&self) -> bool {
        false
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

/// How a scan fetch is allowed to touch the network. Replaces a plain
/// `cache_full: bool` so the prioritized now-playing path can forbid
/// fetching outright rather than merely skipping the post-read drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanFetchMode {
    /// Fetch if needed; once obtained, keep draining to the end so the
    /// backend commits the whole file to its on-disk cache. Used, per
    /// `ScanConfig::cache_full`, for the general background walk's
    /// per-plugin fetches.
    Full,
    /// Fetch if needed, but don't force a drain beyond what the plugin
    /// itself reads.
    Partial,
    /// Never touch the network. Return audio only if it's already fully on
    /// disk; otherwise behave exactly like "no audio available". Used
    /// exclusively for the prioritized now-playing path: the live player is
    /// the only thing allowed to originate a network fetch for a track
    /// that's actively loading/playing, so scanning just waits for that
    /// fetch to land it in cache.
    CacheOnly,
}

/// Best-effort seekable audio for `rendition`: a `MediaCache` hit first (any
/// source), then its `MediaProvider` (a local file / HTTP source), then
/// falls back to `Player::open_for_scan` (Spotify has no `MediaProvider` at
/// all, so this is its only path). `None` when nothing can supply bytes.
/// `CacheOnly` never originates a live fetch through either path — a
/// `MediaProvider`-backed source simply has nothing further to try once the
/// cache misses (there's no raw-bytes cache to fall back to any more); a
/// `Player` may still have something of its own (e.g. Spotify's librespot
/// file cache from a live session), so `mode` is passed through to it as-is.
pub fn open_scan_audio(
    rendition: &Rendition,
    media: &HashMap<SourceId, Arc<dyn MediaProvider>>,
    players: &HashMap<SourceId, Arc<dyn Player>>,
    media_cache: &MediaCache,
    mode: ScanFetchMode,
) -> Option<Box<dyn ReadSeek + Send>> {
    if let Some(p) = media_cache.cached_path(&rendition.source, &rendition.uri) {
        return match std::fs::File::open(&p) {
            Ok(f) => Some(Box::new(f) as Box<dyn ReadSeek + Send>),
            Err(e) => {
                log::warn!("scan: cached file {} unreadable: {e}", p.display());
                None
            }
        };
    }
    let source = &rendition.source;
    if let Some(mp) = media.get(source) {
        if mode == ScanFetchMode::CacheOnly {
            log::debug!("scan: {source} {} not yet cached, skipping (CacheOnly)", rendition.uri);
            return None;
        }
        match mp.materialize(rendition) {
            Ok(r) => Some(r),
            Err(e) => {
                log::warn!("scan: {source} media provider couldn't materialize {}: {e}", rendition.uri);
                None
            }
        }
    } else if let Some(p) = players.get(source) {
        match p.open_for_scan(rendition, mode) {
            Ok(r) => Some(r),
            Err(e) => {
                // `CacheOnly` never originates a fetch, so "nothing cached
                // yet" is the expected outcome whenever this rendition
                // just isn't the one actually playing (same case the
                // `MediaProvider` branch above already logs at `debug!`, not
                // `warn!`) — only a genuine fetch/decrypt failure in
                // Full/Partial mode is worth a warning.
                if mode == ScanFetchMode::CacheOnly {
                    log::debug!("scan: {source} player couldn't open {} for scan (CacheOnly): {e}", rendition.uri);
                } else {
                    log::warn!("scan: {source} player couldn't open {} for scan: {e}", rendition.uri);
                }
                None
            }
        }
    } else {
        log::debug!("scan: no media provider or player registered for source {source}");
        None
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
    /// `ScanConfig::cache_full` — applied to background-walk fetches only;
    /// the prioritized now-playing path always uses `ScanFetchMode::CacheOnly`
    /// regardless, never `Full`/`Partial`.
    cache_full: bool,
    paused: AtomicBool,
    /// Track ids that jumped the queue (currently-playing), oldest first.
    priority: Mutex<VecDeque<TrackId>>,
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
    /// The live, currently-registered set — seeded from `spawn`'s initial
    /// maps, then kept current by `ScanDriver::update_wiring` as later
    /// `Plugin::setup()` completions register a source's player/media
    /// provider. `run`'s walk thread re-reads this every tick instead of a
    /// frozen snapshot captured at spawn time, so a source wired up after
    /// startup (e.g. a deferred OAuth login) is picked up without a restart.
    media: Mutex<HashMap<SourceId, Arc<dyn MediaProvider>>>,
    players: Mutex<HashMap<SourceId, Arc<dyn Player>>>,
}

/// One background thread, shared by every registered plugin.
pub struct ScanDriver {
    inner: Arc<Inner>,
}

impl ScanDriver {
    pub fn new(plugins: Vec<Arc<dyn ScanPlugin>>, cache_full: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                plugins: Mutex::new(plugins),
                cache_full,
                paused: AtomicBool::new(false),
                priority: Mutex::new(VecDeque::new()),
                last_run: Mutex::new(HashMap::new()),
                status: Mutex::new(HashMap::new()),
                view: Mutex::new(None),
                idle_since: Mutex::new(None),
                failures: Mutex::new(HashMap::new()),
                failure_cooldown: Mutex::new(HashMap::new()),
                media: Mutex::new(HashMap::new()),
                players: Mutex::new(HashMap::new()),
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
        media: HashMap<SourceId, Arc<dyn MediaProvider>>,
        players: HashMap<SourceId, Arc<dyn Player>>,
        media_cache: Arc<MediaCache>,
    ) {
        *self.inner.media.lock().unwrap() = media;
        *self.inner.players.lock().unwrap() = players;
        let inner = self.inner.clone();
        thread::Builder::new()
            .name("scan-driver".into())
            .spawn(move || run(inner, catalog, store, media_cache))
            .expect("failed to spawn scan-driver thread");
    }

    /// Registers a source's newly-available player/media-provider after
    /// startup (called wherever `Session::apply_wiring` runs) — mirrors
    /// `apply_wiring`'s own "each field independently, `None` leaves the
    /// existing entry untouched" semantics, so a `setup()` that only
    /// produces a player doesn't wipe out an already-working media-provider
    /// entry for the same source, or vice versa.
    pub fn update_wiring(
        &self,
        id: SourceId,
        media: Option<Arc<dyn MediaProvider>>,
        player: Option<Arc<dyn Player>>,
    ) {
        if let Some(m) = media {
            self.inner.media.lock().unwrap().insert(id.clone(), m);
        }
        if let Some(p) = player {
            self.inner.players.lock().unwrap().insert(id, p);
        }
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
        if !q.contains(&track) {
            q.push_front(track);
        }
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

    /// Runtime pause/resume (`B` / `:togglescan`) — stops/restarts the walk
    /// without dropping anything already scanned. Also drains `priority`
    /// and clears any in-flight `Downloading` status so pausing fully
    /// inhibits the scanner rather than just skipping the next tick.
    pub fn set_paused(&self, paused: bool) {
        self.inner.paused.store(paused, Ordering::Relaxed);
        if paused {
            self.inner.priority.lock().unwrap().clear();
            self.inner.status.lock().unwrap().retain(|_, v| *v != ScanStatus::Downloading);
        }
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Relaxed)
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
    media: &'a HashMap<SourceId, Arc<dyn MediaProvider>>,
    players: &'a HashMap<SourceId, Arc<dyn Player>>,
    media_cache: &'a MediaCache,
}

fn run(
    inner: Arc<Inner>,
    catalog: Arc<Catalog>,
    store: Arc<dyn Store>,
    media_cache: Arc<MediaCache>,
) {
    loop {
        thread::sleep(TICK);
        if inner.paused.load(Ordering::Relaxed) {
            continue;
        }

        // Cloned fresh every tick (cheap: `Arc` clones) rather than a
        // snapshot moved in at spawn time, so a source wired up after
        // startup (`ScanDriver::update_wiring`) is visible on the very next
        // tick instead of never.
        let media = inner.media.lock().unwrap().clone();
        let players = inner.players.lock().unwrap().clone();
        let plugins = inner.plugins.lock().unwrap().clone();
        let ctx = ScanCtx { media: &media, players: &players, media_cache: &media_cache };

        let prio = inner.priority.lock().unwrap().pop_front();
        if let Some(id) = prio {
            match store.get_track(id) {
                Ok(Some(track)) => {
                    if scan_one(&inner, &plugins, &catalog, &ctx, &track, true) {
                        continue;
                    }
                    // Still needs a plugin, just blocked on that plugin's
                    // own cooldown right now (min_interval or failure
                    // backoff) — requeue so it gets first crack again next
                    // tick, rather than silently dropping the priority jump
                    // and falling back to whatever order the general walk
                    // happens to reach it in.
                    if any_plugin_needs(&inner, &plugins, &track) {
                        inner.priority.lock().unwrap().push_front(id);
                    } else {
                        log::debug!("scan: priority track {id:?} has nothing left to do");
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
        let Some(tracks) = resolve_walk_list(&inner, &store) else {
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
            if scan_one(&inner, &plugins, &catalog, &ctx, track, false) {
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
fn resolve_walk_list(inner: &Inner, store: &Arc<dyn Store>) -> Option<Vec<Track>> {
    if let Some(view) = inner.view.lock().unwrap().clone() {
        let n = view.tracks.len();
        let start = view.highlighted % n;
        let mut out = Vec::with_capacity(n);
        for offset in 0..n {
            let id = view.tracks[(start + offset) % n];
            if let Ok(Some(track)) = store.get_track(id) {
                out.push(track);
            }
        }
        return Some(out);
    }
    store.all_tracks().ok()
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

/// Run the first plugin that both `needs()` `track` and is off its own
/// `min_interval` cooldown. Returns whether a plugin was actually invoked.
/// `priority`: this is the currently-playing-track fast path, which always
/// fetches with `ScanFetchMode::CacheOnly` — never `Full`/`Partial` —
/// so scanning never races the live player's own fetch of the same file.
fn scan_one(
    inner: &Inner,
    plugins: &[Arc<dyn ScanPlugin>],
    catalog: &Arc<Catalog>,
    ctx: &ScanCtx,
    track: &Track,
    priority: bool,
) -> bool {
    for plugin in plugins {
        if !plugin.needs(track) {
            continue;
        }
        // A prior `Outcome::Skip` means "not this plugin's job" — honour
        // that for the rest of this run rather than re-attempting it every
        // `min_interval`, which would otherwise waste a cooldown slot
        // forever on a track this plugin will never handle.
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
        {
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

        let mode = if priority {
            ScanFetchMode::CacheOnly
        } else if inner.cache_full || plugin.wants_full_audio() {
            ScanFetchMode::Full
        } else {
            ScanFetchMode::Partial
        };
        log::debug!(
            "scan[{}]: attempting \"{}\" ({:?}, {mode:?}{})",
            plugin.id(),
            track.title,
            track.id,
            if priority { ", priority" } else { "" }
        );
        // Lazy: invoking this is what actually fetches/decrypts over the
        // network, so a plugin should only call it on a `media_cache` miss —
        // a cache hit must cost nothing beyond the disk read.
        let open_audio = || -> Option<(Rendition, Box<dyn ReadSeek + Send>)> {
            let found = track.renditions.iter().find_map(|r| {
                open_scan_audio(r, ctx.media, ctx.players, ctx.media_cache, mode).map(|a| (r.clone(), a))
            });
            if found.is_none() {
                log::debug!(
                    "scan[{}]: no audio available for \"{}\" ({:?}) across {} rendition(s)",
                    plugin.id(),
                    track.title,
                    track.id,
                    track.renditions.len()
                );
            }
            found
        };
        match plugin.analyze(track, &open_audio, ctx.media_cache) {
            Outcome::Done(meta) => {
                log::debug!(
                    "scan[{}]: done with \"{}\" ({:?}): {:?}",
                    plugin.id(),
                    track.title,
                    track.id,
                    meta.attrs
                );
                // `patch` re-reads the track under `Catalog`'s lock and merges
                // into whatever's current, not into this (possibly stale)
                // `track` snapshot — see the comment on `Catalog::lock`.
                let _ = catalog.patch(track.id, |t| t.attrs.extend(meta.attrs));
                inner.status.lock().unwrap().remove(&(plugin.id(), track.id));
                inner.failures.lock().unwrap().remove(&(plugin.id(), track.id));
            }
            Outcome::Skip => {
                log::debug!(
                    "scan[{}]: skipping \"{}\" ({:?}) — not applicable",
                    plugin.id(),
                    track.title,
                    track.id
                );
                inner.status.lock().unwrap().insert((plugin.id(), track.id), ScanStatus::Skipped);
            }
            Outcome::Retry => {
                log::warn!(
                    "scan[{}]: retrying \"{}\" ({:?}) later — transient failure",
                    plugin.id(),
                    track.title,
                    track.id
                );
                inner.status.lock().unwrap().insert((plugin.id(), track.id), ScanStatus::Error);
                let mut failures = inner.failures.lock().unwrap();
                let count = failures.entry((plugin.id(), track.id)).or_insert(0);
                *count += 1;
                if *count >= FAILURE_THRESHOLD {
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

