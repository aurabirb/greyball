//! medley TUI entry point. Wires a real `Session` (HTTP source + rodio player +
//! redb store) to the cursive front-end and runs the event loop.

mod logging;
mod title;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use medley_core::{
    BrowseNode, Bus, BuiltinAction, Config, HotkeyTarget, LogBuf, MediaCache, MediaProvider, Player,
    PlaylistId, Plugin, ScanMode, ScanPlugin, Source, SourceId, Store, Uuid,
};
#[cfg(any(feature = "spotify", feature = "soundcloud", feature = "soulseek"))]
use medley_core::{PluginHealth, Wiring};
use player::RodioPlayer;
use sources_http::HttpDirSource;

use app::build_session;

fn config_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("medley");
    }
    home().join(".config").join("medley")
}

fn data_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(x).join("medley");
    }
    home().join(".local").join("share").join("medley")
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Load `config.toml`, tolerating a missing or malformed file. The persisted
/// volume lives in a separate `state.toml`.
fn load_config() -> Config {
    let path = config_dir().join("config.toml");
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            // Now that logging is up: warn, not just eprintln.
            eprintln!("medley: {}: {e}; using defaults", path.display());
            log::warn!("config: {} failed to parse: {e}; using defaults", path.display());
            Config::default()
        }),
        Err(_) => {
            let mut c = Config::default();
            // A sane default for a fresh install.
            c.http.roots = vec!["http://localhost:8000/".to_string()];
            c.http.recurse_depth = 2;
            c
        }
    };
    // Validate roots — a bad entry is dropped by the http source silently today.
    for root in &cfg.http.roots {
        if let Err(e) = url::Url::parse(root) {
            log::warn!("config: http root {root:?} is not a valid URL: {e}");
        }
    }
    if let Some(v) = load_volume() {
        cfg.volume = v;
    }
    cfg
}

fn state_path() -> PathBuf {
    data_dir().join("state.toml")
}

fn load_volume() -> Option<f32> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    let v: toml::Value = text.parse().ok()?;
    v.get("volume")?.as_float().map(|f| f as f32)
}

fn scan_mode_to_str(mode: ScanMode) -> &'static str {
    match mode {
        ScanMode::Active => "active",
        ScanMode::CacheOnly => "cache-only",
        ScanMode::Disabled => "disabled",
    }
}

fn scan_mode_from_str(s: &str) -> Option<ScanMode> {
    match s {
        "active" => Some(ScanMode::Active),
        "cache-only" => Some(ScanMode::CacheOnly),
        "disabled" => Some(ScanMode::Disabled),
        _ => None,
    }
}

/// Last-exit scan mode, persisted like `volume` — but only written (see
/// `save_state`) when it was an explicit override of the config default, so
/// a later default change isn't masked by a stale value.
fn load_scan_mode(default: ScanMode) -> ScanMode {
    (|| -> Option<ScanMode> {
        let text = std::fs::read_to_string(state_path()).ok()?;
        let v: toml::Value = text.parse().ok()?;
        scan_mode_from_str(v.get("scan_mode")?.as_str()?)
    })()
    .unwrap_or(default)
}

/// `HotkeyTarget` <-> the single string `state.toml` stores it as: a local
/// playlist is `"local:<uuid>"`; a remote one is `"remote:<source>:<node>"`,
/// where `<node>` is empty for `BrowseNode::Root` or the raw path id for
/// `BrowseNode::Path` (only the first two colons are significant — split
/// with `splitn`/`split_once` so a `:`-containing source-native id round-trips);
/// a remapped built-in command is `"builtin:<action-id>"` (see
/// `BuiltinAction::id`/`from_id`).
fn hotkey_target_to_string(t: &HotkeyTarget) -> String {
    match t {
        HotkeyTarget::Local(id) => format!("local:{}", id.0),
        HotkeyTarget::Remote(sid, node) => {
            let node = match node {
                BrowseNode::Root => String::new(),
                BrowseNode::Path(id) => id.clone(),
            };
            format!("remote:{}:{node}", sid.as_str())
        }
        HotkeyTarget::Builtin(action) => format!("builtin:{}", action.id()),
    }
}

fn hotkey_target_from_string(s: &str) -> Option<HotkeyTarget> {
    if let Some(rest) = s.strip_prefix("local:") {
        return Uuid::parse_str(rest).ok().map(|u| HotkeyTarget::Local(PlaylistId(u)));
    }
    if let Some(rest) = s.strip_prefix("builtin:") {
        return BuiltinAction::from_id(rest).map(HotkeyTarget::Builtin);
    }
    let rest = s.strip_prefix("remote:")?;
    let (sid, node) = rest.split_once(':')?;
    let node = if node.is_empty() { BrowseNode::Root } else { BrowseNode::Path(node.to_string()) };
    Some(HotkeyTarget::Remote(SourceId::from(sid), node))
}

/// Hotkeys (`` ` ``/`:keys` menu) — playlist bindings and built-in-command
/// remaps alike — persisted the same way as `volume`/`scan_paused`: one
/// `[hotkeys]` table of `key = target` lines (see
/// `hotkey_target_to_string`) at the end of `state.toml`, read back by
/// `Session::set_hotkeys`. An unremapped built-in never appears here — only
/// an explicit remap does (see `core::app::effective_target_at`).
fn load_hotkeys() -> HashMap<char, HotkeyTarget> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return map;
    };
    let Ok(v) = text.parse::<toml::Value>() else {
        return map;
    };
    let Some(table) = v.get("hotkeys").and_then(|t| t.as_table()) else {
        return map;
    };
    for (k, val) in table {
        let mut chars = k.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            continue;
        };
        let Some(target) = val.as_str().and_then(hotkey_target_from_string) else {
            continue;
        };
        map.insert(ch, target);
    }
    map
}

fn save_state(volume: f32, scan_mode: Option<ScanMode>, hotkeys: &HashMap<char, HotkeyTarget>) {
    let _ = std::fs::create_dir_all(data_dir());
    let mut text = format!("volume = {volume}\n");
    if let Some(scan_mode) = scan_mode {
        text.push_str(&format!("scan_mode = \"{}\"\n", scan_mode_to_str(scan_mode)));
    }
    if !hotkeys.is_empty() {
        text.push_str("\n[hotkeys]\n");
        for (k, target) in hotkeys {
            text.push_str(&format!("\"{k}\" = \"{}\"\n", hotkey_target_to_string(target)));
        }
    }
    let _ = std::fs::write(state_path(), text);
}

/// Install whatever `Wiring` a plugin currently offers into the startup
/// registries — used both here (at launch) and by `ui`'s warnings panel
/// (after a `setup()` completes, via `Session::apply_wiring`, which does the
/// same three inserts against the live session instead of these local maps).
#[cfg(any(feature = "spotify", feature = "soundcloud", feature = "soulseek"))]
fn install_wiring(
    w: Wiring,
    id: &SourceId,
    sources: &mut HashMap<SourceId, Arc<dyn Source>>,
    media: &mut HashMap<SourceId, Arc<dyn MediaProvider>>,
    players: &mut HashMap<SourceId, Arc<dyn Player>>,
) {
    if let Some(s) = w.source {
        sources.insert(id.clone(), s);
    }
    if let Some(m) = w.media {
        media.insert(id.clone(), m);
    }
    if let Some(p) = w.player {
        players.insert(id.clone(), p);
    }
}

/// Probe one optional source plugin and, unless it's outright `Fail`, install
/// whatever it can already offer (`Plugin::wiring`) — never blocks, never
/// opens a browser or prompts for anything (that's `Plugin::setup`, run only
/// later from an explicit user action in the warnings panel). Kept in
/// `plugins` regardless of health so a `Warn`/`Fail` message still shows
/// there.
#[cfg(any(feature = "spotify", feature = "soundcloud", feature = "soulseek"))]
fn register_plugin(
    plugin: Arc<dyn Plugin>,
    sources: &mut HashMap<SourceId, Arc<dyn Source>>,
    media: &mut HashMap<SourceId, Arc<dyn MediaProvider>>,
    players: &mut HashMap<SourceId, Arc<dyn Player>>,
    plugins: &mut Vec<Arc<dyn Plugin>>,
) {
    let id = plugin.id();
    match plugin.probe() {
        PluginHealth::Fail(msg) => log::warn!("{id}: {msg}; excluded"),
        PluginHealth::Ok => {
            log::info!("{id}: ready");
            install_wiring(plugin.wiring(), &id, sources, media, players);
        }
        PluginHealth::Warn(msg) => {
            log::info!("{id}: {msg}");
            install_wiring(plugin.wiring(), &id, sources, media, players);
        }
    }
    plugins.push(plugin);
}

/// One line, at both `info` (so it's always visible without `--log-level
/// debug`) and stderr (so it's visible before the log path note scrolls off).
/// A source silently missing here is nearly always one of: the cargo feature
/// wasn't built in, `enabled = false` in config, or (spotify) login failed.
fn log_registered_sources(sources: &HashMap<SourceId, Arc<dyn Source>>) {
    let mut ids: Vec<&str> = sources.keys().map(SourceId::as_str).collect();
    ids.sort_unstable();
    let list = if ids.is_empty() { "(none)".to_string() } else { ids.join(", ") };
    log::info!("sources registered: {list}");
    for (name, feature) in [
        ("spotify", cfg!(feature = "spotify")),
        ("soundcloud", cfg!(feature = "soundcloud")),
        ("soulseek", cfg!(feature = "soulseek")),
    ] {
        if !feature {
            log::info!(
                "sources: built without the `{name}` cargo feature — its config's \
                 `enabled` has no effect (rebuild with --features {name})"
            );
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let log = match logging::init() {
        Some((path, buf)) => {
            eprintln!("medley: logging to {}", path.display());
            buf
        }
        // Logging didn't come up (e.g. can't open the log file) — the Log
        // pane just stays empty; nothing else depends on it.
        None => Arc::new(LogBuf::default()),
    };
    match run(log) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::error!("fatal: {e}");
            Err(e)
        }
    }
}

fn run(log_buf: Arc<LogBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config();
    let initial_screen = cfg.initial_screen.clone();
    let theme = cfg.theme.clone();

    let mut siv = ui::create_cursive()?;
    let cb_sink = siv.cb_sink().clone();
    let bus = Bus::with_sink(move || {
        cb_sink.send(Box::new(|_| {})).ok();
    });

    // HTTP source doubles as its own MediaProvider. `local` (imported files)
    // always gets a player below regardless of this flag.
    let mut sources: HashMap<SourceId, Arc<dyn Source>> = HashMap::new();
    let mut media: HashMap<SourceId, Arc<dyn MediaProvider>> = HashMap::new();
    // Only pushed to when the spotify/soundcloud/soulseek cargo features are built.
    #[allow(unused_mut)]
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    let http_enabled = cfg.http.enabled;
    if http_enabled {
        let http = Arc::new(HttpDirSource::from_cfg(&cfg));
        sources.insert(SourceId::from("http"), http.clone());
        media.insert(SourceId::from("http"), http);
    }

    #[cfg(feature = "soundcloud")]
    if cfg.soundcloud.enabled {
        let token_cache_dir = data_dir().join("soundcloud");
        let plugin = Arc::new(sources_soundcloud::SoundcloudPlugin::new(
            cfg.soundcloud.client_id.clone(),
            cfg.soundcloud.oauth_token.clone(),
            token_cache_dir,
            bus.clone(),
        ));
        // SoundCloud never populates `Wiring::player` (its playback goes
        // through the shared `rodio` below, not yet built at this point in
        // startup) — a throwaway map here loses nothing.
        register_plugin(plugin, &mut sources, &mut media, &mut HashMap::new(), &mut plugins);
    }

    #[cfg(feature = "soulseek")]
    if cfg.soulseek.enabled {
        let conn = sources_soulseek::SlskdConfig {
            base_url: format!("http://{}:{}", cfg.soulseek.host, cfg.soulseek.port),
            username: cfg.soulseek.username.clone(),
            password: cfg.soulseek.password.clone(),
            api_key: cfg.soulseek.api_key.clone(),
        };
        let cache_dir = data_dir().join("soulseek");
        let plugin = Arc::new(sources_soulseek::SoulseekPlugin::new(
            conn,
            cfg.soulseek.data_dir.clone(),
            cache_dir,
            bus.clone(),
        ));
        // Soulseek never populates `Wiring::player` — playback goes through
        // the shared `rodio` below (once the download lands on disk it's a
        // plain local file, same as an http/soundcloud track).
        register_plugin(plugin, &mut sources, &mut media, &mut HashMap::new(), &mut plugins);
    }

    // Shared between playback and scanning: every source's decoded audio
    // lands here, keyed by rendition — whichever `ScanPlugin` first decodes
    // a rendition saves the rest (and playback itself) from re-fetching/
    // re-decrypting/re-decoding it.
    let media_cache = Arc::new(MediaCache::new(data_dir().join("media-cache")));
    let rodio = Arc::new(RodioPlayer::new(media.clone(), bus.clone(), media_cache.clone()));
    let mut players: HashMap<SourceId, Arc<dyn Player>> = HashMap::new();
    if http_enabled {
        players.insert(SourceId::from("http"), rodio.clone());
    }
    // Imported local files carry `source = "local"`; route them to the same
    // RodioPlayer so `Session::play_track` finds it.
    players.insert(SourceId::from("local"), rodio.clone());
    if media.contains_key(&SourceId::from("soundcloud")) {
        // SoundCloud's MediaProvider yields a plain CDN URL; RodioPlayer
        // downloads it exactly like an http track.
        players.insert(SourceId::from("soundcloud"), rodio.clone());
    }
    if media.contains_key(&SourceId::from("soulseek")) {
        // Soulseek's MediaProvider blocks until its slskd download lands on
        // disk, then yields a plain local path — RodioPlayer just reads it.
        players.insert(SourceId::from("soulseek"), rodio.clone());
    }

    #[cfg(feature = "spotify")]
    if cfg.spotify.enabled {
        let cache_dir = cfg
            .spotify
            .cache_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir().join("spotify"));
        let plugin = Arc::new(sources_spotify::SpotifyPlugin::new(
            cache_dir,
            bus.clone(),
            cfg.volume,
            media_cache.clone(),
        ));
        register_plugin(plugin, &mut sources, &mut media, &mut players, &mut plugins);
    }

    // BpmPlugin is registered into the running driver further down instead
    // (see `register_plugin` below), not passed in here.
    let scan_plugins: Vec<Arc<dyn ScanPlugin>> = Vec::new();
    let bpm_default_mode = if cfg.scan.bpm.enabled { ScanMode::CacheOnly } else { ScanMode::Disabled };
    let bpm_min_interval_secs = cfg.scan.bpm.min_interval_secs;

    log_registered_sources(&sources);

    let db_dir = data_dir();
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        log::warn!("config: cannot create data dir {}: {e}", db_dir.display());
    }
    let store: Arc<dyn Store> = Arc::new(medley_core::RedbStore::open(db_dir.join("db"))?);

    let mut session = build_session(
        cfg,
        bus.clone(),
        store,
        sources,
        media,
        players,
        plugins,
        scan_plugins,
        media_cache,
        data_dir().join("history.m3u8"),
    );
    if let Some(scan) = &session.scan {
        scan.set_mode(load_scan_mode(bpm_default_mode));
        // Runtime mode is `B`/`:togglescan`; `cfg.scan.bpm.enabled` above
        // only seeds the initial mode.
        scan.register_plugin(Arc::new(bpm::BpmPlugin::new(bpm_min_interval_secs)));
    }
    session.set_hotkeys(load_hotkeys());
    let session = Arc::new(Mutex::new(session));

    siv.set_theme(ui::theme::load(&theme));
    siv.set_user_data(session.clone());
    siv.add_fullscreen_layer(ui::root_view(session.clone(), &initial_screen, log_buf));

    // Hardware media keys, lock-screen/notification widgets, etc. — Linux/BSD
    // only (D-Bus). Fails soft internally if no session bus is reachable.
    #[cfg(target_os = "linux")]
    let mpris = app::mpris::MprisManager::spawn(session.clone());

    // macOS equivalent: MPRemoteCommandCenter/MPNowPlayingInfoCenter. These
    // are synchronous, main-thread-affine Cocoa APIs, so registration is a
    // plain call here rather than a spawned worker. UNTESTED, see
    // app/src/media_keys_macos.rs module docs.
    #[cfg(target_os = "macos")]
    let media_keys = app::media_keys_macos::MediaKeysManager::register(session.clone());

    let mut window_title = title::WindowTitle::new();

    // Cursive only flushes queued backend calls (e.g. `set_window_title`,
    // pushed below on every loop iteration) from inside its own `step()` —
    // never from our own `siv.refresh()` call further down. Without an fps
    // set, that flush path is idle-only and gated on an fps-derived cadence,
    // so it never runs while the Vis pane is closed and never runs at all
    // while events keep arriving (e.g. `Progress` ticks during playback) —
    // see `ui::BASELINE_FPS`. Sets the floor `view.rs`'s own Vis-pane fps
    // toggle preserves.
    siv.set_fps(ui::BASELINE_FPS);

    siv.refresh();
    while siv.is_running() {
        siv.step();
        // Re-arm wake coalescing for this iteration, then drain. Drain first,
        // then lock: never hold the Session mutex across a cursive call, and
        // keep the lock span here as short as the events themselves.
        bus.clear_wake();
        let events = bus.drain();
        // A raw-terminal write from outside cursive (e.g. spotify's OAuth
        // dependency printing "Browse to: ..." as a fallback) can leave
        // stray text cursive's diffed redraw won't know to repaint over —
        // rebuild the backend below to force a genuinely clean frame.
        let force_full_redraw =
            events.iter().any(|ev| matches!(ev, medley_core::CoreEvent::PluginLoginSucceeded));
        let mut dirty = false;
        let mut plugin_command_result = None;
        if !events.is_empty() {
            let mut s = session.lock().unwrap();
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let prev_track = s.now_playing_id();
            for ev in &events {
                dirty |= s.on_event(ev).unwrap_or(false);
            }
            // Read here (still under the lock) — `s` drops just below, and a
            // plugin command's result (e.g. `:_spotify addlogin`) is shown
            // as a modal via `siv` directly, once per result.
            if events.iter().any(|ev| matches!(ev, medley_core::CoreEvent::PluginCommandResult)) {
                plugin_command_result = s.take_plugin_command_result();
            }
            // For the terminal window title's marquee — read here, applied
            // to `siv` below once `s` is released.
            let now_playing_track = s.now_playing();
            let player_state = s.player_status().state;
            // Only signal on state transitions / track changes, never on
            // every `Progress` tick — MPRIS clients poll `Position` on
            // demand instead, and macOS's Now Playing info is likewise only
            // worth republishing on an actual change.
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let playback_state_changed = events.iter().any(|ev| {
                matches!(
                    ev,
                    medley_core::CoreEvent::Player(
                        medley_core::PlayerEvent::Playing { .. }
                            | medley_core::PlayerEvent::Paused
                            | medley_core::PlayerEvent::Stopped
                    )
                )
            });
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let track_changed = s.now_playing_id() != prev_track;
            // `update_now_playing` below re-locks `session` itself, so the
            // lock held here must be released first or it deadlocks.
            drop(s);

            window_title.update(&mut siv, now_playing_track.as_ref(), &player_state);

            #[cfg(target_os = "linux")]
            {
                if playback_state_changed {
                    mpris.notify_playback_status();
                }
                if track_changed {
                    mpris.notify_metadata();
                }
            }
            #[cfg(target_os = "macos")]
            {
                if playback_state_changed || track_changed {
                    log::debug!(
                        "media_keys_macos: triggering update (state_changed={playback_state_changed} track_changed={track_changed}) from events={events:?}"
                    );
                    media_keys.update_now_playing();
                }
            }
        }
        if force_full_redraw {
            // `siv.clear()` only re-blanks cursive's diffed draw buffer, which
            // still skips cells the model already considers unchanged — a
            // stray raw write can survive it. Dropping the backend and
            // building a fresh one forces a genuinely new `PrintBuffer`
            // (Cursive itself, and everything set on it, is unaffected).
            let inner = siv.into_inner();
            siv = cursive::CursiveRunner::new(inner, cursive::backends::try_default()?);
        }
        if let Some(msg) = plugin_command_result {
            siv.add_layer(cursive::views::Dialog::info(msg));
            dirty = true;
        }
        if dirty {
            siv.refresh();
        }
    }

    let s = session.lock().unwrap();
    // Only persisted when it's an explicit override of the config default —
    // otherwise a later default change would be masked by today's value.
    let default_mode = if s.cfg.scan.bpm.enabled { ScanMode::CacheOnly } else { ScanMode::Disabled };
    let scan_mode = s.scan.as_ref().map(|d| d.mode()).filter(|&mode| mode != default_mode);
    save_state(s.player_status().volume, scan_mode, &s.hotkeys().into_iter().collect());
    s.save_queue();
    drop(s);
    Ok(())
}
