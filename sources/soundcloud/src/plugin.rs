//! `core::Plugin` impl — non-blocking startup health + deferred token setup.
//! See `core::plugin` for why this exists (replacing a startup-blocking
//! login) and `auth.rs` for why token entry itself moved entirely into the
//! caller (the UI's setup dialog), not this crate.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::{Bus, MediaProvider, MutexExt, Outcome, Plugin, PluginHealth, ScanPlugin, SetupLog, SetupPrompt, Source, SourceId, StreamHandle, Track, Wiring, waveform};

use crate::auth;
use crate::client::{SoundcloudSource, WAVEFORM_URL_ATTR, source_id};

pub struct SoundcloudPlugin {
    client_id: Option<String>,
    cache_dir: PathBuf,
    bus: Bus,
    /// The token `wiring()` builds a `SoundcloudSource` with — `None` until
    /// a config value, a cached file, or a `setup()` call provides one.
    token: Mutex<Option<String>>,
    /// Why the configured `oauth_token` or the cached token file was unusable.
    config_error: Option<String>,
    hls: bool,
    /// The source `wiring()` last built, shared with the waveform scan plugin.
    source: Mutex<Option<Arc<SoundcloudSource>>>,
}

impl SoundcloudPlugin {
    pub fn new(
        client_id: Option<String>,
        configured_token: Option<String>,
        cache_dir: PathBuf,
        bus: Bus,
        hls: bool,
    ) -> Self {
        let configured = configured_token.filter(|s| !s.trim().is_empty()).map(|s| auth::extract_token(&s));
        let mut config_error = configured.as_ref().and_then(|r| r.as_ref().err()).map(|e| format!("[soundcloud] oauth_token is unusable: {e}"));
        let token = match configured.and_then(Result::ok) {
            Some(t) => Some(t),
            None => auth::load_cached(&cache_dir).unwrap_or_else(|e| {
                config_error.get_or_insert(e);
                None
            }),
        };
        if let Some(e) = &config_error {
            log::warn!("soundcloud: {e}");
        }
        Self { client_id, cache_dir, bus, token: Mutex::new(token), config_error, hls, source: Mutex::default() }
    }

    /// The scan plugin that reads SoundCloud's ready-made waveforms; register it before the decoding one.
    pub fn waveform_scan_plugin(self: &Arc<Self>) -> Arc<dyn ScanPlugin> {
        Arc::new(WaveformPlugin(self.clone()))
    }
}

impl Plugin for SoundcloudPlugin {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn probe(&self) -> PluginHealth {
        if self.token.locked().is_some() {
            if self.source.locked().as_ref().is_some_and(|s| s.login_expired()) {
                return PluginHealth::Warn("SoundCloud login expired — set up again".to_string());
            }
            PluginHealth::Ok
        } else {
            if let Some(e) = &self.config_error {
                return PluginHealth::Warn(e.clone());
            }
            PluginHealth::Warn(
                "not logged in — Liked Tracks and your playlists are unavailable until you \
                 add an OAuth token (search and playback still work)"
                    .to_string(),
            )
        }
    }

    fn detail(&self) -> Option<String> {
        self.config_error.clone()
    }

    fn setup_prompt(&self, answers: &[String]) -> Option<SetupPrompt> {
        answers.is_empty().then(|| {
            SetupPrompt::new(
                "Log in on soundcloud.com in your browser, then either:\n\
                 A. Press F12, open Console, type document.cookie, press Enter and copy the printed value.\n\
                 B. Press F12, open Network, click any request to api-v2.soundcloud.com (most look like me?client_id=...) and copy the value \
                 of its Authorization request header (it starts with OAuth ).\n\
                 Paste it here as a single line; it is hidden as you type. A token SoundCloud rejects is \
                 not saved and your previous login is kept.",
            )
            .secret()
        })
    }

    fn setup_answer(&self, _answers: &[String], answer: String) -> Result<String, String> {
        auth::extract_token(&answer)
    }

    fn wiring(&self) -> Wiring {
        // A fresh `SoundcloudSource` every call rather than one built once
        // and mutated — it has no interior mutability for its token, and
        // this is already only called at startup and right after `setup()`,
        // never per-frame.
        let token = self.token.locked().clone();
        let sc = Arc::new(SoundcloudSource::new(self.client_id.clone(), token, self.bus.clone(), self.hls));
        *self.source.locked() = Some(sc.clone());
        Wiring {
            source: Some(sc.clone() as Arc<dyn Source>),
            media: Some(sc as Arc<dyn MediaProvider>),
            player: None,
        }
    }

    fn setup(&self, answers: Vec<String>, log: &SetupLog) -> PluginHealth {
        let Some(token) = answers.into_iter().next().filter(|s| !s.is_empty()) else {
            return PluginHealth::Warn("no token entered".to_string());
        };
        if log.cancelled() {
            return PluginHealth::Warn("cancelled".to_string());
        }
        log.say("Checking the token with SoundCloud…");
        let source = SoundcloudSource::new(self.client_id.clone(), Some(token.clone()), self.bus.clone(), self.hls);
        let checked = source.me();
        if log.cancelled() {
            return PluginHealth::Warn("cancelled".to_string());
        }
        match checked {
            Ok(Some(name)) => {
                log.say(format!("Logged in as {name}"));
                if let Err(e) = auth::persist(&self.cache_dir, &token) {
                    log.say(format!("Could not save the token, you will need to log in again next launch: {e}"));
                }
                *self.token.locked() = Some(token);
                PluginHealth::Ok
            }
            Ok(None) => PluginHealth::Warn(
                "SoundCloud rejected that token — it may have expired; log in again and copy it fresh".to_string(),
            ),
            Err(e) => PluginHealth::Warn(format!("could not check the token: {e}")),
        }
    }
}

/// Fills the waveform attr from SoundCloud's own drawn waveform, so the decode plugin never runs for its tracks.
struct WaveformPlugin(Arc<SoundcloudPlugin>);

fn soundcloud_uri(track: &Track) -> Option<&str> {
    track.renditions.iter().find(|r| r.source == source_id()).map(|r| r.uri.as_str())
}

impl ScanPlugin for WaveformPlugin {
    fn id(&self) -> &'static str {
        "soundcloud-waveform"
    }

    fn name(&self) -> &'static str {
        "SoundCloud waveform"
    }

    fn needs(&self, track: &Track) -> bool {
        !track.attrs.contains_key(waveform::ATTR) && soundcloud_uri(track).is_some()
    }

    fn analyze(&self, track: &Track, _audio: &dyn Fn() -> Result<StreamHandle, Outcome>, _wanted: &dyn Fn() -> bool) -> Outcome {
        let source = self.0.source.locked().clone();
        let (Some(source), Some(url)) = (source, track.attrs.get(WAVEFORM_URL_ATTR)) else {
            return Outcome::Skip;
        };
        match source.waveform_samples(url) {
            Ok(Some(samples)) => waveform::envelope(&samples).map_or(Outcome::Skip, |b| Outcome::Done(waveform::meta(&b))),
            Ok(None) => Outcome::Skip,
            Err(e) => {
                log::warn!("soundcloud-waveform: {e}");
                Outcome::Retry
            }
        }
    }

    fn min_interval(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn needs_audio(&self) -> bool {
        false
    }
}
