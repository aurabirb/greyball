//! `RodioPlayer` — real audio playback.
//!
//! `rodio`'s `OutputStream` wraps a `cpal::Stream`, which is `!Send`, so it
//! cannot live behind the `Arc<Mutex<..>>` a `Send + Sync` `Player` needs.
//! Instead a single long-lived worker thread owns *all* audio state
//! (`OutputStream`, `Sink`, the current tempfile) and the public methods just
//! post `Cmd`s to it over a channel. A separate `Arc<Mutex<Snapshot>>` carries
//! the audio-type-free status back for `status()`.
//!
//! `Cmd::Load` itself never runs on the worker thread: resolving the
//! rendition (a `MediaProvider::open`, possibly a full-file download over the
//! network) and opening the decoder happen on a throwaway background thread,
//! which reports back via `Cmd::Loaded`. This means the worker's `recv`
//! loop is never blocked on that work — a stalled/hanging source (a wedged
//! server, a `MediaProvider` that never returns) leaves the *load* stuck,
//! not the player: `Stop`, `Toggle`, a *different* `Load`, all still get
//! processed immediately. `generation` (bumped on every `Load`/`Stop`) tags
//! each background attempt so a `Loaded` that finally reports in after
//! something superseded it is just dropped, not applied.
//!
//! rodio 0.21 API notes:
//! - stream: `OutputStreamBuilder::open_default_stream()`, then `.mixer()`.
//! - sink: `Sink::connect_new(&mixer)` (no `OutputStreamHandle`).
//! - decode: `Decoder::try_from(File)` (was `Decoder::new(BufReader::new(..))`).
//! - `Sink::get_pos`, `try_seek`, `set_volume`, `empty`, `stop` unchanged.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::{
    Bus, CoreEvent, Media, MediaCache, MediaProvider, Player, PlayerEvent, PlayerState,
    PlayerStatus, Rendition, SourceId,
};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rodio::Source;
use tempfile::NamedTempFile;

use crate::{AudioTap, Snapshot, Tapped, clamp_volume};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Fallback source id (see `load`): a rendition with an unregistered source
/// still gets tried against the http provider. `SourceId` isn't const, so this
/// is a tiny constructor.
fn http_id() -> SourceId {
    SourceId::from("http")
}

enum Cmd {
    Load {
        rendition: Rendition,
        start_paused: bool,
        position_ms: u32,
    },
    /// Reported by the background thread `Load` spawns once resolving +
    /// opening the decoder finishes, one way or the other. Dropped if
    /// `generation` no longer matches `Audio::generation` — a later
    /// `Load` or a `Stop` superseded it while it was still in flight.
    Loaded {
        generation: u64,
        source: SourceId,
        uri: String,
        start_paused: bool,
        position_ms: u32,
        result: core::Result<LoadedTrack>,
    },
    Toggle,
    Seek(u32),
    SetVolume(f32),
    Stop,
}

/// What the background thread spawned by `Cmd::Load` hands back: everything
/// needed to start playback that doesn't touch the (worker-thread-only,
/// `!Send`) `OutputStream`/`Sink`.
struct LoadedTrack {
    tapped: Tapped<rodio::Decoder<std::io::BufReader<File>>>,
    duration_ms: u32,
    /// Kept alive so a downloaded/copied tempfile is not deleted mid-playback.
    temp: Option<NamedTempFile>,
}

pub struct RodioPlayer {
    inner: Arc<Mutex<Snapshot>>,
    tap: Arc<AudioTap>,
    tx: Sender<Cmd>,
}

impl RodioPlayer {
    pub fn new(
        media: HashMap<SourceId, Arc<dyn MediaProvider>>,
        bus: Bus,
        media_cache: Arc<MediaCache>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(Snapshot::default()));
        let tap = Arc::new(AudioTap::default());
        let (tx, rx) = unbounded();
        let worker_tx = tx.clone();
        let worker_inner = inner.clone();
        let worker_tap = tap.clone();
        std::thread::Builder::new()
            .name("rodio-player".into())
            .spawn(move || worker(rx, worker_tx, worker_inner, worker_tap, bus, media, media_cache))
            .expect("spawn rodio worker");
        Self { inner, tap, tx }
    }
}

impl Player for RodioPlayer {
    // MVP: accept everything and let `load` fail loudly on an unknown source.
    // This covers accepting any `Offline` rendition regardless of source —
    // imported `local` tracks route here.
    fn accepts(&self, _r: &Rendition) -> bool {
        true
    }

    fn load(&self, r: &Rendition, start_paused: bool, position_ms: u32) {
        let _ = self.tx.send(Cmd::Load {
            rendition: r.clone(),
            start_paused,
            position_ms,
        });
    }

    fn toggle(&self) {
        let _ = self.tx.send(Cmd::Toggle);
    }

    fn seek(&self, position_ms: u32) {
        let _ = self.tx.send(Cmd::Seek(position_ms));
    }

    fn set_volume(&self, v: f32) {
        let _ = self.tx.send(Cmd::SetVolume(clamp_volume(v)));
    }

    fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    fn status(&self) -> PlayerStatus {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).status()
    }

    fn levels(&self) -> [f32; 5] {
        let window = self.tap.snapshot();
        crate::spectrum::bands(&window.samples, window.sample_rate)
    }
}

/// Everything audio-related lives here, on one thread.
struct Audio {
    stream: Option<rodio::OutputStream>,
    sink: Option<rodio::Sink>,
    /// Kept alive so a downloaded/copied tempfile is not deleted mid-playback.
    _temp: Option<NamedTempFile>,
    /// Current `(source, uri)`; every event carries these.
    playing: Option<(SourceId, String)>,
    duration_ms: u32,
    /// Bumped per `Load`; a stale `Finished` from an earlier generation is
    /// suppressed. Single-threaded worker mostly makes races impossible, but
    /// the counter makes it explicit.
    generation: u64,
    finished_sent: bool,
}

fn worker(
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    inner: Arc<Mutex<Snapshot>>,
    tap: Arc<AudioTap>,
    bus: Bus,
    media: HashMap<SourceId, Arc<dyn MediaProvider>>,
    media_cache: Arc<MediaCache>,
) {
    let stream = match rodio::OutputStreamBuilder::open_default_stream() {
        Ok(s) => Some(s),
        Err(e) => {
            log::error!("rodio: no audio output device: {e}");
            None
        }
    };
    let mut audio = Audio {
        stream,
        sink: None,
        _temp: None,
        playing: None,
        duration_ms: 0,
        generation: 0,
        finished_sent: false,
    };

    loop {
        let msg = rx.recv_timeout(PROGRESS_INTERVAL);
        if matches!(msg, Err(crossbeam_channel::RecvTimeoutError::Disconnected)) {
            break;
        }
        // A panic while handling one `Cmd` must not kill the worker — the UI
        // thread would then block forever on the dead channel. Catch it, log,
        // keep looping (M2b bugfix). `AssertUnwindSafe`
        // because the closure captures `&mut audio`.
        let handle = std::panic::AssertUnwindSafe(|| match msg {
            Ok(Cmd::Load {
                rendition,
                start_paused,
                position_ms,
            }) => start_load(
                &mut audio,
                &inner,
                &tap,
                &bus,
                &media,
                &media_cache,
                &tx,
                rendition,
                start_paused,
                position_ms,
            ),
            Ok(Cmd::Loaded {
                generation,
                source,
                uri,
                start_paused,
                position_ms,
                result,
            }) => {
                if generation != audio.generation {
                    // Superseded by a later `Load` or a `Stop` while this
                    // was still resolving — drop it silently.
                    log::debug!(
                        "rodio: dropping stale load result (gen {generation}, now {})",
                        audio.generation
                    );
                } else {
                    let outcome = result.and_then(|loaded| {
                        finish_load(&mut audio, &inner, &bus, &source, &uri, loaded, start_paused, position_ms)
                    });
                    if let Err(e) = outcome {
                        log::error!("rodio: load failed: {e}");
                        audio.sink = None;
                        audio.playing = None;
                        set_state(&inner, PlayerState::Stopped, 0);
                        bus.send(CoreEvent::Player(PlayerEvent::LoadFailed { source, uri }));
                    }
                }
            }
            Ok(Cmd::Toggle) => toggle(&audio, &inner, &bus),
            Ok(Cmd::Seek(ms)) => {
                if let Some(sink) = &audio.sink {
                    let _ = sink.try_seek(Duration::from_millis(ms as u64));
                    inner.lock().unwrap_or_else(|e| e.into_inner()).position_ms = ms;
                }
            }
            Ok(Cmd::SetVolume(v)) => {
                inner.lock().unwrap_or_else(|e| e.into_inner()).volume = v;
                if let Some(sink) = &audio.sink {
                    sink.set_volume(v);
                }
            }
            Ok(Cmd::Stop) => {
                audio.generation += 1;
                if let Some(sink) = &audio.sink {
                    sink.stop();
                }
                audio.sink = None;
                audio.playing = None;
                set_state(&inner, PlayerState::Stopped, 0);
                bus.send(CoreEvent::Player(PlayerEvent::Stopped));
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => tick(&mut audio, &inner, &bus),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => unreachable!("handled above"),
        });
        if std::panic::catch_unwind(handle).is_err() {
            log::error!("rodio: worker command panicked; recovered, worker still running");
        }
    }
}

fn tick(audio: &mut Audio, inner: &Arc<Mutex<Snapshot>>, bus: &Bus) {
    let Some(sink) = &audio.sink else { return };
    let (source, uri) = match &audio.playing {
        Some(p) => p.clone(),
        None => return,
    };
    if sink.is_paused() {
        return;
    }
    let pos = sink.get_pos().as_millis() as u32;
    inner.lock().unwrap_or_else(|e| e.into_inner()).position_ms = pos;
    bus.send(CoreEvent::Player(PlayerEvent::Progress {
        position_ms: pos,
        duration_ms: audio.duration_ms,
    }));
    if sink.empty() && !audio.finished_sent {
        audio.finished_sent = true;
        set_state(inner, PlayerState::Stopped, 0);
        bus.send(CoreEvent::Player(PlayerEvent::Finished { source, uri }));
    }
}

/// Handles `Cmd::Load`: does the bookkeeping that has to happen right away
/// (bump `generation`, publish `Loading`), then hands the actual resolve +
/// decode-open off to a background thread and returns immediately — the
/// worker's `recv` loop is free to keep handling `Stop`/`Toggle`/a
/// superseding `Load` the whole time that thread is (possibly) stuck on a
/// slow or hanging source. The background thread's result comes back as
/// `Cmd::Loaded`, tagged with the `generation` it was asked to load.
#[allow(clippy::too_many_arguments)]
fn start_load(
    audio: &mut Audio,
    inner: &Arc<Mutex<Snapshot>>,
    tap: &Arc<AudioTap>,
    bus: &Bus,
    media: &HashMap<SourceId, Arc<dyn MediaProvider>>,
    media_cache: &Arc<MediaCache>,
    tx: &Sender<Cmd>,
    r: Rendition,
    start_paused: bool,
    position_ms: u32,
) {
    audio.generation += 1;
    let generation = audio.generation;
    audio.finished_sent = false;
    let (source, uri) = (r.source.clone(), r.uri.clone());
    log::debug!("player: load gen {generation} [{source}] {uri}");
    audio.playing = Some((source.clone(), uri.clone()));

    {
        let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
        s.source = Some(source.clone());
        s.uri = Some(uri.clone());
        s.position_ms = position_ms;
        s.duration_ms = 0;
        s.state = PlayerState::Playing;
    }
    bus.send(CoreEvent::Player(PlayerEvent::Loading {
        source: source.clone(),
        uri: uri.clone(),
    }));

    if audio.stream.is_none() {
        // Fail fast — no point spawning a thread to resolve/download a
        // track this process can never play.
        let _ = tx.send(Cmd::Loaded {
            generation,
            source,
            uri,
            start_paused,
            position_ms,
            result: Err(core::Error::Other("no audio output device".into())),
        });
        return;
    }

    let media = media.clone();
    let media_cache = media_cache.clone();
    let tap = tap.clone();
    let tx = tx.clone();
    let bus = bus.clone();
    std::thread::spawn(move || {
        let result = resolve_and_decode(&media, &media_cache, &r, &tap);
        if result.is_ok() {
            // Fetch (if any) is done — the source's own materialization,
            // announced the same way Spotify's worker announces its own.
            bus.send(CoreEvent::Player(PlayerEvent::Materialized {
                source: source.clone(),
                uri: uri.clone(),
            }));
        }
        let _ = tx.send(Cmd::Loaded {
            generation,
            source,
            uri,
            start_paused,
            position_ms,
            result,
        });
    });
}

/// Runs on the background thread `start_load` spawns: everything that might
/// block on the network or a slow disk (`MediaProvider::open`, the MVP full
/// download, opening the decoder) — nothing here touches worker-thread-only
/// state.
fn resolve_and_decode(
    media: &HashMap<SourceId, Arc<dyn MediaProvider>>,
    media_cache: &MediaCache,
    r: &Rendition,
    tap: &Arc<AudioTap>,
) -> core::Result<LoadedTrack> {
    let source = &r.source;
    let uri = &r.uri;

    // A local rendition whose source has no registered `MediaProvider` (e.g.
    // an imported `local` track) is loaded straight off disk — skip the
    // `media` lookup entirely.
    let (path, temp): (PathBuf, Option<NamedTempFile>) =
        if core::is_local_source(source) && !media.contains_key(source) {
            (PathBuf::from(core::local_path_from_uri(uri)), None)
        } else {
            open_media(media, media_cache, source, r, uri)?
        };

    let file = File::open(&path).map_err(|e| core::Error::Other(e.to_string()))?;
    let decoder =
        rodio::Decoder::try_from(file).map_err(|e| core::Error::Other(format!("decode: {e}")))?;
    let duration_ms = decoder
        .total_duration()
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0);
    let tapped = Tapped::new(decoder, tap.clone());

    Ok(LoadedTrack { tapped, duration_ms, temp })
}

/// Applies a successfully resolved `LoadedTrack` on the worker thread: the
/// only part of loading that must happen here, since `OutputStream`/`Sink`
/// are `!Send`.
#[allow(clippy::too_many_arguments)]
fn finish_load(
    audio: &mut Audio,
    inner: &Arc<Mutex<Snapshot>>,
    bus: &Bus,
    source: &SourceId,
    uri: &str,
    loaded: LoadedTrack,
    start_paused: bool,
    position_ms: u32,
) -> core::Result<()> {
    let stream = audio
        .stream
        .as_ref()
        .ok_or_else(|| core::Error::Other("no audio output device".into()))?;

    let sink = rodio::Sink::connect_new(stream.mixer());
    sink.set_volume(inner.lock().unwrap_or_else(|e| e.into_inner()).volume);
    sink.append(loaded.tapped);
    if position_ms > 0 {
        let _ = sink.try_seek(Duration::from_millis(position_ms as u64));
    }

    audio._temp = loaded.temp;
    audio.duration_ms = loaded.duration_ms;
    audio.sink = Some(sink);

    {
        let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
        s.duration_ms = loaded.duration_ms;
    }
    bus.send(CoreEvent::Player(PlayerEvent::Playing {
        source: source.clone(),
        uri: uri.to_string(),
    }));

    if start_paused {
        if let Some(sink) = &audio.sink {
            sink.pause();
        }
        set_state(inner, PlayerState::Paused, position_ms);
        bus.send(CoreEvent::Player(PlayerEvent::Paused));
    }
    Ok(())
}

/// Resolve a rendition to a decodable local file via its `MediaProvider`
/// (falling back to the http provider), downloading / spooling as needed.
fn open_media(
    media: &HashMap<SourceId, Arc<dyn MediaProvider>>,
    media_cache: &MediaCache,
    source: &SourceId,
    r: &Rendition,
    uri: &str,
) -> core::Result<(std::path::PathBuf, Option<NamedTempFile>)> {
    // Any source: a `MediaCache` hit plays straight from disk, skipping a
    // live fetch entirely — checked before asking the provider to do
    // anything, the same "cache first, else its own fetch mechanics" order
    // scanning uses.
    if let Some(p) = media_cache.cached_path(&r.source, &r.uri) {
        return Ok((p, None));
    }
    let provider = media
        .get(source)
        .or_else(|| media.get(&http_id()))
        .ok_or_else(|| core::Error::NoSource(uri.to_string()))?;

    Ok(match provider.open(r)? {
        // Already sitting on local disk (e.g. Soulseek's own downloads
        // dir) — nothing was fetched, so MediaCache only gets a symlink to
        // it, not a duplicate copy.
        Media::Path(p) => {
            if let Err(e) = media_cache.link_local(&r.source, &r.uri, &p) {
                log::debug!("player: couldn't link {source} {uri} into media cache: {e}");
            }
            (p, None)
        }
        Media::Url(url) => {
            let started = std::time::Instant::now();
            let mut tmp = NamedTempFile::new().map_err(|e| core::Error::Other(e.to_string()))?;
            core::fetch_url_to(&url, tmp.as_file_mut())
                .map_err(|e| core::Error::Other(format!("download {url}: {e}")))?;
            log::info!("player: fetched {url} in {:?}", started.elapsed());
            let p = tmp.path().to_path_buf();
            cache_fetched(media_cache, r, &p, source, uri);
            (p, Some(tmp))
        }
        Media::Reader(mut rdr) => {
            // MVP: copy the stream to a tempfile, then decode from disk.
            let mut tmp = NamedTempFile::new().map_err(|e| core::Error::Other(e.to_string()))?;
            std::io::copy(&mut rdr, tmp.as_file_mut())
                .map_err(|e| core::Error::Other(e.to_string()))?;
            let p = tmp.path().to_path_buf();
            cache_fetched(media_cache, r, &p, source, uri);
            (p, Some(tmp))
        }
    })
}

/// Best-effort: stash a copy of a just-fetched (non-local) rendition in the
/// shared MediaCache for CacheOnly scanning, mirroring Spotify's own
/// playback-triggered materializer.
fn cache_fetched(media_cache: &MediaCache, r: &Rendition, path: &std::path::Path, source: &SourceId, uri: &str) {
    if let Err(e) = media_cache.put_file(&r.source, &r.uri, path) {
        log::debug!("player: couldn't populate media cache for {source} {uri}: {e}");
    }
}

fn toggle(audio: &Audio, inner: &Arc<Mutex<Snapshot>>, bus: &Bus) {
    let Some(sink) = &audio.sink else { return };
    let Some((source, uri)) = audio.playing.clone() else {
        return;
    };
    if sink.is_paused() {
        sink.play();
        inner.lock().unwrap_or_else(|e| e.into_inner()).state = PlayerState::Playing;
        bus.send(CoreEvent::Player(PlayerEvent::Playing { source, uri }));
    } else {
        sink.pause();
        inner.lock().unwrap_or_else(|e| e.into_inner()).state = PlayerState::Paused;
        bus.send(CoreEvent::Player(PlayerEvent::Paused));
    }
}

fn set_state(inner: &Arc<Mutex<Snapshot>>, state: PlayerState, position_ms: u32) {
    let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
    s.state = state;
    s.position_ms = position_ms;
}
