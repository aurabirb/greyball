//! The AP link: a dead `Session` is replaced in the background while a track already streaming keeps
//! reading off the CDN. `Slot` is what `SpotifyMediaProvider` waits on for a live session.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use tokio::sync::oneshot;

use crate::auth::{Auth, MUSIC_CLIENT_ID};

/// A session that stays up at least this long counts as a successful
/// reconnect (not part of a crash loop), resetting `Link::died_streak`.
const SESSION_HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// How often the link task checks that the session is still valid.
const CHECK_EVERY: Duration = Duration::from_millis(500);

/// Failed opens in a row (no success between) that mark the session as bad.
const WEDGED_AFTER_FAILURES: u32 = 2;

#[derive(Clone)]
pub struct Live {
    pub handle: tokio::runtime::Handle,
    pub session: Session,
    /// Bumped per adopted session, so an open can tell whether the link changed under it.
    pub generation: u32,
}

#[derive(Default)]
pub struct Slot {
    live: Mutex<Option<Live>>,
    cond: Condvar,
    failures: AtomicU32,
}

impl Slot {
    fn set(&self, live: Option<Live>) {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = live;
        self.cond.notify_all();
    }

    pub fn current(&self) -> Option<Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Waits for a live session; `None` once `wanted` turns false.
    pub fn wait_live(&self, wanted: &dyn Fn() -> bool) -> Option<Live> {
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !wanted() {
                return None;
            }
            if let Some(live) = guard.as_ref() {
                return Some(live.clone());
            }
            guard = self.cond.wait_timeout(guard, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    pub fn succeeded(&self) {
        self.failures.store(0, Ordering::SeqCst);
    }

    /// Key requests can time out on an AP link that still looks alive; recycling makes the link reconnect.
    pub fn failed(&self, live: &Live) {
        if self.failures.fetch_add(1, Ordering::SeqCst) + 1 >= WEDGED_AFTER_FAILURES {
            log::warn!("spotify: {WEDGED_AFTER_FAILURES} opens failed in a row, recycling the session");
            live.session.shutdown();
            self.failures.store(0, Ordering::SeqCst);
        }
    }
}

fn session_config() -> SessionConfig {
    SessionConfig { client_id: MUSIC_CLIENT_ID.to_string(), ..Default::default() }
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
    let secs = BASE_SECS.saturating_mul(1u64 << attempt.min(6)).min(CAP_SECS);
    Duration::from_secs(secs)
}

struct Link {
    auth: Arc<Auth>,
    session: Session,
    up: bool,
    generation: u32,
    connecting: Option<oneshot::Receiver<Session>>,
    established_at: Instant,
    /// Consecutive sessions that died before `SESSION_HEALTHY_AFTER`; backs off the next connect.
    died_streak: u32,
    slot: Arc<Slot>,
}

impl Link {
    fn new(auth: Auth, slot: Arc<Slot>) -> Self {
        let mut link = Self {
            auth: Arc::new(auth),
            // Never connected: lets the link exist before the first connect lands.
            session: Session::new(session_config(), None),
            up: false,
            generation: 0,
            connecting: None,
            established_at: Instant::now(),
            died_streak: 0,
            slot,
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

    /// Noticing the session died starts the background reconnect.
    fn check(&mut self) {
        if self.up && self.session.is_invalid() {
            log::warn!("spotify: session invalid (dead access-point connection), reconnecting in the background");
            self.up = false;
            self.slot.set(None);
            self.died_streak = if self.established_at.elapsed() >= SESSION_HEALTHY_AFTER {
                0
            } else {
                self.died_streak.saturating_add(1)
            };
            self.spawn_connect();
        }
    }

    /// Resolves once a background connect lands.
    async fn reconnected(&mut self) {
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
                    self.slot.set(Some(Live { handle: tokio::runtime::Handle::current(), session, generation: self.generation }));
                    return;
                }
                Err(_) => self.spawn_connect(),
            }
        }
    }
}

/// Starts the connection manager on its own thread; it ends (closing the session) once every `Slot` holder is gone.
pub fn spawn(auth: Auth) -> Arc<Slot> {
    let slot = Arc::new(Slot::default());
    let task_slot = slot.clone();
    std::thread::Builder::new()
        .name("spotify-link".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("spotify: cannot start tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(async {
                let mut link = Link::new(auth, task_slot.clone());
                let mut tick = tokio::time::interval(CHECK_EVERY);
                while Arc::strong_count(&task_slot) > 1 {
                    tokio::select! {
                        () = link.reconnected() => log::info!("spotify: session connected"),
                        _ = tick.tick() => link.check(),
                    }
                }
                link.session.shutdown();
                log::info!("spotify: link stopped");
            });
        })
        .expect("spawn spotify-link thread");
    slot
}
