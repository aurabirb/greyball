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
//! rendition (a `MediaProvider::open`, possibly a fetch over the network)
//! and opening the decoder happen on a throwaway background thread, which
//! reports back via `Cmd::Loaded`. This means the worker's `recv` loop is
//! never blocked on that work — a stalled/hanging source (a wedged server, a
//! `MediaProvider` that never returns) leaves the *load* stuck, not the
//! player: `Stop`, `Toggle`, a *different* `Load`, all still get processed
//! immediately. `generation` (bumped on every `Load`/`Stop`) tags each
//! background attempt so a `Loaded` that finally reports in after something
//! superseded it is just dropped, not applied.
//!
//! A `Media::Url` rendition (HTTP/SoundCloud) streams: `open_streaming_url`
//! starts the GET, and as soon as the response headers report a
//! `Content-Length` it hands back a `StreamingReader` immediately, letting
//! the decoder start (and playback begin) on the first bytes while the rest
//! downloads in the background — see `StreamingReader`'s doc. Without a
//! `Content-Length` there's no safe way to preallocate the file the reader
//! seeks within, so that case falls back to the old fully-blocking download.
//!
//! rodio 0.21 API notes:
//! - stream: `OutputStreamBuilder::open_default_stream()`, then `.mixer()`.
//! - sink: `Sink::connect_new(&mixer)` (no `OutputStreamHandle`).
//! - decode: `Decoder::builder().with_data(..).with_byte_len(..).build()`.
//! - `Sink::get_pos`, `try_seek`, `set_volume`, `empty`, `stop` unchanged.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Condvar, Mutex};
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
        cache: bool,
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
    /// `ack` is signaled once this player's sink has actually stopped
    /// producing audio — `Player::stop` blocks on it so a caller switching
    /// to a different player (`Session::start_playback`) can't start the
    /// new one before this one has gone silent, which is what closed the
    /// rare two-tracks-at-once race (see that call site's doc).
    Stop { ack: Sender<()> },
}

/// What the background thread spawned by `Cmd::Load` hands back: everything
/// needed to start playback that doesn't touch the (worker-thread-only,
/// `!Send`) `OutputStream`/`Sink`.
struct LoadedTrack {
    tapped: Tapped<rodio::Decoder<StreamingReader>>,
    duration_ms: u32,
    /// Kept alive so a downloaded/copied tempfile is not deleted mid-playback
    /// (and, for a streaming download, so the background fetch thread's own
    /// handle to the same path stays valid).
    temp: Option<NamedTempFile>,
}

/// Coordinates a background download with the `StreamingReader`(s) reading
/// the file it's filling in — `written` only ever grows, `cond` wakes a
/// blocked reader every time it does (or once `done`/`error` is set).
/// `Default` is the "nothing written yet, still in progress" starting state
/// a fresh download begins in.
#[derive(Default)]
struct StreamState {
    progress: Mutex<StreamProgress>,
    cond: Condvar,
}

#[derive(Default)]
struct StreamProgress {
    written: u64,
    done: bool,
    error: Option<String>,
}

impl StreamState {
    fn ready(total: u64) -> Self {
        Self {
            progress: Mutex::new(StreamProgress { written: total, done: true, error: None }),
            cond: Condvar::new(),
        }
    }

    fn advance(&self, n: u64) {
        let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        p.written += n;
        self.cond.notify_all();
    }

    fn finish(&self, error: Option<String>) {
        let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        p.done = true;
        p.error = error;
        self.cond.notify_all();
    }
}

/// `Read + Seek` over a file a background download is still filling in —
/// blocks a `read()` past what's been written so far instead of a premature
/// EOF, so `rodio::Decoder` (and thus playback) can start on the first bytes
/// rather than waiting for the whole fetch. `total_len` is known upfront
/// (the `Content-Length` the download was sized against, or — for an
/// already-complete/local source via `StreamingReader::ready` — the file's
/// actual length), so `Seek` never has to block: the backing file is
/// preallocated (`set_len`) to its final size before any reader sees it.
struct StreamingReader {
    file: File,
    pos: u64,
    total_len: u64,
    state: Arc<StreamState>,
    patch: Option<(u64, [u8; 4])>,
}

impl StreamingReader {
    /// Wraps an already-fully-available file (a `MediaCache` hit, a local
    /// path, or anything else that doesn't need progressive-download
    /// bookkeeping) in the same type `LoadedTrack` expects.
    fn ready(file: File) -> std::io::Result<Self> {
        let total_len = file.metadata()?.len();
        Ok(Self { file, pos: 0, total_len, state: Arc::new(StreamState::ready(total_len)), patch: None })
    }
}

impl Read for StreamingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let (written, done, error) = {
                let p = self.state.progress.lock().unwrap_or_else(|e| e.into_inner());
                (p.written, p.done, p.error.clone())
            };
            if self.pos < written {
                let avail = (written - self.pos).min(buf.len() as u64) as usize;
                self.file.seek(SeekFrom::Start(self.pos))?;
                let n = self.file.read(&mut buf[..avail])?;
                if let Some((off, bytes)) = self.patch {
                    for (i, b) in bytes.iter().enumerate() {
                        if let Some(slot) = (off + i as u64).checked_sub(self.pos).and_then(|k| buf[..n].get_mut(k as usize)) {
                            *slot = *b;
                        }
                    }
                }
                self.pos += n as u64;
                return Ok(n);
            }
            if done {
                return match error {
                    Some(e) => Err(std::io::Error::other(e)),
                    None => Ok(0),
                };
            }
            // More is coming — wait for `advance`/`finish` to notify rather
            // than busy-polling. The timeout is just a safety net (a missed
            // notify must not hang the decoder forever); the normal wakeup
            // is the `notify_all` in `advance`/`finish`.
            let guard = self.state.progress.lock().unwrap_or_else(|e| e.into_inner());
            let _ = self.state.cond.wait_timeout(guard, Duration::from_millis(200));
        }
    }
}

impl Seek for StreamingReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.total_len as i64 + d,
        };
        self.pos = new_pos.max(0) as u64;
        Ok(self.pos)
    }
}

pub struct RodioPlayer {
    inner: Arc<Mutex<Snapshot>>,
    tap: Arc<AudioTap>,
    tx: Sender<Cmd>,
}

impl RodioPlayer {
    pub fn new(
        media: core::SharedMedia,
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

    fn load(&self, r: &Rendition, start_paused: bool, position_ms: u32, cache: bool) {
        let _ = self.tx.send(Cmd::Load {
            rendition: r.clone(),
            start_paused,
            position_ms,
            cache,
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

    /// Blocks until the worker has actually silenced its sink — see
    /// `Cmd::Stop`'s doc.
    fn stop(&self) {
        let (ack, rx) = crossbeam_channel::bounded(1);
        if self.tx.send(Cmd::Stop { ack }).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
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
    media: core::SharedMedia,
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
                cache,
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
                cache,
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
                    match sink.try_seek(Duration::from_millis(ms as u64)) {
                        Ok(()) => inner.lock().unwrap_or_else(|e| e.into_inner()).position_ms = ms,
                        Err(e) => log::debug!("rodio: seek to {ms} ms failed: {e}"),
                    }
                }
            }
            Ok(Cmd::SetVolume(v)) => {
                inner.lock().unwrap_or_else(|e| e.into_inner()).volume = v;
                if let Some(sink) = &audio.sink {
                    sink.set_volume(v);
                }
            }
            Ok(Cmd::Stop { ack }) => {
                audio.generation += 1;
                if let Some(sink) = &audio.sink {
                    sink.stop();
                }
                audio.sink = None;
                audio.playing = None;
                set_state(&inner, PlayerState::Stopped, 0);
                bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                let _ = ack.send(());
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
    media: &core::SharedMedia,
    media_cache: &Arc<MediaCache>,
    tx: &Sender<Cmd>,
    r: Rendition,
    start_paused: bool,
    position_ms: u32,
    cache: bool,
) {
    audio.generation += 1;
    let generation = audio.generation;
    // The previous (drained) sink stays in place until `finish_load`; its emptiness says nothing about this load.
    audio.finished_sent = true;
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

    let media = media.snapshot();
    let media_cache = media_cache.clone();
    let tap = tap.clone();
    let tx = tx.clone();
    let bus = bus.clone();
    std::thread::spawn(move || {
        let result = resolve_and_decode(&media, &media_cache, &r, &tap, cache, &bus);
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
/// block on the network or a slow disk (`MediaProvider::open`, resolving the
/// rendition, opening the decoder) — nothing here touches worker-thread-only
/// state.
fn resolve_and_decode(
    media: &HashMap<SourceId, Arc<dyn MediaProvider>>,
    media_cache: &Arc<MediaCache>,
    r: &Rendition,
    tap: &Arc<AudioTap>,
    cache: bool,
    bus: &Bus,
) -> core::Result<LoadedTrack> {
    let source = &r.source;
    let uri = &r.uri;

    // A local rendition whose source has no registered `MediaProvider` (e.g.
    // an imported `local` track) is loaded straight off disk — skip the
    // `media` lookup entirely.
    let (reader, temp): (StreamingReader, Option<NamedTempFile>) =
        if core::is_local_source(source) && !media.contains_key(source) {
            let path = core::local_path_from_uri(uri);
            let file = File::open(path).map_err(|e| core::Error::Other(e.to_string()))?;
            let reader = StreamingReader::ready(file).map_err(|e| core::Error::Other(e.to_string()))?;
            bus.send(CoreEvent::Player(PlayerEvent::Materialized {
                source: source.clone(),
                uri: uri.clone(),
            }));
            (reader, None)
        } else {
            open_media(media, media_cache, source, r, uri, cache, bus)?
        };

    let mut reader = reader;
    let total_len = reader.total_len;
    reader.patch = crate::fmp4::duration_patch(&mut reader, total_len);
    reader.pos = 0;
    let decoder = rodio::Decoder::builder()
        .with_data(reader)
        .with_byte_len(total_len)
        .build()
        .map_err(|e| core::Error::Other(format!("decode: {e}")))?;
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
    log::debug!("player: playback started [{source}] {uri} ({} ms)", loaded.duration_ms);
    audio.sink = Some(sink);
    audio.finished_sent = false;

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

/// Resolve a rendition to a `StreamingReader` via its `MediaProvider`
/// (falling back to the http provider), fetching as needed. `bus` gets a
/// `PlayerEvent::Materialized` the moment this rendition's bytes are fully
/// available in `MediaCache` — immediately for every branch except a
/// streaming `Media::Url` download, where it's deferred to the background
/// thread that finishes it (see `open_streaming_url`).
fn open_media(
    media: &HashMap<SourceId, Arc<dyn MediaProvider>>,
    media_cache: &Arc<MediaCache>,
    source: &SourceId,
    r: &Rendition,
    uri: &str,
    cache: bool,
    bus: &Bus,
) -> core::Result<(StreamingReader, Option<NamedTempFile>)> {
    let materialized = || {
        bus.send(CoreEvent::Player(PlayerEvent::Materialized {
            source: source.clone(),
            uri: uri.to_string(),
        }))
    };

    // Any source: a `MediaCache` hit plays straight from disk, skipping a
    // live fetch entirely — checked before asking the provider to do
    // anything, the same "cache first, else its own fetch mechanics" order
    // scanning uses.
    if let Some(p) = media_cache.cached_path(&r.source, &r.uri) {
        let file = File::open(&p).map_err(|e| core::Error::Other(e.to_string()))?;
        materialized();
        return Ok((StreamingReader::ready(file).map_err(|e| core::Error::Other(e.to_string()))?, None));
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
            if cache && let Err(e) = media_cache.link_local(&r.source, &r.uri, &p) {
                log::debug!("player: couldn't link {source} {uri} into media cache: {e}");
            }
            let file = File::open(&p).map_err(|e| core::Error::Other(e.to_string()))?;
            materialized();
            (StreamingReader::ready(file).map_err(|e| core::Error::Other(e.to_string()))?, None)
        }
        Media::Url(url) => {
            let (reader, tmp) = open_streaming_url(&url, media_cache, r, source, uri, cache, bus)?;
            (reader, Some(tmp))
        }
        Media::Reader(mut rdr) => {
            // MVP: copy the stream to a tempfile, then decode from disk — no
            // `Content-Length` to preallocate against for a generic `Read`,
            // so this still waits for the whole thing rather than streaming.
            let mut tmp = NamedTempFile::new().map_err(|e| core::Error::Other(e.to_string()))?;
            std::io::copy(&mut rdr, tmp.as_file_mut())
                .map_err(|e| core::Error::Other(e.to_string()))?;
            let p = tmp.path().to_path_buf();
            if cache {
                cache_fetched(media_cache, r, &p, source, uri);
            }
            let file = File::open(&p).map_err(|e| core::Error::Other(e.to_string()))?;
            materialized();
            (StreamingReader::ready(file).map_err(|e| core::Error::Other(e.to_string()))?, Some(tmp))
        }
    })
}

/// Streams `url`'s body into a preallocated tempfile in the background while
/// handing back a `StreamingReader` immediately — the decoder can start
/// consuming the first bytes as they arrive instead of blocking on the whole
/// fetch, which is what made playback of a big file slow to start. Requires
/// a `Content-Length` to preallocate (`set_len`) the file the reader seeks
/// within; without one this falls back to the old fully-blocking download
/// (still correct, just not faster).
fn open_streaming_url(
    url: &str,
    media_cache: &Arc<MediaCache>,
    r: &Rendition,
    source: &SourceId,
    uri: &str,
    cache: bool,
    bus: &Bus,
) -> core::Result<(StreamingReader, NamedTempFile)> {
    let fetch = core::start_get(url).map_err(|e| core::Error::Other(format!("download {url}: {e}")))?;

    let Some(len) = fetch.content_length else {
        let started = std::time::Instant::now();
        let mut tmp = NamedTempFile::new().map_err(|e| core::Error::Other(e.to_string()))?;
        let mut body = fetch.body;
        std::io::copy(&mut body, tmp.as_file_mut()).map_err(|e| core::Error::Other(format!("download {url}: {e}")))?;
        log::info!("player: fetched {url} in {:?} (no content-length, full download)", started.elapsed());
        let p = tmp.path().to_path_buf();
        if cache {
            cache_fetched(media_cache, r, &p, source, uri);
        }
        bus.send(CoreEvent::Player(PlayerEvent::Materialized { source: source.clone(), uri: uri.to_string() }));
        let file = File::open(&p).map_err(|e| core::Error::Other(e.to_string()))?;
        return Ok((StreamingReader::ready(file).map_err(|e| core::Error::Other(e.to_string()))?, tmp));
    };

    let tmp = NamedTempFile::new().map_err(|e| core::Error::Other(e.to_string()))?;
    tmp.as_file().set_len(len).map_err(|e| core::Error::Other(e.to_string()))?;
    let read_file = tmp.reopen().map_err(|e| core::Error::Other(e.to_string()))?;
    let write_file = tmp.reopen().map_err(|e| core::Error::Other(e.to_string()))?;
    let state = Arc::new(StreamState::default());

    let path = tmp.path().to_path_buf();
    let media_cache = media_cache.clone();
    let r = r.clone();
    let source_owned = source.clone();
    let uri_owned = uri.to_string();
    let url_owned = url.to_string();
    let bus = bus.clone();
    let cache_state = state.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let result = stream_body_to_file(fetch.body, write_file, &cache_state, len);
        match &result {
            Ok(()) => {
                log::info!("player: streamed {url_owned} in {:?}", started.elapsed());
                if cache {
                    cache_fetched(&media_cache, &r, &path, &source_owned, &uri_owned);
                }
                bus.send(CoreEvent::Player(PlayerEvent::Materialized {
                    source: source_owned,
                    uri: uri_owned,
                }));
            }
            Err(e) => bus.send(CoreEvent::BackgroundFailure {
                context: source_owned.to_string(),
                message: format!("streaming download of {url_owned} failed: {e}"),
            }),
        }
        cache_state.finish(result.err());
    });

    Ok((StreamingReader { file: read_file, pos: 0, total_len: len, state, patch: None }, tmp))
}

/// Runs on `open_streaming_url`'s background thread: copies the response
/// body into `file` chunk by chunk, advancing `state` after each write so a
/// blocked `StreamingReader::read` wakes as soon as there's more to give it.
///
/// `expected_len` is the `Content-Length` the file was preallocated to
/// (`set_len`). A body that ends (a clean, error-free EOF) before reaching
/// it is a truncated transfer, not a success — treating a short read as
/// "done" used to leave the preallocated file's tail as zero bytes, which
/// then got copied into `MediaCache` as if it were real audio: a later
/// cache-hit playback would decode real audio up to the truncation point,
/// then feed the decoder a wall of zeros it can't parse ("invalid frame"
/// errors) for the rest of the track — audible as a long trailing silence
/// on a long track. Truncating the file to what was actually received and
/// erroring instead keeps a short transfer from ever being cached or
/// reported as materialized, so a later attempt re-fetches it properly.
fn stream_body_to_file(
    mut body: Box<dyn Read + Send>,
    mut file: File,
    state: &StreamState,
    expected_len: u64,
) -> Result<(), String> {
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            if total < expected_len {
                file.set_len(total).map_err(|e| e.to_string())?;
                return Err(format!("truncated: got {total} of {expected_len} bytes"));
            }
            return Ok(());
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        total += n as u64;
        state.advance(n as u64);
    }
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
