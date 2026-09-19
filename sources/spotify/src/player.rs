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
/// reconnect (not part of a crash loop), resetting `Link::died_streak`.
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

#[derive(Clone)]
struct LoadReq {
    uri: String,
    duration_ms: u32,
    start_paused: bool,
    position_ms: u32,
    cache: bool,
}

enum Cmd {
    Load(LoadReq),
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
            state: self.state,
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
    scan: ScanHandle,
}

/// Runtime handle + live librespot session for `open_for_scan`; `None` while the link is down.
type ScanHandle = Arc<Mutex<Option<(tokio::runtime::Handle, Session)>>>;

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
        let _ = self.tx.send(Cmd::Load(LoadReq {
            uri: r.uri.clone(),
            duration_ms: r.duration_ms,
            start_paused,
            position_ms,
            cache,
        }));
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
            || self.scan.lock().unwrap_or_else(|e| e.into_inner()).is_none()
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

async fn connect(auth: &Auth) -> Result<Session, String> {
    let cache: Cache = Auth::cache(&auth.cache_dir)?;
    let session = Session::new(session_config(), Some(cache));
    session.connect(auth.credentials.clone(), true).await.map_err(|e| e.to_string())?;
    // `connect(_, true)` re-saves credentials.json via librespot's own Cache
    // on every (re)connect — pin its mode down each time, since librespot
    // only applies its `0o600` open mode on first create.
    crate::auth::chmod_600(&auth.cache_dir.join("credentials.json"));
    Ok(session)
}

/// Bounded exponential backoff between reconnect attempts: 1s, 2s, 4s, ...,
/// capped at 60s, so an ongoing outage doesn't hammer Spotify's access point.
fn reconnect_backoff(attempt: u32) -> Duration {
    const BASE_SECS: u64 = 1;
    const CAP_SECS: u64 = 60;
    let secs = BASE_SECS
        .saturating_mul(1u64 << attempt.min(6))
        .min(CAP_SECS);
    Duration::from_secs(secs)
}

/// The AP link: a dead `Session` is replaced in the background while the loaded track keeps streaming off the CDN.
struct Link {
    auth: Arc<Auth>,
    session: Session,
    up: bool,
    /// Bumped per adopted session, so a load can tell whether the link changed under it.
    generation: u32,
    connecting: Option<oneshot::Receiver<Session>>,
    established_at: Instant,
    /// Consecutive sessions that died before `SESSION_HEALTHY_AFTER`; backs off the next connect.
    died_streak: u32,
    scan: ScanHandle,
}

impl Link {
    fn new(auth: Auth, scan: ScanHandle) -> Self {
        let mut link = Self {
            auth: Arc::new(auth),
            // Never connected: lets the player exist before the first connect lands.
            session: Session::new(session_config(), None),
            up: false,
            generation: 0,
            connecting: None,
            established_at: Instant::now(),
            died_streak: 0,
            scan,
        };
        link.spawn_connect();
        link
    }

    fn spawn_connect(&mut self) {
        let (tx, rx) = oneshot::channel();
        let auth = self.auth.clone();
        let streak = self.died_streak;
        tokio::spawn(async move {
            if streak > 0 {
                let backoff = reconnect_backoff(streak - 1);
                log::warn!("spotify: session died {streak} times in a row, backing off {backoff:?} before reconnecting");
                tokio::time::sleep(backoff).await;
            }
            let mut attempt: u32 = 0;
            loop {
                match connect(&auth).await {
                    Ok(session) => {
                        let _ = tx.send(session);
                        return;
                    }
                    Err(e) => {
                        let backoff = reconnect_backoff(attempt);
                        attempt = attempt.saturating_add(1);
                        log::error!("spotify: session connect failed: {e}; attempt {attempt} retries in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });
        self.connecting = Some(rx);
    }

    /// The session while it's usable; noticing it died starts the background reconnect.
    fn live(&mut self) -> Option<&Session> {
        if self.up && self.session.is_invalid() {
            log::warn!("spotify: session invalid (dead access-point connection), reconnecting in the background");
            self.up = false;
            *self.scan.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.died_streak = if self.established_at.elapsed() >= SESSION_HEALTHY_AFTER {
                0
            } else {
                self.died_streak.saturating_add(1)
            };
            self.spawn_connect();
        }
        self.up.then_some(&self.session)
    }

    fn live_generation(&mut self) -> Option<u32> {
        self.live().is_some().then_some(self.generation)
    }

    /// Resolves with the replacement session once a background connect lands.
    async fn reconnected(&mut self) -> Session {
        loop {
            let Some(connecting) = self.connecting.as_mut() else {
                return std::future::pending().await;
            };
            match connecting.await {
                Ok(session) => {
                    self.connecting = None;
                    self.session = session.clone();
                    self.up = true;
                    self.generation = self.generation.wrapping_add(1);
                    self.established_at = Instant::now();
                    *self.scan.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((tokio::runtime::Handle::current(), session.clone()));
                    return session;
                }
                Err(_) => self.spawn_connect(),
            }
        }
    }
}

/// Why the inner playback loop ended.
enum LoopExit {
    /// Command channel closed — the whole app is shutting down.
    Shutdown,
    /// librespot's event channel closed: its player is gone and must be rebuilt.
    PlayerDied,
}

/// Worker-side state of the track the player has loaded.
struct Loaded {
    id: (SourceId, String),
    req: LoadReq,
    duration_ms: u32,
    playback_start: Option<Instant>,
    /// `Link::generation` the load went out on; `None` for one issued while the link was down.
    generation: Option<u32>,
    /// `TrackChanged`'s item, held until a `Playing`/`Paused` for this track proves it isn't a superseded load's.
    pending_item: Option<Box<AudioItem>>,
    item: Option<Box<AudioItem>>,
    materialized_sent: bool,
}

impl Loaded {
    fn confirm(&mut self, track_id: &SpotifyUri) {
        if self.pending_item.is_some() && track_id.to_uri().is_ok_and(|uri| uri == self.id.1) {
            self.item = self.pending_item.take();
        }
    }

    /// The request that picks this track back up where it is now.
    fn resume(&self, snap: &Snap) -> LoadReq {
        LoadReq {
            duration_ms: self.duration_ms,
            start_paused: snap.state == PlayerState::Paused,
            position_ms: self
                .playback_start
                .map_or(snap.position_ms, |start| start.elapsed().as_millis() as u32),
            ..self.req.clone()
        }
    }
}

/// Validates `req`, updates `snap`, announces `Loading` and hands the track to librespot.
fn do_load(
    player: &LsPlayer,
    bus: &Bus,
    snap: &Mutex<Snap>,
    req: LoadReq,
    generation: Option<u32>,
) -> Option<Loaded> {
    let sp_uri = match SpotifyUri::from_uri(&req.uri) {
        Ok(sp_uri) if sp_uri.is_playable() => sp_uri,
        _ => {
            log::warn!("spotify: cannot play {}", req.uri);
            bus.send(CoreEvent::Player(PlayerEvent::Finished {
                source: crate::source_id(),
                uri: req.uri,
            }));
            return None;
        }
    };
    {
        let mut s = snap.lock().unwrap_or_else(|e| e.into_inner());
        s.state = if req.start_paused {
            PlayerState::Paused
        } else {
            PlayerState::Playing
        };
        s.position_ms = req.position_ms;
        s.duration_ms = req.duration_ms;
    }
    bus.send(CoreEvent::Player(PlayerEvent::Loading {
        source: crate::source_id(),
        uri: req.uri.clone(),
    }));
    player.load(sp_uri, !req.start_paused, req.position_ms);
    Some(Loaded {
        id: (crate::source_id(), req.uri.clone()),
        duration_ms: req.duration_ms,
        playback_start: (!req.start_paused)
            .then(|| Instant::now() - Duration::from_millis(u64::from(req.position_ms))),
        generation,
        pending_item: None,
        item: None,
        materialized_sent: false,
        req,
    })
}

fn do_preload(player: &LsPlayer, uri: &str) {
    match SpotifyUri::from_uri(uri) {
        Ok(sp_uri) if sp_uri.is_playable() => {
            log::debug!("spotify: preloading {uri}");
            player.preload(sp_uri);
        }
        _ => log::warn!("spotify: cannot preload {uri}"),
    }
}

/// Worker entry point. Owns the librespot `Player`, mixer and `Link`.
async fn run(
    auth: Auth,
    bus: Bus,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    snap: Arc<Mutex<Snap>>,
    tap: Arc<AudioTap>,
    scan: ScanHandle,
    media_cache: Arc<MediaCache>,
) {
    let mut link = Link::new(auth, scan);
    // A load waiting for a live session; a newer one replaces it.
    let mut held: Option<LoadReq> = None;

    loop {
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
            link.session.clone(),
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

        let mut cur: Option<Loaded> = None;
        // The requested preload, and whether librespot got it on a live session.
        let mut preload: Option<(String, bool)> = None;
        let mut tick = tokio::time::interval(TICK);

        let exit = 'inner: loop {
            let generation = link.live_generation();
            // A track librespot already preloaded needs no session to start.
            let preloaded = |req: &mut LoadReq| {
                preload.as_ref().is_some_and(|(uri, issued)| *issued && *uri == req.uri)
            };
            if let Some(req) = held.take_if(|req| generation.is_some() || preloaded(req)) {
                preload = None;
                if let Some(loaded) = do_load(&player, &bus, &snap, req, generation) {
                    cur = Some(loaded);
                }
            }
            if generation.is_some()
                && let Some((uri, issued @ false)) = preload.as_mut()
            {
                do_preload(&player, uri);
                *issued = true;
            }

            tokio::select! {
                cmd = rx.recv() => match cmd {
                    None => break 'inner LoopExit::Shutdown,
                    Some(Cmd::Load(req)) => held = Some(req),
                    Some(Cmd::Preload(uri)) => preload = Some((uri, false)),
                    Some(Cmd::Toggle) => {
                        if let Some(req) = held.as_mut() {
                            req.start_paused = !req.start_paused;
                        }
                        if cur.is_some() {
                            let playing = snap.lock().unwrap_or_else(|e| e.into_inner()).state == PlayerState::Playing;
                            if playing { player.pause() } else { player.play() }
                        }
                    }
                    Some(Cmd::Seek(ms)) => {
                        player.seek(ms);
                        if let Some(start) = cur.as_mut().and_then(|c| c.playback_start.as_mut()) {
                            *start = Instant::now() - Duration::from_millis(u64::from(ms));
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
                        held = None;
                        preload = None;
                        set_state(&snap, PlayerState::Stopped, 0);
                        bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                        let _ = ack.send(());
                    }
                },
                session = link.reconnected() => {
                    log::info!("spotify: session connected");
                    player.set_session(session);
                }
                ev = events.recv() => match ev {
                    None => {
                        log::warn!("spotify: librespot player died, rebuilding it");
                        let snap = snap.lock().unwrap_or_else(|e| e.into_inner());
                        held = held.or(cur.map(|c| c.resume(&snap)));
                        break 'inner LoopExit::PlayerDied;
                    }
                    Some(LsEvent::Playing { position_ms, track_id, .. }) => {
                        set_state(&snap, PlayerState::Playing, position_ms);
                        if let Some(c) = cur.as_mut() {
                            c.confirm(&track_id);
                            c.playback_start = Some(Instant::now() - Duration::from_millis(u64::from(position_ms)));
                            let (source, uri) = c.id.clone();
                            bus.send(CoreEvent::Player(PlayerEvent::Playing { source, uri }));
                        }
                    }
                    Some(LsEvent::Paused { position_ms, track_id, .. }) => {
                        set_state(&snap, PlayerState::Paused, position_ms);
                        if let Some(c) = cur.as_mut() {
                            c.confirm(&track_id);
                            c.playback_start = None;
                        }
                        bus.send(CoreEvent::Player(PlayerEvent::Paused));
                    }
                    Some(LsEvent::Stopped { .. }) => {
                        if let Some(c) = cur.as_mut() {
                            c.playback_start = None;
                        }
                        set_state(&snap, PlayerState::Stopped, 0);
                        bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                    }
                    Some(LsEvent::TrackChanged { audio_item }) => {
                        if let Some(c) = cur.as_mut() {
                            if audio_item.duration_ms > 0 {
                                c.duration_ms = audio_item.duration_ms;
                                snap.lock().unwrap_or_else(|e| e.into_inner()).duration_ms = c.duration_ms;
                            }
                            c.pending_item = Some(audio_item);
                        }
                    }
                    Some(LsEvent::TimeToPreloadNextTrack { .. }) => {
                        if let Some((source, uri)) = cur.as_ref().map(|c| c.id.clone()) {
                            bus.send(CoreEvent::Player(PlayerEvent::PreloadHint { source, uri }));
                        }
                    }
                    // A load that failed only because the link died under it is retried, not skipped.
                    Some(LsEvent::Unavailable { .. })
                        if cur.as_ref().is_some_and(|c| c.generation != link.live_generation()) =>
                    {
                        log::warn!("spotify: track unavailable over a dead session, retrying once reconnected");
                        held = held.or(cur.take().map(|c| c.req));
                    }
                    Some(LsEvent::EndOfTrack { .. }) | Some(LsEvent::Unavailable { .. }) => {
                        set_state(&snap, PlayerState::Stopped, 0);
                        if let Some(c) = cur.take() {
                            let (source, uri) = c.id;
                            bus.send(CoreEvent::Player(PlayerEvent::Finished { source, uri }));
                        }
                    }
                    Some(_) => {}
                },
                _ = tick.tick() => {
                    let Some(c) = cur.as_mut() else { continue };
                    if let Some(start) = c.playback_start {
                        let pos = start.elapsed().as_millis() as u32;
                        snap.lock().unwrap_or_else(|e| e.into_inner()).position_ms = pos;
                        bus.send(CoreEvent::Player(PlayerEvent::Progress {
                            position_ms: pos,
                            duration_ms: c.duration_ms,
                        }));
                    }
                    if !c.materialized_sent
                        && let Some(item) = c.item.as_ref()
                        && let Some(session) = link.live()
                        && crate::scan_audio::is_materialized(session, item)
                    {
                        c.materialized_sent = true;
                        let (source, uri) = c.id.clone();
                        log::debug!("spotify: materialized {uri}");
                        bus.send(CoreEvent::Player(PlayerEvent::Materialized { source: source.clone(), uri: uri.clone() }));
                        if c.req.cache {
                            spawn_materialize_to_cache(session.clone(), media_cache.clone(), source, uri, item.clone(), bus.clone());
                        }
                    }
                }
            }
        };

        player.stop();
        match exit {
            LoopExit::Shutdown => {
                link.session.shutdown();
                log::info!("spotify: worker stopped");
                return;
            }
            LoopExit::PlayerDied => {
                bus.send(CoreEvent::Player(PlayerEvent::Stopped));
                set_state(&snap, PlayerState::Stopped, 0);
                // Keeps a player that dies on startup from spinning.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Copies the decrypted Ogg into `MediaCache`, then re-sends `Materialized` once it actually lands.
fn spawn_materialize_to_cache(
    session: Session,
    media_cache: Arc<MediaCache>,
    source: SourceId,
    uri: String,
    item: Box<AudioItem>,
    bus: Bus,
) {
    tokio::spawn(async move {
        let audio = match crate::scan_audio::open_materialized(&session, &uri, &item).await {
            Ok(audio) => audio,
            Err(e) => {
                log::debug!("spotify: materialize-to-cache open failed for {uri}: {e}");
                return;
            }
        };
        let source_for_put = source.clone();
        let uri_for_put = uri.clone();
        let copied = tokio::task::spawn_blocking(move || {
            let bytes = core::audio_decode::read_all(audio).map_err(|e| format!("read failed for {uri_for_put}: {e}"))?;
            media_cache.put(&source_for_put, &uri_for_put, &bytes).map_err(|e| format!("write failed for {uri_for_put}: {e}"))
        })
        .await;
        match copied {
            Ok(Ok(_)) => bus.send(CoreEvent::Player(PlayerEvent::Materialized { source, uri })),
            Ok(Err(e)) => log::warn!("spotify: materialize-to-cache {e}"),
            Err(e) => log::warn!("spotify: materialize-to-cache task failed: {e}"),
        }
    });
}

fn set_state(snap: &Arc<Mutex<Snap>>, state: PlayerState, position_ms: u32) {
    let mut s = snap.lock().unwrap_or_else(|e| e.into_inner());
    s.state = state;
    s.position_ms = position_ms;
}

