//! `core::Plugin` impl for Soulseek. Connectivity (does a local `slskd`
//! actually answer on the configured host:port?) is exactly the kind of
//! check `core::plugin`'s module doc says `probe()` must never do inline —
//! it's real network I/O and `probe()` is called both from the warnings
//! panel and from `app`'s periodic health timer. So it follows
//! `sources_spotify::SpotifyPlugin`'s
//! `kick_off_auto_refresh` pattern instead: `probe()` kicks off a cooldown-
//! guarded background check and reports the last result, `setup()` does one
//! for real inline (it's the one place a blocking network call is fine).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{Bus, CoreEvent, MediaProvider, Plugin, PluginHealth, SetupKind, Source, SourceId, Wiring};

use crate::client::{SlskdClient, SlskdConfig};
use crate::persist;
use crate::source::SoulseekSource;
use crate::yaml_config::{self, YamlAuth};

/// Cooldown between automatic background reachability checks kicked off
/// from `probe` (called by the warnings modal and by `app`'s periodic
/// health timer) — keeps a permanently-unreachable daemon from getting
/// hammered with connection attempts.
const CHECK_COOLDOWN: Duration = Duration::from_secs(30);

pub struct SoulseekPlugin {
    conn: SlskdConfig,
    cache_dir: PathBuf,
    bus: Bus,
    /// The slskd data directory — `None` until a config value or a cached
    /// `setup()` value provides one. Needed for playback only; search works
    /// without it.
    data_dir: Mutex<Option<String>>,
    /// Web-API credentials read straight out of that data directory's own
    /// `slskd.yml`, if it has an explicit `web.authentication` block —
    /// takes precedence over `conn`'s config.toml/default values so the
    /// daemon's actual config doesn't need to be duplicated by hand.
    yaml_auth: Mutex<Option<YamlAuth>>,
    /// Last background/`setup()` reachability result. `None` until the
    /// first check completes.
    reachable: Arc<Mutex<Option<bool>>>,
    checking: Arc<AtomicBool>,
    next_check: Arc<Mutex<Instant>>,
}

impl SoulseekPlugin {
    pub fn new(conn: SlskdConfig, configured_data_dir: Option<String>, cache_dir: PathBuf, bus: Bus) -> Self {
        let data_dir = configured_data_dir
            .filter(|s| !s.trim().is_empty())
            .or_else(|| persist::load_cached(&cache_dir));
        let yaml_auth = data_dir.as_deref().and_then(|d| yaml_config::read(&expand_home(d)));
        Self {
            conn,
            cache_dir,
            bus,
            data_dir: Mutex::new(data_dir),
            yaml_auth: Mutex::new(yaml_auth),
            reachable: Arc::new(Mutex::new(None)),
            checking: Arc::new(AtomicBool::new(false)),
            next_check: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// `conn` (from `config.toml`/defaults) with any override found in the
    /// data directory's own `slskd.yml` applied on top.
    fn effective_conn(&self) -> SlskdConfig {
        let mut conn = self.conn.clone();
        if let Some(auth) = self.yaml_auth.lock().unwrap().as_ref() {
            if let Some(key) = &auth.api_key {
                conn.api_key = Some(key.clone());
            } else {
                if let Some(u) = &auth.username {
                    conn.username = u.clone();
                }
                if let Some(p) = &auth.password {
                    conn.password = p.clone();
                }
            }
        }
        conn
    }

    fn kick_off_connectivity_check(&self) {
        if self.checking.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut next = self.next_check.lock().unwrap();
            let already_checked = self.reachable.lock().unwrap().is_some();
            if already_checked && Instant::now() < *next {
                self.checking.store(false, Ordering::SeqCst);
                return;
            }
            *next = Instant::now() + CHECK_COOLDOWN;
        }
        let conn = self.conn.clone();
        let reachable = self.reachable.clone();
        let checking = self.checking.clone();
        let bus = self.bus.clone();
        std::thread::spawn(move || {
            let ok = SlskdClient::new(conn).reachable();
            let mut r = reachable.lock().unwrap();
            let changed = *r != Some(ok);
            *r = Some(ok);
            drop(r);
            checking.store(false, Ordering::SeqCst);
            if changed {
                bus.send(CoreEvent::PluginStatusChanged);
            }
        });
    }

    fn unreachable_message(&self) -> String {
        format!(
            "no slskd reachable at {} — install and run it \
             (https://github.com/slskd/slskd/), or set [soulseek] enabled = false in config.toml \
             to silence this; check [soulseek] host/port if it's running on a different address",
            self.conn.base_url
        )
    }
}

impl Plugin for SoulseekPlugin {
    fn id(&self) -> SourceId {
        SourceId::from("soulseek")
    }

    fn probe(&self) -> PluginHealth {
        self.kick_off_connectivity_check();
        match *self.reachable.lock().unwrap() {
            None => PluginHealth::Warn("checking for a local slskd…".to_string()),
            Some(false) => PluginHealth::Warn(self.unreachable_message()),
            Some(true) if self.data_dir.lock().unwrap().is_none() => PluginHealth::Warn(
                "slskd found, but no data directory configured — select to set it up \
                 (search works either way; playback needs it to find finished downloads)"
                    .to_string(),
            ),
            Some(true) => PluginHealth::Ok,
        }
    }

    fn setup_kind(&self) -> SetupKind {
        SetupKind::TextInput {
            prompt: "slskd data directory — the folder slskd itself reads/writes (contains \
                     downloads/, incomplete/, slskd.yml); needed to find finished downloads on \
                     disk. Connection settings (host/port/username/password/api_key) come from \
                     [soulseek] in config.toml, not here."
                .to_string(),
        }
    }

    fn wiring(&self) -> Wiring {
        let client = SlskdClient::new(self.effective_conn());
        let data_dir = self.data_dir.lock().unwrap().clone().map(|d| expand_home(&d));
        let src = Arc::new(SoulseekSource::new(client, data_dir));
        Wiring {
            source: Some(src.clone() as Arc<dyn Source>),
            media: Some(src as Arc<dyn MediaProvider>),
            player: None,
        }
    }

    fn setup(&self, input: Option<String>) -> PluginHealth {
        let Some(dir) = input.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
            return PluginHealth::Warn("no directory entered".to_string());
        };
        let path = expand_home(&dir);
        if !path.is_dir() {
            return PluginHealth::Fail(format!("{} is not a directory", path.display()));
        }

        *self.yaml_auth.lock().unwrap() = yaml_config::read(&path);
        let client = SlskdClient::new(self.effective_conn());
        let ok = client.reachable();
        *self.reachable.lock().unwrap() = Some(ok);
        if !ok {
            // Same transient condition `probe` reports as `Warn` — a
            // moment-in-time connectivity check, not a permanent verdict on
            // this plugin, so it shouldn't outrank `probe`'s own `Warn` with
            // a harsher icon once the daemon comes up.
            return PluginHealth::Warn(self.unreachable_message());
        }

        persist::persist(&self.cache_dir, &dir);
        *self.data_dir.lock().unwrap() = Some(dir);
        PluginHealth::Ok
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}
