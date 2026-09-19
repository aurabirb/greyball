//! Plain configuration struct. NO file IO — `app` reads/writes the TOML and
//! passes a `Config` value in.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub http: HttpConfig,
    /// Spotify source plugin. Only consulted when `app` is built with the
    /// `spotify` cargo feature; otherwise inert.
    pub spotify: SpotifyConfig,
    /// SoundCloud source plugin. Only consulted when `app` is built with the
    /// `soundcloud` cargo feature; otherwise inert.
    pub soundcloud: SoundcloudConfig,
    /// Soulseek source plugin (via a local `slskd`). Only consulted when
    /// `app` is built with the `soulseek` cargo feature; otherwise inert.
    pub soulseek: SoulseekConfig,
    /// Background scan plugins (bpm, later genre/mood/...).
    pub scan: ScanConfig,
    /// Which `Track::attrs` keys show in list rows, in order, when present.
    /// Default: just the one canonical MVP attr — any other attr (a future
    /// `genre`, or `key` once something populates it) stays hidden until
    /// the user opts in here.
    pub visible_track_attrs: Vec<String>,
    /// Default placement preference for optional panes (Log, Settings).
    /// `ui` seeds its runtime state from this; `:panes` changes it live but
    /// (MVP) does not persist the change back to this file.
    pub panes: PaneLayoutConfig,
    pub vis: VisConfig,
    /// Whether the bottom scrubber row is shown.
    pub status_line: bool,
    /// Whether windows show key hints on their last row.
    pub show_hints: bool,
    /// Whether a newer release is downloaded in the background at startup.
    pub auto_update: bool,
    /// Media cache directory; changing it applies at the next launch.
    pub media_cache_dir: PathBuf,
    pub theme: String,
    /// Persisted player volume, 0.0..=1.0.
    pub volume: f32,
}

/// Sources the Settings pane / `state.toml` can toggle by name.
pub const TOGGLABLE_SOURCES: [&str; 4] = ["http", "spotify", "soundcloud", "soulseek"];

/// `~` and `~/x` expanded against `$HOME`.
pub fn expand_home(text: &str) -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    match text.strip_prefix('~') {
        Some("") => home,
        Some(rest) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(text),
    }
}

/// `path` with a leading `$HOME` shown as `~`.
pub fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

impl Config {
    /// `enabled` bit for one of `TOGGLABLE_SOURCES`, `None` for any other name.
    pub fn source_enabled(&self, source: &str) -> Option<bool> {
        match source {
            "http" => Some(self.http.enabled),
            "spotify" => Some(self.spotify.enabled),
            "soundcloud" => Some(self.soundcloud.enabled),
            "soulseek" => Some(self.soulseek.enabled),
            _ => None,
        }
    }

    /// Sets one of `TOGGLABLE_SOURCES`' `enabled` bit; no-op for any other name.
    pub fn set_source_enabled(&mut self, source: &str, enabled: bool) {
        match source {
            "http" => self.http.enabled = enabled,
            "spotify" => self.spotify.enabled = enabled,
            "soundcloud" => self.soundcloud.enabled = enabled,
            "soulseek" => self.soulseek.enabled = enabled,
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VisConfig {
    pub fps: u32,
}

impl VisConfig {
    pub const MIN_FPS: u32 = 5;
    pub const MAX_FPS: u32 = 60;

    pub fn limit(&self) -> u32 {
        self.fps.clamp(Self::MIN_FPS, Self::MAX_FPS)
    }
}

impl Default for VisConfig {
    fn default() -> Self {
        Self { fps: 30 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    /// Register the http source at startup. On by default; with no `roots`
    /// configured it simply finds nothing (`search` reports "no roots
    /// configured" as a `BackgroundFailure` — not fatal).
    pub enabled: bool,
    pub roots: Vec<String>,
    pub recurse_depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotifyConfig {
    /// Register the Spotify source + player at startup. On by default —
    /// **with no cached credentials yet, this blocks startup on an OAuth
    /// browser login** (`Auth::login`). Set `enabled = false` to opt out
    /// entirely.
    pub enabled: bool,
    /// Override for the librespot cache / credentials dir. Defaults to
    /// `<data_dir>/spotify/`.
    pub cache_dir: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundcloudConfig {
    /// Register the SoundCloud source at startup. On by default; no login,
    /// no network at startup (the `client_id` scrape is lazy, on first use).
    pub enabled: bool,
    /// API `client_id`. If unset, medley scrapes one from the public web
    /// player on first use (best-effort).
    pub client_id: Option<String>,
    /// A user OAuth token (`Authorization: OAuth <token>`), needed for
    /// anything scoped to a logged-in user — "Liked Tracks" and the user's
    /// own playlists. There's no browser login flow for this (SoundCloud
    /// isn't granting new API app registrations); paste a token obtained
    /// out-of-band (e.g. from the web player's own requests). Unset: the
    /// SoundCloud source works for search/resolve/play only, and its browse
    /// root has no folders.
    pub oauth_token: Option<String>,
    /// Prefer a higher-bitrate HLS (AAC 160kbps) stream over the 128kbps
    /// MP3 progressive stream when the track offers one. On by default —
    /// this is a verified-working quality improvement, not experimental.
    pub hls: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SoulseekConfig {
    /// Register the Soulseek source at startup. On by default; with no
    /// local `slskd` reachable it just shows a warning (see
    /// `sources_soulseek::SoulseekPlugin::probe`), never fatal. Set `false`
    /// to silence that warning entirely if you don't run one.
    pub enabled: bool,
    /// slskd's HTTP API host. Default matches slskd's own default bind.
    pub host: String,
    /// slskd's HTTP API port. Default matches slskd's own default (5030).
    pub port: u16,
    /// slskd login username. Default matches slskd's own out-of-the-box
    /// default credentials — override if the local instance changed them.
    pub username: String,
    /// slskd login password. Same default-credentials note as `username`.
    pub password: String,
    /// An slskd API key (`X-API-Key`), if configured on the daemon — takes
    /// precedence over `username`/`password` and skips the login call
    /// entirely. Unset by default.
    pub api_key: Option<String>,
    /// The directory slskd itself reads/writes (contains `downloads/`,
    /// `incomplete/`, `slskd.yml`) — needed to locate a finished download on
    /// disk for playback (search works without it). Normally set once via
    /// the plugin's setup prompt (see `SetupKind::TextInput`), which caches
    /// it outside this file; set here to skip that prompt.
    pub data_dir: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// After supplying whatever a plugin reads, keep draining the rest of
    /// the track so the backend commits the whole file to its local cache
    /// (for Spotify: librespot's on-disk cache). Applies to every plugin's
    /// background-walk fetches, not just bpm's. See `Player::open_for_scan`.
    pub cache_full: bool,
    pub bpm: BpmScanConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BpmScanConfig {
    /// Whether the scan driver starts in `CacheOnly` (`true`) or fully
    /// `Disabled` (`false`). The plugin itself is always registered (so `B`
    /// only ever moves between `Active`/`CacheOnly`, never in or out of
    /// `Disabled`) — this only sets the initial state, and only on the very
    /// first run; after that the last `B`-toggled state (persisted in
    /// `state.toml`) wins. On by default: `CacheOnly` never originates a
    /// fetch, only reads audio something else already materialized, so
    /// there's no network cost to leaving it on.
    pub enabled: bool,
    /// Minimum spacing between this plugin's own background fetches.
    pub min_interval_secs: u64,
}

/// Where an open optional pane (Log, Settings) renders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneLayoutConfig {
    pub mode: PaneMode,
    /// Which side embedded panes attach to. `Left`/`Right` make the pane
    /// column a narrow strip alongside the main content; `Top`/`Bottom` make
    /// it a full-width bar above/below the main content instead. Ignored in
    /// `Screen` mode.
    pub side: Side,
    /// How multiple simultaneously-open embedded panes share that space:
    /// `Horizontal` lays them out side by side (split along x), `Vertical`
    /// stacks them (split along y) — independent of `side`, so e.g.
    /// `side: Top, stack: Horizontal` is a full-width bar with panes side by
    /// side in it. Ignored in `Screen` mode or with at most one pane open.
    pub stack: Axis,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneMode {
    /// Pushed as a full modal layer, like the `:open` file browser.
    Screen,
    /// Drawn in a slice of the main screen, alongside the primary content.
    #[default]
    Embedded,
    /// A bordered box over the current view.
    Float,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    #[default]
    Right,
    /// Pane column becomes a full-width bar above the main content.
    Top,
    /// Pane column becomes a full-width bar below the main content.
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    #[default]
    Vertical,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http: HttpConfig::default(),
            spotify: SpotifyConfig::default(),
            soundcloud: SoundcloudConfig::default(),
            soulseek: SoulseekConfig::default(),
            scan: ScanConfig::default(),
            visible_track_attrs: vec!["bpm".to_string()],
            panes: PaneLayoutConfig::default(),
            vis: VisConfig::default(),
            status_line: true,
            show_hints: true,
            auto_update: true,
            media_cache_dir: PathBuf::new(),
            theme: "default".to_string(),
            volume: 1.0,
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            cache_full: true,
            bpm: BpmScanConfig::default(),
        }
    }
}

impl Default for BpmScanConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_interval_secs: 15,
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: vec![],
            recurse_depth: 2,
        }
    }
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_dir: None,
        }
    }
}

impl Default for SoundcloudConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            client_id: None,
            oauth_token: None,
            hls: true,
        }
    }
}

impl Default for SoulseekConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "localhost".to_string(),
            port: 5030,
            username: "slskd".to_string(),
            password: "slskd".to_string(),
            api_key: None,
            data_dir: None,
        }
    }
}

/// The window layout `state.toml` keeps: windows by their startup names, placements by their `:panes` words.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// The tabbed windows, in tab-bar order.
    pub tabs: Vec<String>,
    pub active: String,
    /// The open non-tab windows, oldest first.
    pub open: Vec<String>,
    /// Every non-tab window's placement.
    pub placements: BTreeMap<String, String>,
    pub side: Side,
    pub stack: Axis,
}
