//! `SpotifyPlayer` — a `core::Player` backed by `librespot-playback`.
//!
//! Mirrors `player::RodioPlayer`'s shape: one long-lived worker owns all the
//! playback state, the public struct is just a command `Sender` + a shared
//! status `Mutex`. The worker here runs a Tokio runtime (librespot is async)
//! and bridges `librespot_playback::player::PlayerEvent` onto the medley
//! `core::Bus` as `core::PlayerEvent`s carrying `(source, uri)`.
//!
//! `Player::levels` (the `:vis` bars EQ) needs real decoded audio, which
//! `RodioPlayer` gets by wrapping its `rodio::Source` in `player::Tapped`.
//! librespot never hands us a `Source` — instead `TappedSink` wraps the
//! `librespot_playback::audio_backend::Sink` every backend already writes
//! decoded PCM to, forwarding every packet unchanged while also publishing a
//! downmixed-to-mono window into the same `player::AudioTap` `RodioPlayer`
//! uses, so `spectrum::bands` doesn't need a second implementation.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{
    Bus, CoreEvent, Error, MediaCache, Player, PlayerEvent, PlayerState, PlayerStatus, ReadSeek,
    Rendition, SourceId,
};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::SpotifyUri;
use librespot_metadata::audio::AudioItem;
use librespot_playback::audio_backend;
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::MixerConfig;
use librespot_playback::player::{Player as LsPlayer, PlayerEvent as LsEvent};
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use player::AudioTap;
use tokio::sync::{mpsc, oneshot};

use crate::auth::{Auth, MUSIC_CLIENT_ID};

const TICK: Duration = Duration::from_millis(500);

/// A session that stays up at least this long counts as a successful
/// reconnect (not part of a crash loop), resetting `session_died_streak`.
const SESSION_HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// Wraps the real backend `Sink`, forwarding every packet unchanged while
/// also folding it into `tap` — the same downmix-then-publish-a-window
/// shape as `player::Tapped`, just working off the raw interleaved `f64`
/// PCM librespot hands every backend instead of a `rodio::Source`'s
/// per-sample iterator. librespot fixes its output at
/// `NUM_CHANNELS`/`SAMPLE_RATE`, so unlike `Tapped` there's no per-track
/// channel count or sample rate to track.
struct TappedSink {
    inner: Box<dyn Sink>,
    tap: Arc<AudioTap>,
    /// Accumulates downmixed mono samples until there's a full
    /// `player::WINDOW` to publish.
    window: Vec<f32>,
}

impl TappedSink {
    fn new(inner: Box<dyn Sink>, tap: Arc<AudioTap>) -> Self {
        Self {
            inner,
            tap,
            window: Vec::with_capacity(player::WINDOW),
        }
    }
}

impl Sink for TappedSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.inner.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if let Ok(samples) = packet.samples() {
            for frame in samples.chunks_exact(NUM_CHANNELS as usize) {
                let mono = (frame.iter().sum::<f64>() / f64::from(NUM_CHANNELS)) as f32;
                self.window.push(mono);
                if self.window.len() >= player::WINDOW {
                    self.tap.publish(&self.window, SAMPLE_RATE);
                    self.window.clear();
                }
            }
        }
        self.inner.write(packet, converter)
    }
}

fn vol_to_ls(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * f64::from(u16::MAX) as f32) as u16
}

enum Cmd {
    Load {
        uri: String,
        duration_ms: u32,
        start_paused: bool,
        position_ms: u32,
        cache: bool,
    },
    Preload(String),
    Toggle,
    Seek(u32),
    SetVolume(f32),
    /// `ack` fires once `player.stop()` has been issued to librespot —
    /// `Player::stop` blocks on it so `Session::start_playback` can't start
    /// a different player before this one has been told to go silent (see
    /// that call site's doc for the race this closes).
    Stop { ack: oneshot::Sender<()> },
}

/// What to re-`load` once a `SessionDied` reconnect succeeds — captured from
/// `cur`/`duration_ms`/`playback_start`/`snap` right before tearing the dead
/// session down. `paused` carries the pre-death play state across the
/// reconnect so it isn't lost — see `capture_resume`.
struct PendingResume {
    uri: String,
    duration_ms: u32,
    position_ms: u32,
    paused: bool,
}

#[derive(Clone)]
struct Snap {
    state: PlayerState,
    position_ms: u32,
    duration_ms: u32,
    volume: f32,
}

impl Snap {
    fn status(&self) -> PlayerStatus {
        PlayerStatus {
            state: self.state.clone(),
            position_ms: self.position_ms,
            duration_ms: self.duration_ms,
            volume: self.volume,
        }
    }
}

pub struct SpotifyPlayer {
    tx: mpsc::UnboundedSender<Cmd>,
    snap: Arc<Mutex<Snap>>,
    tap: Arc<AudioTap>,
    /// Runtime handle + connected librespot session, for `open_for_scan`.
    /// `None` until the worker's session comes up (or forever, if it never
    /// does) — independent of playback state, since a scan must never touch
    /// or race the playback command channel.
    scan: Arc<Mutex<Option<(tokio::runtime::Handle, Session)>>>,
}

impl SpotifyPlayer {
    /// Spawn the worker. Blocks only long enough to hand the credentials to the
    /// worker thread; the librespot session connects asynchronously in there.
    pub fn new(auth: Auth, bus: Bus, volume: f32, media_cache: Arc<MediaCache>) -> Self {
        let snap = Arc::new(Mutex::new(Snap {
            state: PlayerState::Stopped,
            position_ms: 0,
            duration_ms: 0,
            volume,
        }));
        let tap = Arc::new(AudioTap::default());
        let scan = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::unbounded_channel();
        let worker_snap = snap.clone();
        let worker_tap = tap.clone();
        let worker_scan = scan.clone();
        std::thread::Builder::new()
            .name("spotify-player".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("spotify: cannot start tokio runtime: {e}");
                        return;
                    }
                };
                rt.block_on(run(auth, bus, rx, worker_snap, worker_tap, worker_scan, media_cache));
            })
            .expect("spawn spotify-player worker");
        Self { tx, snap, tap, scan }
    }
}

impl Player for SpotifyPlayer {
    fn accepts(&self, r: &Rendition) -> bool {
        r.source.as_str() == "spotify"
    }

    fn load(&self, r: &Rendition, start_paused: bool, position_ms: u32, cache: bool) {
        let _ = self.tx.send(Cmd::Load {
            uri: r.uri.clone(),
            duration_ms: r.duration_ms,
            start_paused,
            position_ms,
            cache,
        });
    }

    fn preload(&self, r: &Rendition) {
        let _ = self.tx.send(Cmd::Preload(r.uri.clone()));
    }

    fn toggle(&self) {
        let _ = self.tx.send(Cmd::Toggle);
    }

    fn seek(&self, position_ms: u32) {
        let _ = self.tx.send(Cmd::Seek(position_ms));
    }

    fn set_volume(&self, v: f32) {
        let _ = self.tx.send(Cmd::SetVolume(v));
    }

    /// Blocks until the worker has issued the stop to librespot — see
    /// `Cmd::Stop`'s doc.
    fn stop(&self) {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Cmd::Stop { ack }).is_ok() {
            let _ = rx.blocking_recv();
        }
    }

    fn status(&self) -> PlayerStatus {
        self.snap.lock().unwrap_or_else(|e| e.into_inner()).status()
    }

    fn scan_fetch_paused(&self) -> bool {
        self.snap.lock().unwrap_or_else(|e| e.into_inner()).state == PlayerState::Playing
    }

    fn levels(&self) -> [f32; 5] {
        let window = self.tap.snapshot();
        player::spectrum::bands(&window.samples, window.sample_rate)
    }

    fn open_for_scan(
        &self,
        r: &Rendition,
        mode: core::ScanFetchMode,
    ) -> core::Result<Box<dyn ReadSeek + Send>> {
        let guard = self.scan.lock().unwrap_or_else(|e| e.into_inner());
        let Some((handle, session)) = guard.as_ref() else {
            return Err(Error::Unsupported("spotify: session not connected yet"));
        };
        let (handle, session) = (handle.clone(), session.clone());
        drop(guard);
        handle.block_on(crate::scan_audio::fetch_scan_audio(&session, &r.uri, mode))
    }
}

fn session_config() -> SessionConfig {
    SessionConfig {
        client_id: MUSIC_CLIENT_ID.to_string(),
        ..Default::default()
    }
}

async fn connect(auth: &Auth, credentials: Credentials) -> Result<Session, String> {
    let cache: Cache = Auth::cache(&auth.cache_dir)?;
    let session = Session::new(session_config(), Some(cache));
    session.connect(credentials, true).await.map_err(|e| e.to_string())?;
    // `connect(_, true)` re-saves credentials.json via librespot's own Cache
    // on every (re)connect — pin its mode down each time, since librespot
    // only applies its `0o600` open mode on first create.
    crate::auth::chmod_600(&auth.cache_dir.join("credentials.json"));
    Ok(session)
}

/// Bounded exponential backoff between reconnect attempts: 1s, 2s, 4s, ...,
/// capped at 60s. `attempt` is 0-indexed (0 = first retry after the initial
/// failed `connect()`). Capping avoids hammering Spotify's access point
/// while an outage is ongoing; retrying forever (rather than giving up) is
/// the whole point of this fix.
fn reconnect_backoff(attempt: u32) -> Duration {
    const BASE_SECS: u64 = 1;
    const CAP_SECS: u64 = 60;
    let secs = BASE_SECS
        .saturating_mul(1u64 << attempt.min(6))
        .min(CAP_SECS);
    Duration::from_secs(secs)
}

/// Sleeps out `backoff`, draining (and discarding) any commands sent in the
/// meantime — there's no live session/player to act on them. Returns `false`
/// as soon as the command channel closes, so quitting mid-backoff doesn't
/// hang shutdown.
async fn wait_for_reconnect(rx: &mut mpsc::UnboundedReceiver<Cmd>, backoff: Duration) -> bool {
    let sleep = tokio::time::sleep(backoff);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return true,
            cmd = rx.recv() => match cmd {
                None => return false,
                Some(_) => continue,
            },
        }
    }
}

/// Why the inner playback loop ended.
enum LoopExit {
    /// Command channel closed — `SpotifyPlayer` (the whole app) is shutting
    /// down. Exit `run` for real.
    Shutdown,
    /// The session died (dead TCP/Shannon link, `is_invalid()`) or
    /// librespot's own event channel closed — both mean the `Session` is
    /// unusable and everything built on it must be torn down and rebuilt.
    /// The outer loop reconnects and, if something was playing, resumes it —
    /// see `PendingResume`.
    SessionDied,
}

/// Captures what to resume from the current playback state, right before
/// tearing a dead session down. `None` if nothing was actually
/// playing/loaded (nothing to resume). `paused` comes from `snap` (the
/// state the last librespot `Paused`/`Playing` event set) rather than
/// `playback_start` alone, so a session that dies while paused reconnects
/// back into the paused state instead of silently resuming playback —
/// `playback_start` is already `None` whenever paused, which previously also
/// made the resumed position collapse to 0 instead of the actual paused
/// position.
fn capture_resume(
    cur: &Option<(SourceId, String)>,
    duration_ms: u32,
    playback_start: Option<Instant>,
    snap: &Snap,
) -> Option<PendingResume> {
    let (_, uri) = cur.as_ref()?;
    let position_ms = playback_start
        .map(|start| start.elapsed().as_millis() as u32)
        .unwrap_or(snap.position_ms);
    Some(PendingResume {
        uri: uri.clone(),
        duration_ms,
        position_ms,
        paused: snap.state == PlayerState::Paused,
    })
}

/// The bits of worker state a load needs to touch — bundled so `do_load`
/// stays under clippy's argument-count limit.
struct LoadCtx<'a> {
    player: &'a LsPlayer,
    bus: &'a Bus,
    snap: &'a Arc<Mutex<Snap>>,
}

/// Body of a `Cmd::Load`: validate the URI, update `snap`, kick off the
/// actual `player.load`, and compute the resulting `playback_start`. Shared
/// by the `Cmd::Load` command arm and the post-reconnect auto-resume path so
/// there's exactly one place that knows how to start a track playing.
/// `duration_ms` is the rendition's; `LsEvent::TrackChanged` corrects it.
fn do_load(
    ctx: LoadCtx<'_>,
    uri: String,
    duration_ms: u32,
    start_paused: bool,
    position_ms: u32,
) -> Option<((SourceId, String), Option<Instant>)> {
    let Ok(sp_uri) = SpotifyUri::from_uri(&uri) else {
        log::warn!("spotify: bad uri {uri}");
        ctx.bus.send(CoreEvent::Player(PlayerEvent::Finished {
            source: crate::source_id(),
            uri,
        }));
        return None;
    };
    if !sp_uri.is_playable() {
        ctx.bus.send(CoreEvent::Player(PlayerEvent::Finished {
            source: crate::source_id(),
            uri,
        }));
        return None;
    }
    let cur = (crate::source_id(), uri.clone());
    {
        let mut s = ctx.snap.lock().unwrap_or_else(|e| e.into_inner());
        s.state = if start_paused {
            PlayerState::Paused
        } else {
            PlayerState::Playing
        };
        s.position_ms = position_ms;
        s.duration_ms = duration_ms;
    }
    ctx.bus.send(CoreEvent::Player(PlayerEvent::Loading {
        source: crate::source_id(),
        uri: uri.clone(),
    }));
    ctx.player.load(sp_uri, !start_paused, position_ms);
    let playback_start = if start_paused {
        None
    } else {
        Some(Instant::now() - Duration::from_millis(position_ms as u64))
    };
    Some((cur, playback_start))
}

/// Worker entry point. Owns the librespot `Player`, mixer and session, and
/// reconnects from scratch (new `Session`, new `Player`/mixer) whenever the
/// session dies underneath it — see module docs / the bug this fixes:
/// librespot never recovers a dead `Session` on its own.
async fn run(
    auth: Auth,
    bus: Bus,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    snap: Arc<Mutex<Snap>>,
    tap: Arc<AudioTap>,
    scan: Arc<Mutex<Option<(tokio::runtime::Handle, Session)>>>,
    media_cache: Arc<MediaCache>,
) {
    // Set only from the `SessionDied` exit path (never from a clean
    // `Shutdown`, an explicit `Cmd::Stop`, or a legitimate `EndOfTrack`/
    // `Unavailable`) — carries what was playing across a reconnect so it can
    // be resumed once the new session/player are up.
    let mut resume: Option<PendingResume> = None;

    // How many *consecutive* short-lived sessions have died in a row (reset
    // once a session survives `SESSION_HEALTHY_AFTER`). Without this, a
    // session that connects successfully but dies again immediately (e.g.
    // the AP accepts the connection but is otherwise unreachable) would
    // reconnect in a tight loop with no backoff at all — `attempt` below only
    // covers the initial `connect()` *failing*, not a session dying right
    // after it succeeds.
    let mut session_died_streak: u32 = 0;

    loop {
        // No live session while (re)connecting; open_for_scan already treats
        // `None` as "not connected yet".
        *scan.lock().unwrap_or_else(|e| e.into_inner()) = None;

        if session_died_streak > 0 {
            let backoff = reconnect_backoff(session_died_streak - 1);
            log::warn!(
                "spotify: session died {session_died_streak} times in a row, backing off {backoff:?} before reconnecting"
            );
            if !wait_for_reconnect(&mut rx, backoff).await {
                log::info!("spotify: worker stopped (shutdown during reconnect backoff)");
                return;
            }
        }

        let mut attempt: u32 = 0;
        let session = loop {
            match connect(&auth, auth.credentials.clone()).await {
                Ok(s) => break s,
                Err(e) => {
                    log::error!("spotify: session connect failed: {e}");
                    let backoff = reconnect_backoff(attempt);
                    attempt = attempt.saturating_add(1);
                    log::warn!("spotify: reconnect attempt {attempt} in {backoff:?}");
                    if !wait_for_reconnect(&mut rx, backoff).await {
                        log::info!("spotify: worker stopped (shutdown during reconnect)");
                        return;
                    }
                }
            }
        };
        log::info!("spotify: session connected");
        let session_established_at = Instant::now();
        *scan.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((tokio::runtime::Handle::current(), session.clone()));

        let mixer_fn =
            librespot_playback::mixer::find(Some(SoftMixer::NAME)).expect("softvol mixer present");
        let mixer = match mixer_fn(MixerConfig::default()) {
            Ok(m) => m,
            Err(e) => {
                log::error!("spotify: mixer init failed: {e}");
                return;
            }
        };
        let current_volume = snap.lock().unwrap_or_else(|e| e.into_inner()).volume;
        mixer.set_volume(vol_to_ls(current_volume));

        let backend = audio_backend::BACKENDS
            .first()
            .expect("a librespot audio backend is compiled in")
            .1;
        let player = LsPlayer::new(
            // Max quality; librespot falls back to 160/96 per-track/account
            // if a 320kbps rendition isn't available, so this never errors.
            PlayerConfig { bitrate: Bitrate::Bitrate320, ..PlayerConfig::default() },
            session.clone(),
            mixer.get_soft_volume(),
            {
                let tap = tap.clone();
                move || {
                    let inner = (backend)(None, AudioFormat::default());
                    Box::new(TappedSink::new(inner, tap.clone())) as Box<dyn Sink>
                }
            },
        );
        let mut events = player.get_player_event_channel();

        let mut cur: Option<(SourceId, String)> = None;
        let mut cur_cache = true;
        let mut duration_ms: u32 = 0;
        let mut playback_start: Option<Instant> = None;
        // Whether `PlayerEvent::Materialized` has already been sent for
        // `cur` — reset on every new load so the tick branch below polls
        // (and eventually announces) each track exactly once.
        let mut materialized_sent = false;
        // `TrackChanged`'s item, held until a `Playing`/`Paused` for `cur` proves it isn't a superseded load's.
        let mut pending_item: Option<Box<AudioItem>> = None;
        let mut cur_item: Option<Box<AudioItem>> = None;
        let mut tick = tokio::time::interval(TICK);

        if let Some(pending) = resume.take() {
            log::info!(
                "spotify: reconnected, resuming {} (paused={})",
                pending.uri,
                pending.paused
            );
            let ctx = LoadCtx { player: &player, bus: &bus, snap: &snap };
            if let Some((c, p)) =
                do_load(ctx, pending.uri, pending.duration_ms, pending.paused, pending.position_ms)
            {
                cur = Some(c);
                cur_cache = true;
                duration_ms = pending.duration_ms;
                playback_start = p;
                materialized_sent = false;
            }
        }

        let exit = 'inner: loop {
            tokio::select! {
                    cmd = rx.recv() => match cmd {
                        None => break 'inner LoopExit::Shutdown,
                        Some(Cmd::Load { uri, duration_ms: hint, start_paused, position_ms, cache }) => {
                        let ctx = LoadCtx { player: &player, bus: &bus, snap: &snap };
                        if let Some((c, p)) = do_load(ctx, uri, hint, start_paused, position_ms) {
                            cur = Some(c);
                            cur_cache = cache;
                            duration_ms = hint;
                            playback_start = p;
                            materialized_sent = false;
                            pending_item = None;
                            cur_item = None;
                        }
                    }
                    Some(Cmd::Preload(uri)) => match SpotifyUri::from_uri(&uri) {
                        Ok(sp_uri) if sp_uri.is_playable() => {
                            log::debug!("spotify: preloading {uri}");
                            player.preload(sp_uri);
                        }
                        _ => log::warn!("spotify: cannot preload {uri}"),
                    },
                    Some(Cmd::Toggle) => {
                        let playing = snap.lock().unwrap_or_else(|e| e.into_inner()).state == PlayerState::Playing;
                        if playing { player.pause() } else { player.play() }
                    }
                    Some(Cmd::Seek(ms)) => {
                        player.seek(ms);
                        if playback_start.is_some() {
                            playback_start = Some(Instant::now() - Duration::from_millis(u64::from(ms)));
                        }
                        snap.lock().unwrap_or_else(|e| e.into_inner()).position_ms = ms;
                    }
                    Some(Cmd::SetVolume(v)) => {
                        mixer.set_volume(vol_to_ls(v));
                        snap.lock().unwrap_or_else(|e| e.into_inner()).volume = v.clamp(0.0, 1.0);
                    }
                    Some(Cmd::Stop { ack }) => {
                        player.stop();
                        cur = None;
                        playback_start = None;
                        set_state(&snap, PlayerState::Stopped, 0);
                        bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                        let _ = ack.send(());
                    }
                },
                ev = events.recv() => match ev {
                    None => {
                        log::warn!("spotify: librespot event channel closed, reconnecting");
                        resume = capture_resume(&cur, duration_ms, playback_start, &snap.lock().unwrap_or_else(|e| e.into_inner()));
                        break 'inner LoopExit::SessionDied;
                    }
                    Some(LsEvent::Playing { position_ms, track_id, .. }) => {
                        if is_cur(&cur, &track_id) && pending_item.is_some() {
                            cur_item = pending_item.take();
                        }
                        playback_start = Some(Instant::now() - Duration::from_millis(position_ms as u64));
                        set_state(&snap, PlayerState::Playing, position_ms);
                        if let Some((source, uri)) = cur.clone() {
                            bus.send(CoreEvent::Player(PlayerEvent::Playing { source, uri }));
                        }
                    }
                    Some(LsEvent::Paused { position_ms, track_id, .. }) => {
                        if is_cur(&cur, &track_id) && pending_item.is_some() {
                            cur_item = pending_item.take();
                        }
                        playback_start = None;
                        set_state(&snap, PlayerState::Paused, position_ms);
                        bus.send(CoreEvent::Player(PlayerEvent::Paused));
                    }
                    Some(LsEvent::Stopped { .. }) => {
                        playback_start = None;
                        set_state(&snap, PlayerState::Stopped, 0);
                        bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                    }
                    Some(LsEvent::TrackChanged { audio_item }) => {
                        if audio_item.duration_ms > 0 {
                            duration_ms = audio_item.duration_ms;
                            snap.lock().unwrap_or_else(|e| e.into_inner()).duration_ms = duration_ms;
                        }
                        pending_item = Some(audio_item);
                    }
                    Some(LsEvent::TimeToPreloadNextTrack { .. }) => {
                        if let Some((source, uri)) = cur.clone() {
                            bus.send(CoreEvent::Player(PlayerEvent::PreloadHint { source, uri }));
                        }
                    }
                    // A load that failed only because the AP died must resume, not skip.
                    Some(LsEvent::Unavailable { .. }) if session.is_invalid() => {
                        log::warn!("spotify: track unavailable on a dead session, reconnecting");
                        resume = capture_resume(&cur, duration_ms, playback_start, &snap.lock().unwrap_or_else(|e| e.into_inner()));
                        break 'inner LoopExit::SessionDied;
                    }
                    Some(LsEvent::EndOfTrack { .. }) | Some(LsEvent::Unavailable { .. }) => {
                        playback_start = None;
                        set_state(&snap, PlayerState::Stopped, 0);
                        if let Some((source, uri)) = cur.take() {
                            bus.send(CoreEvent::Player(PlayerEvent::Finished { source, uri }));
                        }
                    }
                    Some(_) => {}
                },
                _ = tick.tick() => {
                    if session.is_invalid() {
                        log::warn!("spotify: session invalid (dead access-point connection), reconnecting");
                        resume = capture_resume(&cur, duration_ms, playback_start, &snap.lock().unwrap_or_else(|e| e.into_inner()));
                        break 'inner LoopExit::SessionDied;
                    }
                    if let (Some(start), Some(_)) = (playback_start, cur.as_ref()) {
                        let pos = start.elapsed().as_millis() as u32;
                        snap.lock().unwrap_or_else(|e| e.into_inner()).position_ms = pos;
                        bus.send(CoreEvent::Player(PlayerEvent::Progress {
                            position_ms: pos,
                            duration_ms,
                        }));
                    }
                    if !materialized_sent
                        && let (Some((source, uri)), Some(item)) = (cur.clone(), cur_item.as_ref())
                        && crate::scan_audio::is_materialized(&session, item)
                    {
                        materialized_sent = true;
                        log::debug!("spotify: materialized {uri}");
                        bus.send(CoreEvent::Player(PlayerEvent::Materialized { source: source.clone(), uri: uri.clone() }));
                        if cur_cache {
                            spawn_materialize_to_cache(session.clone(), media_cache.clone(), source, uri, item.clone());
                        }
                    }
                }
            }
        };

        player.stop();
        session.shutdown();

        match exit {
            LoopExit::Shutdown => {
                log::info!("spotify: worker stopped");
                return;
            }
            LoopExit::SessionDied => {
                // Plain `Stopped`, not `Finished` for `cur` — `Finished`
                // would advance the queue into another track that would
                // just fail the same way before reconnection completes.
                // `resume` (captured above, from `cur`/`playback_start`)
                // carries the interrupted track+position across the
                // reconnect; once the outer loop gets a fresh session/player
                // up, it re-`load`s the same track at the same position
                // instead of leaving playback stopped for good.
                bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                set_state(&snap, PlayerState::Stopped, 0);

                // A session that stayed up a while wasn't a crash loop —
                // treat it as recovered so a later, unrelated death starts
                // its backoff from scratch instead of picking up a stale
                // streak.
                if session_established_at.elapsed() >= SESSION_HEALTHY_AFTER {
                    session_died_streak = 0;
                } else {
                    session_died_streak = session_died_streak.saturating_add(1);
                }
            }
        }
    }
}

/// Once `is_materialized`, copy the decrypted Ogg into the shared `MediaCache` so a `CacheOnly` scan can read it.
fn spawn_materialize_to_cache(
    session: Session,
    media_cache: Arc<MediaCache>,
    source: SourceId,
    uri: String,
    item: Box<AudioItem>,
) {
    tokio::spawn(async move {
        let audio = match crate::scan_audio::open_materialized(&session, &uri, &item).await {
            Ok(audio) => audio,
            Err(e) => {
                log::debug!("spotify: materialize-to-cache open failed for {uri}: {e}");
                return;
            }
        };
        let copied = tokio::task::spawn_blocking(move || {
            let bytes = core::audio_decode::read_all(audio).map_err(|e| format!("read failed for {uri}: {e}"))?;
            media_cache.put(&source, &uri, &bytes).map_err(|e| format!("write failed for {uri}: {e}"))
        })
        .await;
        match copied {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log::warn!("spotify: materialize-to-cache {e}"),
            Err(e) => log::warn!("spotify: materialize-to-cache task failed: {e}"),
        }
    });
}

fn is_cur(cur: &Option<(SourceId, String)>, track_id: &SpotifyUri) -> bool {
    cur.as_ref().is_some_and(|(_, uri)| track_id.to_uri().is_ok_and(|t| &t == uri))
}

fn set_state(snap: &Arc<Mutex<Snap>>, state: PlayerState, position_ms: u32) {
    let mut s = snap.lock().unwrap_or_else(|e| e.into_inner());
    s.state = state;
    s.position_ms = position_ms;
}

