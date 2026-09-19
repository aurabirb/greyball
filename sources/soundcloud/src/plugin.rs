//! `core::Plugin` impl — non-blocking startup health + deferred token setup.
//! See `core::plugin` for why this exists (replacing a startup-blocking
//! login) and `auth.rs` for why token entry itself moved entirely into the
//! caller (the UI's warnings panel), not this crate.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use core::{Bus, MediaProvider, Plugin, PluginHealth, Source, SourceId, Wiring};

use crate::auth;
use crate::client::SoundcloudSource;

pub struct SoundcloudPlugin {
    client_id: Option<String>,
    cache_dir: PathBuf,
    bus: Bus,
    /// The token `wiring()` builds a `SoundcloudSource` with — `None` until
    /// a config value, a cached file, or a `setup()` call provides one.
    token: Mutex<Option<String>>,
    hls: bool,
}

impl SoundcloudPlugin {
    pub fn new(
        client_id: Option<String>,
        configured_token: Option<String>,
        cache_dir: PathBuf,
        bus: Bus,
        hls: bool,
    ) -> Self {
        let token = configured_token
            .filter(|s| !s.trim().is_empty())
            .or_else(|| auth::load_cached(&cache_dir));
        Self { client_id, cache_dir, bus, token: Mutex::new(token), hls }
    }
}

impl Plugin for SoundcloudPlugin {
    fn id(&self) -> SourceId {
        SourceId::from("soundcloud")
    }

    fn probe(&self) -> PluginHealth {
        if self.token.lock().unwrap().is_some() {
            PluginHealth::Ok
        } else {
            PluginHealth::Warn(
                "not logged in — Liked Tracks and your playlists are unavailable until you \
                 add an OAuth token (search and playback still work)"
                    .to_string(),
            )
        }
    }

    fn setup_prompt(&self, answers: &[String]) -> Option<String> {
        answers.is_empty().then(|| {
            "SoundCloud OAuth token — devtools on soundcloud.com, Application tab, \
             Local Storage, the `oauth_token` key (or a request's Authorization header)"
                .to_string()
        })
    }

    fn wiring(&self) -> Wiring {
        // A fresh `SoundcloudSource` every call rather than one built once
        // and mutated — it has no interior mutability for its token, and
        // this is already only called at startup and right after `setup()`,
        // never per-frame.
        let token = self.token.lock().unwrap().clone();
        let sc = Arc::new(SoundcloudSource::new(self.client_id.clone(), token, self.bus.clone(), self.hls));
        Wiring {
            source: Some(sc.clone() as Arc<dyn Source>),
            media: Some(sc as Arc<dyn MediaProvider>),
            player: None,
        }
    }

    fn setup(&self, answers: Vec<String>) -> PluginHealth {
        let token = answers.first().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let Some(token) = token else {
            return PluginHealth::Warn("no token entered".to_string());
        };
        auth::persist(&self.cache_dir, &token);
        *self.token.lock().unwrap() = Some(token);
        PluginHealth::Ok
    }
}
