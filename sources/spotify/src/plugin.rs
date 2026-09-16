//! `core::Plugin` impl — non-blocking startup health + deferred OAuth login.
//! See `core::plugin` for why this exists: this used to be an OAuth browser
//! flow (`Auth::login`) run synchronously during `app` startup, blocking
//! the whole TUI behind it whenever there were no cached credentials yet.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{
    Bus, CoreEvent, MediaCache, Player, Plugin, PluginCommand, PluginHealth, SetupKind, Source, SourceId,
    Wiring,
};

use crate::auth::Auth;
use crate::player::SpotifyPlayer;
use crate::source::SpotifySource;

/// Cooldown between automatic background refresh attempts kicked off from
/// `probe` (called on every redraw) — keeps a permanently-failing refresh
/// (e.g. a revoked grant) from hammering Spotify's token endpoint.
const AUTO_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

pub struct SpotifyPlugin {
    cache_dir: PathBuf,
    bus: Bus,
    volume: f32,
    media_cache: Arc<MediaCache>,
    /// Guards against `probe` spawning overlapping automatic refresh
    /// attempts (it's called on every redraw, ~4/s).
    refreshing: Arc<AtomicBool>,
    next_auto_refresh: Arc<Mutex<Instant>>,
}

impl SpotifyPlugin {
    pub fn new(cache_dir: PathBuf, bus: Bus, volume: f32, media_cache: Arc<MediaCache>) -> Self {
        crate::auth::heal_permissions(&cache_dir);
        Self {
            cache_dir,
            bus,
            volume,
            media_cache,
            refreshing: Arc::new(AtomicBool::new(false)),
            next_auto_refresh: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Fire-and-forget: the same non-interactive refresh `webapi.rs` runs on
    /// a 401, kicked off automatically the moment `probe` notices the
    /// cached token is stale but still has a refresh_token. Without this,
    /// the token only got refreshed either by an incidental Web API call
    /// hitting a 401, or by the user pressing Enter on the warning (which
    /// runs `setup` -> `Auth::login`, which happens to also refresh) —
    /// otherwise the warning just sat there until one of those happened.
    fn kick_off_auto_refresh(&self) {
        if self.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut next = self.next_auto_refresh.lock().unwrap();
            if Instant::now() < *next {
                self.refreshing.store(false, Ordering::SeqCst);
                return;
            }
            *next = Instant::now() + AUTO_REFRESH_COOLDOWN;
        }
        let cache_dir = self.cache_dir.clone();
        let bus = self.bus.clone();
        let refreshing = self.refreshing.clone();
        std::thread::spawn(move || {
            let refreshed = crate::auth::refresh_and_persist(&cache_dir).is_some();
            refreshing.store(false, Ordering::SeqCst);
            if refreshed {
                // `probe` would pick this up on the next periodic redraw
                // regardless, but this makes the warning clear immediately.
                bus.send(CoreEvent::PluginStatusChanged);
            }
        });
    }
}

impl Plugin for SpotifyPlugin {
    fn id(&self) -> SourceId {
        SourceId::from("spotify")
    }

    fn probe(&self) -> PluginHealth {
        if Auth::load_cached(&self.cache_dir).is_some() {
            return PluginHealth::Ok;
        }
        if crate::auth::has_refresh_token(&self.cache_dir) {
            self.kick_off_auto_refresh();
            return PluginHealth::Warn("refreshing session — should clear on its own".to_string());
        }
        PluginHealth::Warn("not logged in — select to open a browser and log in".to_string())
    }

    fn setup_kind(&self) -> SetupKind {
        SetupKind::Action
    }

    fn wiring(&self) -> Wiring {
        // Unlike SoundCloud, Spotify has no anonymous mode at all — no
        // token means nothing to build. `Warn` health here means "not
        // registered yet", not "registered but degraded".
        let Some(auth) = Auth::load_cached(&self.cache_dir) else {
            return Wiring::default();
        };
        let source: Arc<dyn Source> = Arc::new(SpotifySource::new(
            auth.access_token.clone(),
            self.cache_dir.clone(),
            self.bus.clone(),
        ));
        let player: Arc<dyn Player> =
            Arc::new(SpotifyPlayer::new(auth, self.bus.clone(), self.volume, self.media_cache.clone()));
        Wiring { source: Some(source), player: Some(player), media: None }
    }

    fn setup(&self, _input: Option<String>) -> PluginHealth {
        match Auth::login(&self.cache_dir) {
            Ok(_) => PluginHealth::Ok,
            Err(e) => PluginHealth::Warn(format!("login failed: {e}")),
        }
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            word: "_spotify".to_string(),
            help: "addlogin [name] [client_id] — (re)authenticate and add a Web API credential \
                   pair alongside any already stored (never replaces one). name is just a label, \
                   default if omitted. client_id is \"medley\" or \"ncspot\" for those apps' own \
                   ids, any other string to use verbatim, or omitted for ncspot's (medley's own \
                   app 403s some endpoints, e.g. Like)"
                .to_string(),
        }]
    }

    fn run_command(&self, _word: &str, arg: Option<String>) -> String {
        let arg = arg.unwrap_or_default();
        let mut parts = arg.split_whitespace();
        match parts.next() {
            Some("addlogin") => match Auth::add_login(&self.cache_dir, parts.next(), parts.next()) {
                Ok(name) => format!("logged in — added Web API credential pair {name:?}"),
                Err(e) => format!("login failed: {e}"),
            },
            _ => "usage: _spotify addlogin [name] [client_id]".to_string(),
        }
    }
}
