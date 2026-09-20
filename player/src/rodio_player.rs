//! `RodioPlayer` — real audio playback.
//!
//! `rodio`'s `OutputStream` wraps a `cpal::Stream`, which is `!Send`, so it
//! cannot live behind the `Arc<Mutex<..>>` a `Send + Sync` `Player` needs.
//! Instead a single long-lived worker thread owns *all* audio state
//! (`OutputStream`, `Sink`, the current stream) and the public methods just
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
//! Audio comes from `core::StreamEngine`: `Intent::Play` returns a `StreamHandle` at once, the load
//! thread waits for the first `START_BYTES`, and the decoder reads through a `StreamReader`. A stream
//! that knows its length and can jump (or is finished) gets a seekable decoder; any other gets a
//! non-seekable one and ignores seeks until it is `Done`.
//!
//! rodio 0.21 API notes:
//! - stream: `OutputStreamBuilder::open_default_stream()`, then `.mixer()`.
//! - sink: `Sink::connect_new(&mixer)` (no `OutputStreamHandle`).
//! - decode: `Decoder::builder().with_data(..).with_byte_len(..).build()`.
//! - `Sink::get_pos`, `try_seek`, `set_volume`, `empty`, `stop` unchanged.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{
    Bus, Claim, CoreEvent, Intent, Player, PlayerEvent, PlayerState, PlayerStatus, Rendition,
    SourceId, StreamEngine, StreamHandle, StreamReader, StreamState,
};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rodio::Source;

use crate::{AudioTap, Snapshot, Tapped, clamp_volume};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Bytes on disk before a decoder is built, and before a starved sink resumes.
const START_BYTES: u64 = 256 * 1024;
/// Playback pauses to buffer when the contiguous bytes past the read position fall below this.
const LOW_WATER: u64 = 64 * 1024;
/// How long a load waits for the first bytes once connected; `Connecting` is unbounded here (providers own their limits, a skip supersedes the load).
const START_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// `ack` is signaled once this player's sink has actually stopped
    /// producing audio — `Player::stop` blocks on it so a caller switching
    /// to a different player (`Session::start_playback`) can't start the
    /// new one before this one has gone silent, which is what closed the
    /// rare two-tracks-at-once race (see that call site's doc).
    Stop { ack: Sender<()> },
}

/// A stream's bytes with the fMP4 duration patch applied on the way through.
struct PatchedReader {
    inner: StreamReader,
    pos: u64,
    patch: Option<(u64, [u8; 4])>,
}

impl Read for PatchedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some((off, bytes)) = self.patch {
            for (i, b) in bytes.iter().enumerate() {
                if let Some(slot) = (off + i as u64).checked_sub(self.pos).and_then(|k| buf[..n].get_mut(k as usize)) {
                    *slot = *b;
                }
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for PatchedReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

type Decoded = Tapped<rodio::Decoder<PatchedReader>>;

/// A decoder over `handle`'s bytes, seekable only if the stream can honor seeks now.
fn build_source(handle: &StreamHandle, tap: &Arc<AudioTap>) -> core::Result<(Decoded, bool, u32)> {
    let info = handle.info();
    let done = info.state == StreamState::Done;
    let byte_len = info.len.filter(|_| done || info.jumpable);
    let mut reader = handle.reader();
    let patch = match info.len {
        Some(len) if done => {
            let patch = crate::fmp4::duration_patch(&mut reader, len);
            let _ = reader.seek(SeekFrom::Start(0));
            patch
        }
        _ => None,
    };
    let builder = rodio::Decoder::builder().with_data(PatchedReader { inner: reader, pos: 0, patch });
    let builder = if let Some(len) = byte_len { builder.with_byte_len(len) } else { builder };
    let decoder = builder.build().map_err(|e| core::Error::Other(format!("decode: {e}")))?;
    let duration_ms = decoder.total_duration().map(|d| d.as_millis() as u32).unwrap_or(0);
    Ok((Tapped::new(decoder, tap.clone()), byte_len.is_some(), duration_ms))
}

/// What the background thread spawned by `Cmd::Load` hands back: everything
/// needed to start playback that doesn't touch the (worker-thread-only,
/// `!Send`) `OutputStream`/`Sink`.
struct LoadedTrack {
    tapped: Decoded,
    duration_ms: u32,
    live: Live,
}

/// The stream behind the current sink; dropping it releases the claim.
struct Live {
    handle: StreamHandle,
    _claim: Claim,
    seekable: bool,
}

pub struct RodioPlayer {
    inner: Arc<Mutex<Snapshot>>,
    tap: Arc<AudioTap>,
    tx: Sender<Cmd>,
}

impl RodioPlayer {
    pub fn new(engine: StreamEngine, bus: Bus) -> Self {
        let inner = Arc::new(Mutex::new(Snapshot::default()));
        let tap = Arc::new(AudioTap::default());
        let (tx, rx) = unbounded();
        let worker_tx = tx.clone();
        let worker_inner = inner.clone();
        let worker_tap = tap.clone();
        std::thread::Builder::new()
            .name("rodio-player".into())
            .spawn(move || worker(rx, worker_tx, worker_inner, worker_tap, bus, engine))
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
    live: Option<Live>,
    /// Playback is paused because the download fell behind (not by the user).
    buffering: bool,
    /// The track the sink is playing; every event carries these.
    playing: Option<(SourceId, String)>,
    duration_ms: u32,
    /// Bumped per `Load`; a stale `Finished` from an earlier generation is
    /// suppressed. Single-threaded worker mostly makes races impossible, but
    /// the counter makes it explicit.
    generation: Arc<AtomicU64>,
    finished_sent: bool,
    /// The newest load in flight (sink and snapshot stay on the previous track until the swap) and
    /// whether the user paused meanwhile, which the new track then starts in.
    pending: Option<(SourceId, String, bool)>,
}

fn worker(
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    inner: Arc<Mutex<Snapshot>>,
    tap: Arc<AudioTap>,
    bus: Bus,
    engine: StreamEngine,
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
        live: None,
        buffering: false,
        playing: None,
        duration_ms: 0,
        generation: Arc::new(AtomicU64::new(0)),
        finished_sent: false,
        pending: None,
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
            }) => start_load(&mut audio, &inner, &tap, &bus, &engine, &tx, rendition, start_paused, position_ms),
            Ok(Cmd::Loaded {
                generation,
                source,
                uri,
                start_paused,
                position_ms,
                result,
            }) => {
                if generation != audio.generation.load(Ordering::SeqCst) {
                    // Superseded by a later `Load` or a `Stop` while this
                    // was still resolving — drop it silently.
                    log::debug!(
                        "rodio: dropping stale load result (gen {generation}, now {})",
                        audio.generation.load(Ordering::SeqCst)
                    );
                } else {
                    let paused = start_paused || audio.pending.as_ref().is_some_and(|p| p.2);
                    let outcome = result
                        .and_then(|loaded| finish_load(&mut audio, &inner, &bus, &source, &uri, loaded, paused, position_ms));
                    if let Err(e) = outcome {
                        // The previous track plays on (the app decides what replaces it, and a pause made meanwhile stays
                        // for it); with none, the load's own status goes.
                        log::error!("rodio: load failed: {e}");
                        if let Some(live) = &audio.live {
                            log::debug!("player: the previous track plays on after the failed load");
                            inner.lock().unwrap_or_else(|e| e.into_inner()).stream = Some(live.handle.clone());
                        } else {
                            audio.pending = None;
                            release(&mut audio, &inner);
                        }
                        bus.send(CoreEvent::Player(PlayerEvent::LoadFailed { source, uri }));
                    }
                }
            }
            Ok(Cmd::Toggle) => toggle(&mut audio, &inner, &bus),
            Ok(Cmd::Seek(ms)) => seek(&mut audio, &inner, &tap, ms),
            Ok(Cmd::SetVolume(v)) => {
                inner.lock().unwrap_or_else(|e| e.into_inner()).volume = v;
                if let Some(sink) = &audio.sink {
                    sink.set_volume(v);
                }
            }
            Ok(Cmd::Stop { ack }) => {
                audio.generation.fetch_add(1, Ordering::SeqCst);
                audio.pending = None;
                release(&mut audio, &inner);
                audio.playing = None;
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
    if sink.is_paused() && !audio.buffering {
        return;
    }
    let pos = sink.get_pos().as_millis() as u32;
    inner.lock().unwrap_or_else(|e| e.into_inner()).position_ms = pos;
    bus.send(CoreEvent::Player(PlayerEvent::Progress {
        source: source.clone(),
        uri: uri.clone(),
        position_ms: pos,
        duration_ms: audio.duration_ms,
    }));
    let info = audio.live.as_ref().map(|l| l.handle.info());
    if let Some(info) = &info {
        let active = matches!(info.state, StreamState::Connecting | StreamState::Fetching | StreamState::Buffering);
        if audio.buffering && (!active || info.ahead >= START_BYTES) {
            log::debug!("rodio: buffered enough, resuming");
            audio.buffering = false;
            inner.lock().unwrap_or_else(|e| e.into_inner()).buffering = false;
            sink.play();
        } else if !audio.buffering && active && info.ahead < LOW_WATER {
            log::debug!("rodio: {} bytes ahead of the read position, pausing to buffer", info.ahead);
            audio.buffering = true;
            inner.lock().unwrap_or_else(|e| e.into_inner()).buffering = true;
            sink.pause();
        }
    }
    if sink.empty() && !audio.finished_sent && !audio.buffering {
        audio.finished_sent = true;
        match info.map(|i| i.state) {
            Some(StreamState::Failed(why)) => {
                log::error!("rodio: stream failed during playback: {why}");
                bus.send(CoreEvent::Player(PlayerEvent::LoadFailed { source, uri }));
            }
            Some(StreamState::Cancelled) => {
                log::error!("rodio: stream was cancelled during playback");
                bus.send(CoreEvent::Player(PlayerEvent::LoadFailed { source, uri }));
            }
            _ => bus.send(CoreEvent::Player(PlayerEvent::Finished { source, uri })),
        }
        release(audio, inner);
    }
}

/// Drops the sink and the stream behind it (its download stops after the grace period); the snapshot reads stopped.
fn release(audio: &mut Audio, inner: &Arc<Mutex<Snapshot>>) {
    if audio.sink.is_some() {
        log::debug!("player: sink and stream released");
    }
    audio.sink = None;
    audio.live = None;
    audio.buffering = false;
    let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
    // A load in flight keeps its stream shown.
    let stream = s.stream.take().filter(|_| audio.pending.is_some());
    *s = Snapshot { volume: s.volume, stream, ..Snapshot::default() };
}

/// Handles `Cmd::Load`: does the bookkeeping that has to happen right away
/// (bump `generation`, publish `Loading`), then hands the actual open +
/// decode-open off to a background thread and returns immediately — the
/// worker's `recv` loop is free to keep handling `Stop`/`Toggle`/a
/// superseding `Load` the whole time that thread waits on a slow source.
/// The background thread's result comes back as `Cmd::Loaded`, tagged with
/// the `generation` it was asked to load.
#[allow(clippy::too_many_arguments)]
fn start_load(
    audio: &mut Audio,
    inner: &Arc<Mutex<Snapshot>>,
    tap: &Arc<AudioTap>,
    bus: &Bus,
    engine: &StreamEngine,
    tx: &Sender<Cmd>,
    r: Rendition,
    start_paused: bool,
    position_ms: u32,
) {
    let generation = audio.generation.fetch_add(1, Ordering::SeqCst) + 1;
    // The previous sink and stream keep playing, and the snapshot keeps describing them, until
    // `finish_load` swaps them out (gapless skip); with nothing playing the load itself is the status.
    let (source, uri) = (r.source.clone(), r.uri.clone());
    let paused_meanwhile = audio.pending.as_ref().is_some_and(|p| p.2);
    audio.pending = Some((source.clone(), uri.clone(), paused_meanwhile));
    if let Some(sink) = &audio.sink {
        log::debug!("player: previous track keeps playing at {} ms while gen {generation} loads", sink.get_pos().as_millis());
    } else {
        set_state(inner, PlayerState::Playing, position_ms);
    }
    log::debug!("player: load gen {generation} [{source}] {uri}");
    bus.send(CoreEvent::Player(PlayerEvent::Loading {
        source: source.clone(),
        uri: uri.clone(),
    }));

    if audio.stream.is_none() {
        // Fail fast — no point spawning a thread to open a track this process can never play.
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

    let engine = engine.clone();
    let tap = tap.clone();
    let tx = tx.clone();
    let inner = inner.clone();
    let live_generation = audio.generation.clone();
    std::thread::spawn(move || {
        let current = || live_generation.load(Ordering::SeqCst) == generation;
        // A panic must still report back, or the load would stay pending forever.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_and_decode(&engine, &r, &tap, &inner, &current)))
            .unwrap_or_else(|_| Err(core::Error::Other("load thread panicked".into())));
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

/// Runs on the background thread `start_load` spawns: opens the stream, waits for its first bytes and
/// builds the decoder — nothing here touches worker-thread-only state.
fn open_and_decode(
    engine: &StreamEngine,
    r: &Rendition,
    tap: &Arc<AudioTap>,
    inner: &Arc<Mutex<Snapshot>>,
    current: &dyn Fn() -> bool,
) -> core::Result<LoadedTrack> {
    let (handle, claim) = engine.open(r, Intent::Play)?;
    {
        // The newest load's stream is the one shown while it loads.
        let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
        if current() {
            s.stream = Some(handle.clone());
        }
    }
    let mut deadline = Instant::now() + START_TIMEOUT;
    while !handle.wait_range(0..START_BYTES, Duration::from_millis(200)) {
        if !current() {
            return Err(core::Error::Other("load superseded".into()));
        }
        match handle.info().state {
            StreamState::Failed(why) => return Err(core::Error::Other(format!("stream failed: {why}"))),
            StreamState::Cancelled => return Err(core::Error::Other("stream cancelled".into())),
            StreamState::Connecting | StreamState::Buffering => deadline = Instant::now() + START_TIMEOUT,
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(core::Error::Other("timed out waiting for the first bytes".into()));
        }
    }
    let (tapped, seekable, decoded_ms) = build_source(&handle, tap)?;
    let duration_ms = if decoded_ms > 0 { decoded_ms } else { r.duration_ms };
    Ok(LoadedTrack { tapped, duration_ms, live: Live { handle, _claim: claim, seekable } })
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
    sink.pause();
    sink.append(loaded.tapped);
    if position_ms > 0 && loaded.live.seekable {
        let _ = sink.try_seek(Duration::from_millis(position_ms as u64));
    }

    {
        let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
        s.stream = Some(loaded.live.handle.clone());
        s.duration_ms = loaded.duration_ms;
        s.position_ms = position_ms;
        s.buffering = false;
        s.state = if start_paused { PlayerState::Paused } else { PlayerState::Playing };
    }
    audio.buffering = false;
    audio.live = Some(loaded.live);
    audio.duration_ms = loaded.duration_ms;
    audio.playing = Some((source.clone(), uri.to_string()));
    audio.finished_sent = false;
    audio.pending = None;
    log::debug!("player: playback started [{source}] {uri} ({} ms)", loaded.duration_ms);
    // Dropping the previous track's sink stops it.
    let sink = audio.sink.insert(sink);
    bus.send(CoreEvent::Player(PlayerEvent::Playing {
        source: source.clone(),
        uri: uri.to_string(),
    }));

    if start_paused {
        bus.send(CoreEvent::Player(PlayerEvent::Paused));
    } else {
        sink.play();
    }
    Ok(())
}

/// `Cmd::Seek`: a stream that cannot honor seeks yet ignores them (a failed backward seek on a
/// non-seekable decoder would end the track); once it is `Done` the decoder is rebuilt seekable.
fn seek(audio: &mut Audio, inner: &Arc<Mutex<Snapshot>>, tap: &Arc<AudioTap>, ms: u32) {
    let (Some(live), Some(old)) = (&mut audio.live, &audio.sink) else { return };
    if !live.seekable {
        if live.handle.info().state != StreamState::Done {
            log::debug!("rodio: seek to {ms} ms ignored: the stream is not seekable until it is downloaded");
            return;
        }
        let Some(stream) = &audio.stream else { return };
        let rebuilt = match build_source(&live.handle, tap) {
            Ok((tapped, true, _)) => tapped,
            Ok(_) => return log::debug!("rodio: seek to {ms} ms ignored: rebuilt decoder is not seekable"),
            Err(e) => return log::debug!("rodio: seek to {ms} ms failed: {e}"),
        };
        let sink = rodio::Sink::connect_new(stream.mixer());
        sink.set_volume(inner.lock().unwrap_or_else(|e| e.into_inner()).volume);
        let paused = old.is_paused();
        sink.pause();
        sink.append(rebuilt);
        if let Err(e) = sink.try_seek(Duration::from_millis(ms as u64)) {
            log::debug!("rodio: seek to {ms} ms failed: {e}");
        }
        if !paused {
            sink.play();
        }
        old.stop();
        live.seekable = true;
        audio.sink = Some(sink);
        inner.lock().unwrap_or_else(|e| e.into_inner()).position_ms = ms;
        return;
    }
    match old.try_seek(Duration::from_millis(ms as u64)) {
        Ok(()) => inner.lock().unwrap_or_else(|e| e.into_inner()).position_ms = ms,
        Err(e) => log::debug!("rodio: seek to {ms} ms failed: {e}"),
    }
}

fn toggle(audio: &mut Audio, inner: &Arc<Mutex<Snapshot>>, bus: &Bus) {
    let Some(sink) = &audio.sink else {
        // First load: no sink yet, the pause is recorded for the swap.
        if let Some((source, uri, paused)) = &mut audio.pending {
            *paused = !*paused;
            if *paused {
                set_state(inner, PlayerState::Paused, 0);
                bus.send(CoreEvent::Player(PlayerEvent::Paused));
            } else {
                set_state(inner, PlayerState::Playing, 0);
                bus.send(CoreEvent::Player(PlayerEvent::Playing { source: source.clone(), uri: uri.clone() }));
            }
        }
        return;
    };
    let Some((source, uri)) = audio.playing.clone() else {
        return;
    };
    let resume = sink.is_paused() && !audio.buffering;
    if let Some(p) = &mut audio.pending {
        p.2 = !resume;
    }
    if resume {
        sink.play();
        inner.lock().unwrap_or_else(|e| e.into_inner()).state = PlayerState::Playing;
        bus.send(CoreEvent::Player(PlayerEvent::Playing { source, uri }));
    } else {
        sink.pause();
        audio.buffering = false;
        let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
        s.buffering = false;
        s.state = PlayerState::Paused;
        bus.send(CoreEvent::Player(PlayerEvent::Paused));
    }
}

fn set_state(inner: &Arc<Mutex<Snapshot>>, state: PlayerState, position_ms: u32) {
    let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
    s.state = state;
    s.position_ms = position_ms;
}
