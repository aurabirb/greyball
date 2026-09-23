//! Background scan plugins: a shared driver walks the library and lets
//! registered plugins compute per-track metadata (bpm, later genre/mood/...)
//! into the generic `Track::attrs` map.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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

    /// Cheap, no I/O: does this track still need this plugin's data?
    fn needs(&self, track: &Track) -> bool;

    /// Blocking analysis of one track. `audio` opens the track's stream (a cached file is a complete
    /// one, a download in progress is read as it grows); `Err` is the outcome to return when there is
    /// none. A stream that fails or is cancelled under the decode is `Outcome::Retry`, never `Skip`;
    /// so is a decode abandoned because `wanted` turned false (poll it between blocks).
    fn analyze(&self, track: &Track, audio: &dyn Fn() -> Result<StreamHandle, Outcome>, wanted: &dyn Fn() -> bool) -> Outcome;

    /// Minimum spacing between this plugin's own background fetches — keeps
    /// a slow/rate-limited source from being hammered.
    fn min_interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    /// False for a plugin that never opens audio, so it skips the stream gate.
    fn needs_audio(&self) -> bool {
        true
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
    /// Both workers do nothing.
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

/// A track that has failed (`Outcome::Retry`) this many times in a row for the same plugin waits
/// `FAILURE_COOLDOWN` instead of `RETRY_SPACING` before the next try, so a permanently-broken track
/// (e.g. no fetchable audio) stops spending walk slots.
const FAILURE_THRESHOLD: u32 = 3;
const RETRY_SPACING: Duration = Duration::from_secs(15);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// Two walk passes are at least this far apart: a burst of track updates (a crawl ingesting hits)
/// costs one pass, not one per update.
const PASS_SPACING: Duration = Duration::from_secs(1);

/// How long the now-playing worker keeps retrying a track whose stream cannot be opened yet (not
/// playing, not cached) before leaving it to the walk's own cadence, and how often it retries.
const PRIORITY_RETRY_TIMEOUT: Duration = Duration::from_secs(10);
const PRIORITY_RETRY_TICK: Duration = Duration::from_millis(500);

/// How long a stream may take to deliver its first byte before the attempt is a `Retry`.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(60);

/// After a failed fetch a source is left alone for `SOURCE_BACKOFF << (failures - 1)`, capped at
/// `SOURCE_BACKOFF_MAX`, so a run of bad tracks cannot hammer it (Spotify recycles its session after
/// two failed opens in a row, which would interrupt playback).
const SOURCE_BACKOFF: Duration = Duration::from_secs(60);
const SOURCE_BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// One plugin's claim on one track.
type Job = (&'static str, TrackId);

/// The order + position the UI's current view reports via
/// `ScanDriver::follow_view` — the walk's primary traversal order once set.
#[derive(Clone, PartialEq, Eq)]
struct ViewOrder {
    tracks: Vec<TrackId>,
    highlighted: usize,
}

/// What the walk thread has been told since it last woke.
#[derive(Default)]
struct WalkInbox {
    woken: bool,
    /// Tracks to re-read from the store (updated, added or removed).
    dirty: Vec<TrackId>,
    /// Re-read the whole library (a plugin was registered).
    rebuild: bool,
}

struct Inner {
    /// Grows via `ScanDriver::register_plugin` as sources finish async
    /// setup. Cloned fresh per pass rather than locked across `analyze`.
    plugins: Mutex<Vec<Arc<dyn ScanPlugin>>>,
    mode: AtomicU8,
    /// Now-playing tracks, newest first, each with when it was first prioritized.
    priority: Mutex<VecDeque<(TrackId, Instant)>>,
    priority_wake: Condvar,
    walk: Mutex<WalkInbox>,
    walk_wake: Condvar,
    last_run: Mutex<HashMap<&'static str, Instant>>,
    /// Per-(plugin, track) live status, for `ScanDriver::status` — cleared on `Outcome::Done` since a
    /// resolved value already lives in `Track::attrs`. `Downloading` is also the in-flight marker
    /// that keeps the two workers off the same (plugin, track).
    status: Mutex<HashMap<Job, ScanStatus>>,
    /// The UI's current view/selection, see `ScanDriver::follow_view`. `None`
    /// while no view has reported one yet, or the last-reported view had no
    /// tracks — the walk covers the whole library in that case.
    view: Mutex<Option<ViewOrder>>,
    /// Consecutive `Outcome::Retry` count per (plugin, track) and when the last one happened;
    /// removed on `Outcome::Done`.
    failures: Mutex<HashMap<Job, (u32, Instant)>>,
    /// Background fetches the driver keeps alive until they finish, so a walk download runs to `Done`.
    held: Mutex<Vec<(StreamHandle, Claim)>>,
    /// Per source: consecutive failed fetches and when it may be fetched from again.
    source_backoff: Mutex<HashMap<SourceId, (u32, Instant)>>,
}

impl Inner {
    fn mode(&self) -> ScanMode {
        ScanMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    fn wake_walk(&self) {
        self.walk.lock().unwrap().woken = true;
        self.walk_wake.notify_one();
    }
}

/// Two threads shared by every registered plugin: the walk (background, one download at a time)
/// and the now-playing worker, which only ever peeks at a running or cached stream so it never
/// waits behind a long background analysis.
pub struct ScanDriver {
    inner: Arc<Inner>,
}

impl ScanDriver {
    pub fn new(plugins: Vec<Arc<dyn ScanPlugin>>, mode: ScanMode) -> Self {
        Self {
            inner: Arc::new(Inner {
                plugins: Mutex::new(plugins),
                mode: AtomicU8::new(mode.to_u8()),
                priority: Mutex::new(VecDeque::new()),
                priority_wake: Condvar::new(),
                walk: Mutex::new(WalkInbox { woken: true, dirty: Vec::new(), rebuild: true }),
                walk_wake: Condvar::new(),
                last_run: Mutex::new(HashMap::new()),
                status: Mutex::new(HashMap::new()),
                view: Mutex::new(None),
                failures: Mutex::new(HashMap::new()),
                held: Mutex::new(Vec::new()),
                source_backoff: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Spawns both workers. Writes go through `catalog` (`Catalog::patch`, see the lock on `Catalog`
    /// itself) so a scan result can never race a concurrent UI-thread `ingest`/`patch` into a lost
    /// update; `store` only serves reads that pick what to look at next.
    pub fn spawn(
        &self,
        catalog: Arc<Catalog>,
        store: Arc<dyn Store>,
        engine: StreamEngine,
        media_cache: Arc<MediaCache>,
    ) {
        let walk = Worker { inner: self.inner.clone(), catalog: catalog.clone(), store: store.clone(), engine: engine.clone(), media_cache: media_cache.clone() };
        thread::Builder::new()
            .name("scan-driver".into())
            .spawn(move || walk.run_walk())
            .expect("failed to spawn scan-driver thread");
        let priority = Worker { inner: self.inner.clone(), catalog, store, engine, media_cache };
        thread::Builder::new()
            .name("scan-priority".into())
            .spawn(move || priority.run_priority())
            .expect("failed to spawn scan-priority thread");
    }

    /// Adds a plugin to the already-running driver, for one whose
    /// availability depends on setup finishing rather than being known at
    /// construction time.
    pub fn register_plugin(&self, plugin: Arc<dyn ScanPlugin>) {
        self.inner.plugins.lock().unwrap().push(plugin);
        self.inner.walk.lock().unwrap().rebuild = true;
        self.inner.wake_walk();
    }

    /// The currently-playing track goes to the now-playing worker for every plugin that still
    /// `needs()` it, preempting whatever that worker was analyzing. Call from wherever `Session`
    /// reacts to `PlayerEvent::Playing` and `Stream{Done}`.
    pub fn prioritize(&self, track: TrackId) {
        let mut q = self.inner.priority.lock().unwrap();
        // A fresh call (e.g. `Stream{Done}` after `Playing` already queued it) restarts the retry window.
        q.retain(|(t, _)| *t != track);
        q.push_front((track, Instant::now()));
        self.inner.priority_wake.notify_one();
    }

    /// `track` was written, added or removed: the walk re-reads it.
    pub fn track_changed(&self, track: TrackId) {
        let mut walk = self.inner.walk.lock().unwrap();
        walk.dirty.push(track);
        walk.woken = true;
        self.inner.walk_wake.notify_one();
    }

    /// A stream reached a terminal state: a held fetch may be over, so the walk can start the next.
    pub fn stream_ended(&self) {
        self.inner.wake_walk();
    }

    /// Reports "the list the user is currently looking at, and which row is
    /// highlighted" — the walk then proceeds from `highlighted`, forward,
    /// wrapping to the start, instead of the whole library in arbitrary
    /// order. Call whenever the UI's active screen/selection changes. An empty `tracks` clears it,
    /// falling back to the full-library walk (e.g. a screen with no track list, like Playlists' own
    /// folder view).
    pub fn follow_view(&self, tracks: Vec<TrackId>, highlighted: usize) {
        let next = if tracks.is_empty() { None } else { Some(ViewOrder { tracks, highlighted }) };
        let mut view = self.inner.view.lock().unwrap();
        if *view != next {
            *view = next;
            drop(view);
            self.inner.wake_walk();
        }
    }

    /// Sets the driver's mode (`B` / `:togglescan` cycles it). Leaving `Active` drops the held
    /// fetch claims so a background download stops; `Disabled` also drains `priority` and makes
    /// every running analysis give up.
    pub fn set_mode(&self, mode: ScanMode) {
        self.inner.mode.store(mode.to_u8(), Ordering::Relaxed);
        if mode == ScanMode::Disabled {
            self.inner.priority.lock().unwrap().clear();
        }
        reap_held(&self.inner, mode);
        self.inner.wake_walk();
        self.inner.priority_wake.notify_one();
    }

    pub fn mode(&self) -> ScanMode {
        self.inner.mode()
    }

    /// Live status of `plugin_id`'s last/current attempt at `track`. `None`
    /// means "waiting" — never attempted yet, or the previous attempt
    /// succeeded (in which case the result is in `Track::attrs` instead).
    pub fn status(&self, plugin_id: &'static str, track: TrackId) -> Option<ScanStatus> {
        self.inner.status.lock().unwrap().get(&(plugin_id, track)).copied()
    }
}

/// What one attempt at a track came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// A plugin ran.
    Attempted,
    /// Blocked by a timer (cooldown, backoff, `min_interval`) until then.
    NotBefore(Instant),
    /// Blocked on an event: another download is held, the other worker has it, or no stream exists yet.
    Waiting,
    /// No plugin has anything left to do for this track.
    Nothing,
}

/// Everything a worker thread needs; one per thread.
struct Worker {
    inner: Arc<Inner>,
    catalog: Arc<Catalog>,
    store: Arc<dyn Store>,
    engine: StreamEngine,
    media_cache: Arc<MediaCache>,
}

impl Worker {
    /// The background walk: sleeps until told something changed (or a timer runs out), then makes
    /// one pass over the candidates in view order and attempts the first one that can go.
    fn run_walk(&self) {
        // Every track some plugin still `needs()`; kept in memory and patched per `track_changed`.
        let mut candidates: HashMap<TrackId, Track> = HashMap::new();
        let mut next_at: Option<Instant> = None;
        let mut earliest = Instant::now();
        loop {
            let inbox = self.wait_walk(next_at, earliest);
            let mode = self.inner.mode();
            reap_held(&self.inner, mode);
            let plugins = self.inner.plugins.lock().unwrap().clone();
            let needed = |t: &Track| plugins.iter().any(|p| p.needs(t));
            if inbox.rebuild {
                candidates.clear();
                if !plugins.is_empty() {
                    match self.store.all_tracks() {
                        Ok(all) => candidates.extend(all.into_iter().filter(|t| needed(t)).map(|t| (t.id, t))),
                        Err(e) => log::warn!("scan: listing the library failed: {e}"),
                    }
                    log::debug!("scan: {} track(s) still need analysis", candidates.len());
                }
            }
            for id in inbox.dirty {
                match self.store.get_track(id) {
                    Ok(Some(t)) if needed(&t) => {
                        candidates.insert(id, t);
                    }
                    Ok(_) => {
                        candidates.remove(&id);
                    }
                    Err(e) => log::warn!("scan: reading {id:?} failed: {e}"),
                }
            }
            next_at = None;
            earliest = Instant::now() + PASS_SPACING;
            if mode == ScanMode::Disabled {
                continue;
            }
            let mut done: Vec<TrackId> = Vec::new();
            for id in self.walk_order(&candidates) {
                let Some(track) = candidates.get(&id) else { continue };
                match self.scan_one(&plugins, track, Access::from_mode(mode), &|| self.inner.mode() != ScanMode::Disabled) {
                    Verdict::Attempted => {
                        // Re-read what the attempt wrote before its `TrackUpdated` comes back, then pass again.
                        self.inner.walk.lock().unwrap().dirty.push(id);
                        self.inner.wake_walk();
                        break;
                    }
                    Verdict::NotBefore(t) => next_at = Some(next_at.map_or(t, |n| n.min(t))),
                    Verdict::Waiting => {}
                    Verdict::Nothing => done.push(id),
                }
            }
            for id in done {
                candidates.remove(&id);
            }
        }
    }

    /// Blocks until woken (not before `earliest`) or `next_at` passes; hands back what arrived.
    fn wait_walk(&self, next_at: Option<Instant>, earliest: Instant) -> WalkInbox {
        let mut walk = self.inner.walk.lock().unwrap();
        loop {
            let now = Instant::now();
            if next_at.is_some_and(|t| t <= now) || (walk.woken && earliest <= now) {
                walk.woken = false;
                return std::mem::take(&mut walk);
            }
            let deadline = match (walk.woken, next_at) {
                (true, Some(t)) => Some(earliest.min(t)),
                (true, None) => Some(earliest),
                (false, t) => t,
            };
            walk = match deadline {
                Some(t) => self.inner.walk_wake.wait_timeout(walk, t - now).unwrap().0,
                None => self.inner.walk_wake.wait(walk).unwrap(),
            };
        }
    }

    /// The candidates in the order the walk tries them: the UI-reported view from its highlighted
    /// row, wrapping, if one is set; otherwise all of them.
    fn walk_order(&self, candidates: &HashMap<TrackId, Track>) -> Vec<TrackId> {
        match self.inner.view.lock().unwrap().as_ref() {
            Some(view) => {
                let n = view.tracks.len();
                let start = view.highlighted % n;
                let mut seen = HashSet::new();
                (0..n)
                    .map(|offset| view.tracks[(start + offset) % n])
                    .filter(|id| candidates.contains_key(id) && seen.insert(*id))
                    .collect()
            }
            None => candidates.keys().copied().collect(),
        }
    }

    /// The now-playing worker: analyzes whatever `prioritize` queued, newest first, peeking at the
    /// running or cached stream. A newer track preempts the analysis in progress.
    fn run_priority(&self) {
        loop {
            let (id, since) = {
                let mut q = self.inner.priority.lock().unwrap();
                loop {
                    if let Some(front) = q.pop_front() {
                        break front;
                    }
                    q = self.inner.priority_wake.wait(q).unwrap();
                }
            };
            let track = match self.store.get_track(id) {
                Ok(Some(track)) => track,
                Ok(None) => {
                    log::debug!("scan: priority track {id:?} not found in store");
                    continue;
                }
                Err(e) => {
                    log::warn!("scan: priority lookup of {id:?} failed: {e}");
                    continue;
                }
            };
            let plugins = self.inner.plugins.lock().unwrap().clone();
            let wanted = || {
                self.inner.mode() != ScanMode::Disabled && self.inner.priority.lock().unwrap().front().is_none_or(|(t, _)| *t == id)
            };
            let mut waiting = false;
            for plugin in &plugins {
                match self.scan_one(std::slice::from_ref(plugin), &track, Access::Peek, &wanted) {
                    Verdict::Waiting => waiting = true,
                    Verdict::Attempted if !wanted() => break,
                    _ => {}
                }
            }
            if !waiting || !wanted() {
                continue;
            }
            if since.elapsed() >= PRIORITY_RETRY_TIMEOUT {
                log::debug!("scan: priority track {id:?} still waiting after {PRIORITY_RETRY_TIMEOUT:?}, leaving it to the walk");
                continue;
            }
            let mut q = self.inner.priority.lock().unwrap();
            if q.is_empty() {
                q.push_back((id, since));
                drop(self.inner.priority_wake.wait_timeout(q, PRIORITY_RETRY_TICK).unwrap());
            }
        }
    }

    /// Whether a fetch for `track` may start now: no download is being kept alive (one at a time)
    /// and one of its sources is not backed off. A cached or already-running rendition needs none.
    fn fetchable(&self, track: &Track) -> Result<(), Verdict> {
        let held = self.inner.held.lock().unwrap();
        if track.renditions.iter().any(|r| held.iter().any(|(h, _)| *h.key() == (r.source.clone(), r.uri.clone()))) {
            return Ok(());
        }
        if !held.is_empty() {
            return Err(Verdict::Waiting);
        }
        let backoff = self.inner.source_backoff.lock().unwrap();
        let now = Instant::now();
        let mut soonest: Option<Instant> = None;
        for r in &track.renditions {
            match backoff.get(&r.source) {
                Some((_, until)) if *until > now => soonest = Some(soonest.map_or(*until, |s| s.min(*until))),
                _ => return Ok(()),
            }
        }
        Err(soonest.map_or(Verdict::Nothing, Verdict::NotBefore))
    }

    /// Whether some rendition of `track` is cached or downloading right now.
    fn has_stream(&self, track: &Track) -> bool {
        track.renditions.iter().any(|r| {
            self.media_cache.cached_path(&r.source, &r.uri).is_some() || self.engine.is_running(&(r.source.clone(), r.uri.clone()))
        })
    }

    /// Opens `track`'s audio as a stream: a cached rendition first, else per `access`. Records the stream in `used`.
    fn open_stream(&self, track: &Track, access: Access, used: &RefCell<Option<StreamHandle>>) -> Result<StreamHandle, Outcome> {
        let mut renditions: Vec<&Rendition> = track.renditions.iter().collect();
        renditions.sort_by_key(|r| self.media_cache.cached_path(&r.source, &r.uri).is_none());
        for r in renditions {
            let intent = if access == Access::Fetch { Intent::Fetch } else { Intent::Peek };
            let (handle, claim) = match self.engine.open(r, intent) {
                Ok(opened) => opened,
                Err(Error::NotFound) => continue,
                Err(e) => {
                    log::warn!("scan: {} couldn't open {}: {e}", r.source, r.uri);
                    continue;
                }
            };
            *used.borrow_mut() = Some(handle.clone());
            if intent == Intent::Fetch && handle.info().state != StreamState::Done {
                self.inner.held.lock().unwrap().push((handle.clone(), claim));
            }
            if !handle.wait_range(0..1, FIRST_BYTE_TIMEOUT) {
                self.inner.held.lock().unwrap().retain(|(h, _)| h.key() != handle.key());
                return Err(Outcome::Retry);
            }
            return Ok(handle);
        }
        Err(Outcome::Skip)
    }

    /// A failed fetch backs its source off; a finished one clears it.
    fn note_source(&self, source: &SourceId, failed: bool) {
        let mut backoff = self.inner.source_backoff.lock().unwrap();
        if !failed {
            backoff.remove(source);
            return;
        }
        let count = backoff.get(source).map_or(0, |(n, _)| *n) + 1;
        let wait = (SOURCE_BACKOFF * 2u32.saturating_pow(count - 1)).min(SOURCE_BACKOFF_MAX);
        log::debug!("scan: {source} fetch failed {count} time(s) in a row, leaving it alone for {wait:?}");
        backoff.insert(source.clone(), (count, Instant::now() + wait));
    }

    /// Runs the first plugin that `needs()` `track` and can go right now; otherwise why none could.
    fn scan_one(&self, plugins: &[Arc<dyn ScanPlugin>], track: &Track, access: Access, wanted: &dyn Fn() -> bool) -> Verdict {
        let mut verdict = Verdict::Nothing;
        let mut block = |v: Verdict| {
            verdict = match (verdict, v) {
                (Verdict::NotBefore(a), Verdict::NotBefore(b)) => Verdict::NotBefore(a.min(b)),
                (Verdict::NotBefore(a), _) | (_, Verdict::NotBefore(a)) => Verdict::NotBefore(a),
                (Verdict::Waiting, _) | (_, Verdict::Waiting) => Verdict::Waiting,
                _ => Verdict::Nothing,
            }
        };
        let mut has_stream: Option<bool> = None;
        for plugin in plugins {
            if !plugin.needs(track) {
                continue;
            }
            let key = (plugin.id(), track.id);
            match self.inner.status.lock().unwrap().get(&key) {
                // A prior `Outcome::Skip` means "not this plugin's job" for the rest of this run.
                Some(ScanStatus::Skipped) => continue,
                Some(ScanStatus::Downloading) => {
                    block(Verdict::Waiting);
                    continue;
                }
                _ => {}
            }
            if let Some((count, at)) = self.inner.failures.lock().unwrap().get(&key) {
                let wait = if *count >= FAILURE_THRESHOLD { FAILURE_COOLDOWN } else { RETRY_SPACING };
                if at.elapsed() < wait {
                    block(Verdict::NotBefore(*at + wait));
                    continue;
                }
            }
            let gated = plugin.needs_audio() && !*has_stream.get_or_insert_with(|| self.has_stream(track));
            if gated {
                if access == Access::Peek {
                    block(Verdict::Waiting);
                    continue;
                }
                if let Err(v) = self.fetchable(track) {
                    block(v);
                    continue;
                }
            }
            // `min_interval` spaces real downloads and API calls; a cached or running stream costs the source nothing.
            let mut last_run: Option<MutexGuard<HashMap<&'static str, Instant>>> = None;
            if gated || !plugin.needs_audio() {
                let guard = self.inner.last_run.lock().unwrap();
                if let Some(t) = guard.get(plugin.id())
                    && t.elapsed() < plugin.min_interval()
                {
                    block(Verdict::NotBefore(*t + plugin.min_interval()));
                    continue;
                }
                last_run = Some(guard);
            }
            {
                let mut status = self.inner.status.lock().unwrap();
                if status.get(&key) == Some(&ScanStatus::Downloading) {
                    block(Verdict::Waiting);
                    continue;
                }
                status.insert(key, ScanStatus::Downloading);
            }
            if let Some(mut guard) = last_run {
                guard.insert(plugin.id(), Instant::now());
            }

            log::debug!("scan[{}]: attempting \"{}\" ({:?}, {access:?})", plugin.id(), track.title, track.id);
            let used: RefCell<Option<StreamHandle>> = RefCell::new(None);
            let open_audio = || self.open_stream(track, access, &used);
            let outcome = plugin.analyze(track, &open_audio, wanted);
            let stream = used.into_inner();
            let source = stream.as_ref().map(|h| h.key().0.clone());
            match outcome {
                Outcome::Done(meta) => {
                    log::debug!("scan[{}]: done with \"{}\" ({:?}): {:?}", plugin.id(), track.title, track.id, meta.attrs.keys().collect::<Vec<_>>());
                    // `patch` re-reads the track under `Catalog`'s lock and merges into whatever's current.
                    let _ = self.catalog.patch(track.id, |t| t.attrs.extend(meta.attrs));
                    self.inner.status.lock().unwrap().remove(&key);
                    self.inner.failures.lock().unwrap().remove(&key);
                    if let Some(source) = &source {
                        self.note_source(source, false);
                    }
                }
                Outcome::Skip if plugin.needs_audio() && stream.is_none() && access == Access::Peek => {
                    // No stream to peek at (not playing, not cached): not a verdict.
                    log::debug!("scan[{}]: \"{}\" ({:?}) has no cached or running stream yet", plugin.id(), track.title, track.id);
                    self.inner.status.lock().unwrap().remove(&key);
                    block(Verdict::Waiting);
                    continue;
                }
                Outcome::Skip => {
                    log::debug!("scan[{}]: skipping \"{}\" ({:?}) — not applicable", plugin.id(), track.title, track.id);
                    self.inner.status.lock().unwrap().insert(key, ScanStatus::Skipped);
                    // A real fetch may have cached audio even with nothing to report — announce it.
                    self.catalog.announce(track.id);
                }
                Outcome::Retry if !wanted() || stream.as_ref().is_some_and(|h| matches!(h.info().state, StreamState::Cancelled | StreamState::Connecting)) => {
                    // Skipped, paused, preempted mid-analysis or still waiting on the source's link: not a failure, tried again later.
                    log::debug!("scan[{}]: \"{}\" ({:?}) abandoned, trying again later", plugin.id(), track.title, track.id);
                    self.inner.status.lock().unwrap().remove(&key);
                }
                Outcome::Retry => {
                    log::warn!("scan[{}]: retrying \"{}\" ({:?}) later — transient failure", plugin.id(), track.title, track.id);
                    self.inner.status.lock().unwrap().insert(key, ScanStatus::Error);
                    if access == Access::Fetch
                        && let Some(source) = &source
                    {
                        self.note_source(source, true);
                    }
                    let mut failures = self.inner.failures.lock().unwrap();
                    let (count, at) = failures.entry(key).or_insert((0, Instant::now()));
                    *count += 1;
                    *at = Instant::now();
                    if *count >= FAILURE_THRESHOLD {
                        // No track name, so every track that gives up shares one warnings row.
                        self.catalog.warn(plugin.id(), "scan keeps failing for some tracks — see :log".to_string());
                    }
                }
            }
            return Verdict::Attempted;
        }
        verdict
    }
}

impl Access {
    fn from_mode(mode: ScanMode) -> Self {
        if mode == ScanMode::Active { Access::Fetch } else { Access::Peek }
    }
}

/// Drops the claims of finished fetches, and all of them once the driver stops fetching.
fn reap_held(inner: &Inner, mode: ScanMode) {
    let mut held = inner.held.lock().unwrap();
    held.retain(|(h, _)| mode == ScanMode::Active && matches!(h.info().state, StreamState::Connecting | StreamState::Fetching | StreamState::Buffering));
}
