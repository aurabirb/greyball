//! `core::Plugin` impl — non-blocking startup health + deferred OAuth login.
//! See `core::plugin` for why this exists: this used to be an OAuth browser
//! flow (`Auth::login`) run synchronously during `app` startup, blocking
//! the whole TUI behind it whenever there were no cached credentials yet.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{Bus, CoreEvent, Plugin, PluginCommand, PluginHealth, SetupLog, SetupPrompt, SourceId, Wiring};

use crate::auth::Auth;
use crate::provider::SpotifyMediaProvider;
use crate::source::SpotifySource;

/// Cooldown between automatic background refresh attempts kicked off from
/// `probe` (called by the warnings modal and by `app`'s periodic health
/// timer) — keeps a permanently-failing refresh (e.g. a revoked grant) from
/// hammering Spotify's token endpoint.
const AUTO_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

pub struct SpotifyPlugin {
    cache_dir: PathBuf,
    bus: Bus,
    /// Guards against `probe` spawning overlapping automatic refresh
    /// attempts (it can be called concurrently by the warnings modal and by
    /// `app`'s periodic health timer).
    refreshing: Arc<AtomicBool>,
    next_auto_refresh: Arc<Mutex<Instant>>,
    /// Reused across rewires: a second `SpotifyMediaProvider` is a second session login on the same account.
    wired: Mutex<Option<(Arc<SpotifySource>, Arc<SpotifyMediaProvider>)>>,
}

impl SpotifyPlugin {
    pub fn new(cache_dir: PathBuf, bus: Bus) -> Self {
        crate::auth::heal_permissions(&cache_dir);
        Self {
            cache_dir,
            bus,
            refreshing: Arc::new(AtomicBool::new(false)),
            next_auto_refresh: Arc::new(Mutex::new(Instant::now())),
            wired: Mutex::new(None),
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
            } else {
                bus.send(CoreEvent::BackgroundFailure {
                    context: "spotify".to_string(),
                    message: "session refresh failed — see :log".to_string(),
                });
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

    fn wiring(&self) -> Wiring {
        // Unlike SoundCloud, Spotify has no anonymous mode at all — no
        // token means nothing to build. `Warn` health here means "not
        // registered yet", not "registered but degraded".
        let Some(auth) = Auth::load_cached(&self.cache_dir) else {
            return Wiring::default();
        };
        let mut wired = self.wired.lock().unwrap_or_else(|e| e.into_inner());
        let (source, media) = match wired.as_ref() {
            Some((source, media)) => {
                source.set_token(auth.access_token);
                (source.clone(), media.clone())
            }
            None => {
                let source = Arc::new(SpotifySource::new(
                    auth.access_token.clone(),
                    self.cache_dir.clone(),
                    self.bus.clone(),
                ));
                let media = Arc::new(SpotifyMediaProvider::new(auth));
                wired.insert((source, media)).clone()
            }
        };
        Wiring { source: Some(source), player: None, media: Some(media) }
    }

    fn setup_prompt(&self, answers: &[String]) -> Option<SetupPrompt> {
        answers.is_empty().then(|| SetupPrompt::new("This opens your browser to log in to Spotify. Press Enter to continue, or Esc to cancel."))
    }

    fn setup(&self, _answers: Vec<String>, log: &SetupLog) -> PluginHealth {
        let had_credentials = Auth::cache(&self.cache_dir).is_ok_and(|c| c.credentials().is_some());
        match Auth::login(&self.cache_dir, log) {
            Ok(_) => {
                log.say("Logged in to Spotify.");
                // A fresh music login may be a different account; the cached provider is bound to the old one.
                if !had_credentials {
                    *self.wired.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                PluginHealth::Ok
            }
            Err(e) => PluginHealth::Warn(format!("login failed: {e}")),
        }
    }

    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            word: "spotify".to_string(),
            help: "addlogin [name] [client_id] — (re)authenticate and add a Web API credential \
                   pair alongside any already stored (never replaces one). name is just a label, \
                   default if omitted. client_id is \"ncspot\" for ncspot's id, any other string \
                   to use verbatim, or omitted to pick an embedded id not already logged in"
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
            _ => "usage: spotify addlogin [name] [client_id]".to_string(),
        }
    }
}
