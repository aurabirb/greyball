//! medley TUI entry point. Wires a real `Session` (HTTP source + rodio player +
//! redb store) to the cursive front-end and runs the event loop.

mod logging;
mod title;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use medley_core::{
    BrowseNode, Bus, BuiltinAction, Config, HotkeyTarget, LastPlayed, Layout, LogBuf, MediaCache, Player, SharedMedia,
    PlaylistId, Plugin, ScanMode, ScanPlugin, Source, SourceId, Store, TOGGLABLE_SOURCES, Track, TrackId, Uuid,
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

/// `config.toml` (a malformed one falls back to defaults) and what was wrong with it, for the warnings list.
fn load_config() -> (Config, Vec<String>) {
    let path = config_dir().join("config.toml");
    let mut problems = Vec::new();
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            problems.push(format!("{} failed to parse: {e}; using defaults", path.display()));
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
            problems.push(format!("http root {root:?} is not a valid URL: {e}"));
        }
    }
    if let Some(v) = load_volume() {
        cfg.volume = v;
    }
    (cfg, problems)
}

fn state_path() -> PathBuf {
    data_dir().join("state.toml")
}

fn load_volume() -> Option<f32> {
    load_state_value("volume")?.as_float().map(|f| f as f32)
}

fn load_vis_fps() -> Option<u32> {
    u32::try_from(load_state_value("vis_fps")?.as_integer()?).ok()
}

fn load_status_line() -> Option<bool> {
    load_state_value("status_line")?.as_bool()
}

fn load_show_hints() -> Option<bool> {
    load_state_value("show_hints")?.as_bool()
}

fn load_auto_update() -> Option<bool> {
    load_state_value("auto_update")?.as_bool()
}

fn load_waveform_enabled() -> Option<bool> {
    load_state_value("waveform_enabled")?.as_bool()
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
    load_state_string("scan_mode").and_then(|s| scan_mode_from_str(&s)).unwrap_or(default)
}

/// Settings-pane source overrides, same shape/precedent as `load_scan_mode`.
fn load_source_overrides() -> HashMap<String, bool> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return map;
    };
    let Ok(v) = text.parse::<toml::Value>() else {
        return map;
    };
    let Some(table) = v.get("sources").and_then(|t| t.as_table()) else {
        return map;
    };
    for name in TOGGLABLE_SOURCES {
        if let Some(b) = table.get(name).and_then(|v| v.as_bool()) {
            map.insert(name.to_string(), b);
        }
    }
    map
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

/// Hotkeys — playlist bindings and built-in-command
/// remaps alike — persisted the same way as `volume`/`scan_mode`: one
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

/// The `[layout]` table; one that is absent or doesn't parse whole means the default layout.
fn load_layout() -> Option<Layout> {
    load_state_value("layout")?.try_into().ok()
}

/// The `[last_played]` table: the track id and, when it played from a playlist, that playlist's target string.
fn load_last_played() -> Option<LastPlayed> {
    let table = load_state_value("last_played")?;
    let track = TrackId(Uuid::parse_str(table.get("track")?.as_str()?).ok()?);
    let playlist = table.get("playlist").and_then(|p| p.as_str()).and_then(hotkey_target_from_string);
    Some(LastPlayed { track, playlist })
}

fn last_played_table(last: &LastPlayed) -> toml::Table {
    let mut table = toml::Table::new();
    table.insert("track".into(), last.track.0.to_string().into());
    if let Some(target) = &last.playlist {
        table.insert("playlist".into(), hotkey_target_to_string(target).into());
    }
    toml::Table::from_iter([("last_played".to_string(), toml::Value::Table(table))])
}

/// Rewrites just the `[last_played]` table, so a track change is saved without waiting for quit.
fn save_last_played(last: &LastPlayed) {
    let mut state = std::fs::read_to_string(state_path()).ok().and_then(|text| text.parse::<toml::Table>().ok()).unwrap_or_default();
    state.extend(last_played_table(last));
    if let Ok(text) = toml::to_string(&state) {
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(state_path(), text);
    }
}

fn load_state_value(key: &str) -> Option<toml::Value> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    text.parse::<toml::Table>().ok()?.remove(key)
}

fn load_state_string(key: &str) -> Option<String> {
    load_state_value(key)?.as_str().map(str::to_string)
}

fn load_state_path(key: &str) -> Option<PathBuf> {
    load_state_string(key).map(PathBuf::from)
}

#[allow(clippy::too_many_arguments)]
fn save_state(
    last_played: Option<&LastPlayed>,
    volume: f32,
    vis_fps: Option<u32>,
    status_line: Option<bool>,
    show_hints: Option<bool>,
    auto_update: Option<bool>,
    waveform_enabled: Option<bool>,
    liked_playlist: Option<&str>,
    media_cache_dir: Option<&std::path::Path>,
    media_cache_move_from: Option<&std::path::Path>,
    scan_mode: Option<ScanMode>,
    hotkeys: &HashMap<char, HotkeyTarget>,
    source_overrides: &HashMap<&'static str, bool>,
    layout: Option<Layout>,
) {
    let _ = std::fs::create_dir_all(data_dir());
    let mut text = format!("volume = {volume:?}\n");
    if let Some(fps) = vis_fps {
        text.push_str(&format!("vis_fps = {fps}\n"));
    }
    if let Some(shown) = status_line {
        text.push_str(&format!("status_line = {shown}\n"));
    }
    if let Some(shown) = show_hints {
        text.push_str(&format!("show_hints = {shown}\n"));
    }
    if let Some(on) = auto_update {
        text.push_str(&format!("auto_update = {on}\n"));
    }
    if let Some(on) = waveform_enabled {
        text.push_str(&format!("waveform_enabled = {on}\n"));
    }
    if let Some(name) = liked_playlist {
        text.push_str(&format!("liked_playlist = {}\n", toml::Value::String(name.to_string())));
    }
    if let Some(dir) = media_cache_dir {
        text.push_str(&format!("media_cache_dir = {}\n", toml::Value::String(dir.to_string_lossy().into_owned())));
    }
    if let Some(dir) = media_cache_move_from {
        text.push_str(&format!("media_cache_move_from = {}\n", toml::Value::String(dir.to_string_lossy().into_owned())));
    }
    if let Some(scan_mode) = scan_mode {
        text.push_str(&format!("scan_mode = \"{}\"\n", scan_mode_to_str(scan_mode)));
    }
    if !hotkeys.is_empty() {
        let table: toml::Table = hotkeys.iter().map(|(k, target)| (k.to_string(), hotkey_target_to_string(target).into())).collect();
        if let Ok(table) = toml::to_string(&toml::Table::from_iter([("hotkeys".to_string(), toml::Value::Table(table))])) {
            text.push_str(&format!("\n{table}"));
        }
    }
    if !source_overrides.is_empty() {
        text.push_str("\n[sources]\n");
        for (name, enabled) in source_overrides {
            text.push_str(&format!("{name} = {enabled}\n"));
        }
    }
    let layout = layout.and_then(|layout| toml::Value::try_from(layout).ok());
    if let Some(table) = layout.and_then(|layout| toml::to_string(&toml::Table::from_iter([("layout".to_string(), layout)])).ok()) {
        text.push_str(&format!("\n{table}"));
    }
    if let Some(table) = last_played.and_then(|last| toml::to_string(&last_played_table(last)).ok()) {
        text.push_str(&format!("\n{table}"));
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
    media: &SharedMedia,
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
    media: &SharedMedia,
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

/// Background timer, independent of the UI: periodically re-probes every
/// plugin so a token expiring or an slskd instance going down/coming back is
/// noticed even if nobody opens the warnings modal — previously `probe()`
/// only ran from there, or from a plugin's own cooldown-gated background
/// check, so once the cache said "all Ok" nothing ever looked again. Plugins
/// are cloned and probed with the session lock released (`probe()` can do
/// disk/network I/O), then the results applied back under a short lock;
/// `Session::apply_probed_plugin_health` only stores them (and this only
/// sends the event) when something actually changed, so an unchanged tick
/// causes no rewire/redraw. Runs for the life of the process, like the
/// scan-driver thread: nothing joins it, so it simply stops the moment
/// `main` returns.
fn spawn_plugin_health_timer(session: Arc<Mutex<medley_core::Session>>, bus: Bus) {
    std::thread::Builder::new()
        .name("plugin-health".into())
        .spawn(move || {
            loop {
                std::thread::sleep(medley_core::PLUGIN_HEALTH_CHECK_INTERVAL);
                let plugins = {
                    let s = session.lock().unwrap();
                    s.maybe_check_for_update();
                    s.plugins.clone()
                };
                let probed: Vec<(SourceId, medley_core::PluginHealth)> =
                    plugins.iter().map(|p| (p.id(), p.probe())).collect();
                let changed = session.lock().unwrap().apply_probed_plugin_health(probed);
                if changed {
                    bus.send(medley_core::CoreEvent::PluginStatusChanged);
                }
            }
        })
        .expect("failed to spawn plugin-health thread");
}

/// Background thread, independent of the UI: periodically pulls each registered source's own
/// remote play history (e.g. Spotify's "Recently Played") and folds new plays into medley's
/// local history — see `medley_core::Session::merge_remote_history`. `recently_played` is
/// blocking network I/O, so (like `spawn_plugin_health_timer`'s `probe()`) it runs with the
/// session lock released; only the ingest + file rewrite happen under the lock.
fn spawn_recently_played_merge_timer(session: Arc<Mutex<medley_core::Session>>) {
    std::thread::Builder::new()
        .name("recently-played".into())
        .spawn(move || {
            loop {
                let sources: Vec<Arc<dyn Source>> = session.lock().unwrap().sources.values().cloned().collect();
                let remote: Vec<(Track, DateTime<Utc>)> = sources
                    .iter()
                    .flat_map(|s| {
                        s.recently_played().unwrap_or_else(|e| {
                            log::debug!("recently-played: {}: {e}", s.id());
                            Vec::new()
                        })
                    })
                    .collect();
                if !remote.is_empty() {
                    let added = session.lock().unwrap().merge_remote_history(remote);
                    if added > 0 {
                        log::info!("recently-played: merged {added} play(s) into history");
                    }
                }
                std::thread::sleep(medley_core::RECENTLY_PLAYED_MERGE_INTERVAL);
            }
        })
        .expect("failed to spawn recently-played thread");
}

fn run(log_buf: Arc<LogBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let (mut cfg, mut config_problems) = load_config();
    // Diff base for `source_overrides` at shutdown — captured before Settings-toggled overrides apply.
    let config_source_defaults: Vec<(&str, bool)> =
        TOGGLABLE_SOURCES.iter().map(|&n| (n, cfg.source_enabled(n).unwrap_or(false))).collect();
    let config_vis_fps = cfg.vis.limit();
    if let Some(fps) = load_vis_fps() {
        cfg.vis.fps = fps;
    }
    let config_status_line = cfg.status_line;
    if let Some(shown) = load_status_line() {
        cfg.status_line = shown;
    }
    let config_show_hints = cfg.show_hints;
    if let Some(shown) = load_show_hints() {
        cfg.show_hints = shown;
    }
    let config_auto_update = cfg.auto_update;
    if let Some(on) = load_auto_update() {
        cfg.auto_update = on;
    }
    let config_waveform_enabled = cfg.scan.waveform.enabled;
    if let Some(on) = load_waveform_enabled() {
        cfg.scan.waveform.enabled = on;
    }
    let config_liked_playlist = cfg.liked_playlist.clone();
    if let Some(name) = load_state_string("liked_playlist") {
        cfg.liked_playlist = name;
    }
    if cfg.media_cache_dir.as_os_str().is_empty() {
        cfg.media_cache_dir = data_dir().join("media-cache");
    }
    let config_media_cache_dir = cfg.media_cache_dir.clone();
    if let Some(dir) = load_state_path("media_cache_dir") {
        cfg.media_cache_dir = dir;
    }
    if let Some(from) = load_state_path("media_cache_move_from").filter(|from| *from != cfg.media_cache_dir)
        && let Err(e) = medley_core::move_cache(&from, &cfg.media_cache_dir)
    {
        config_problems.push(format!("media cache not moved to {}: {e}; still using {}", cfg.media_cache_dir.display(), from.display()));
        cfg.media_cache_dir = from;
    }
    let running_media_cache_dir = cfg.media_cache_dir.clone();
    for (name, enabled) in load_source_overrides() {
        cfg.set_source_enabled(&name, enabled);
    }
    let theme = cfg.theme.clone();

    let mut siv = ui::create_cursive()?;
    let cb_sink = siv.cb_sink().clone();
    let bus = Bus::with_sink(move || {
        cb_sink.send(Box::new(|_| {})).ok();
    });

    // HTTP source doubles as its own MediaProvider. `local` (imported files)
    // always gets a player below regardless of this flag.
    let mut sources: HashMap<SourceId, Arc<dyn Source>> = HashMap::new();
    let media = SharedMedia::default();
    // Only pushed to when the spotify/soundcloud/soulseek cargo features are built.
    #[allow(unused_mut)]
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    #[allow(unused_mut)]
    let mut extra_scan_plugins: Vec<Arc<dyn ScanPlugin>> = Vec::new();
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
            cfg.soundcloud.hls,
        ));
        // SoundCloud never populates `Wiring::player` (its playback goes
        // through the shared `rodio` below, not yet built at this point in
        // startup) — a throwaway map here loses nothing.
        register_plugin(plugin.clone(), &mut sources, &media, &mut HashMap::new(), &mut plugins);
        extra_scan_plugins.push(plugin.waveform_scan_plugin());
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
            cfg.media_cache_dir.clone(),
            bus.clone(),
        ));
        // Soulseek never populates `Wiring::player` — playback goes through
        // the shared `rodio` below (once the download lands on disk it's a
        // plain local file, same as an http/soundcloud track).
        register_plugin(plugin, &mut sources, &media, &mut HashMap::new(), &mut plugins);
    }

    // Built before `media_cache` (which needs it) rather than down by the
    // rest of the store wiring below.
    let db_dir = data_dir();
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        log::warn!("config: cannot create data dir {}: {e}", db_dir.display());
    }
    let store: Arc<dyn Store> = Arc::new(medley_core::RedbStore::open(db_dir.join("db"))?);

    // Shared between playback and scanning: every source's decoded audio
    // lands here, keyed by rendition — whichever `ScanPlugin` first decodes
    // a rendition saves the rest (and playback itself) from re-fetching/
    // re-decrypting/re-decoding it. Takes `store` only to resolve a
    // human-readable filename and to prune entries for tracks the store no
    // longer has — never to originate a fetch.
    let media_cache = Arc::new(MediaCache::new(cfg.media_cache_dir.clone(), store.clone()));
    {
        let media_cache = media_cache.clone();
        std::thread::spawn(move || {
            media_cache.prune_orphans();
            media_cache.log_file_stats();
        });
    }
    if cfg.cached.enabled {
        let cached_source = Arc::new(sources_cached::CachedSource::new(media_cache.clone(), store.clone()));
        sources.insert(SourceId::from("cached"), cached_source);
    }
    let engine = medley_core::StreamEngine::new(media.clone(), media_cache.clone(), bus.clone());
    let rodio = Arc::new(RodioPlayer::new(engine.clone(), bus.clone()));
    let mut players: HashMap<SourceId, Arc<dyn Player>> = HashMap::new();
    if http_enabled {
        players.insert(SourceId::from("http"), rodio.clone());
    }
    // Imported local files carry `source = "local"`; route them to the same
    // RodioPlayer so `Session::play_track` finds it.
    players.insert(SourceId::from("local"), rodio.clone());
    if media.contains(&SourceId::from("soundcloud")) {
        // SoundCloud's MediaProvider yields a plain CDN URL; RodioPlayer
        // downloads it exactly like an http track.
        players.insert(SourceId::from("soundcloud"), rodio.clone());
    }
    if media.contains(&SourceId::from("soulseek")) {
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
        ));
        register_plugin(plugin, &mut sources, &media, &mut players, &mut plugins);
        if media.contains(&SourceId::from("spotify")) {
            players.insert(SourceId::from("spotify"), rodio.clone());
        }
    }

    // BpmPlugin is registered into the running driver further down instead
    // (see `register_plugin` below), not passed in here.
    let scan_plugins: Vec<Arc<dyn ScanPlugin>> = Vec::new();
    let bpm_default_mode = if cfg.scan.bpm.enabled { ScanMode::CacheOnly } else { ScanMode::Disabled };
    let bpm_min_interval_secs = cfg.scan.bpm.min_interval_secs;
    let bpm_deezer_enabled = cfg.scan.bpm_deezer.enabled;
    let bpm_deezer_min_interval_secs = cfg.scan.bpm_deezer.min_interval_secs;
    let bpm_getsongbpm_enabled = cfg.scan.bpm_getsongbpm.enabled;
    let bpm_getsongbpm_api_key = cfg.scan.bpm_getsongbpm.api_key.clone();
    let bpm_getsongbpm_min_interval_secs = cfg.scan.bpm_getsongbpm.min_interval_secs;
    // Runtime mode is `B`/`:togglescan`; `cfg.scan.bpm.enabled` above only
    // seeds the default when there's no persisted override in `state.toml`.
    let initial_scan_mode = load_scan_mode(bpm_default_mode);

    log_registered_sources(&sources);

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
        engine,
        data_dir().join("history.m3u8"),
        initial_scan_mode,
    );
    if let Some(scan) = &session.scan {
        scan.register_plugin(Arc::new(bpm::BpmPlugin::new(bpm_min_interval_secs)));
        for p in extra_scan_plugins {
            scan.register_plugin(p);
        }
        scan.register_plugin(Arc::new(waveform::WaveformPlugin::new(
            bpm_min_interval_secs,
            session.waveform_enabled.clone(),
        )));
        if bpm_deezer_enabled {
            scan.register_plugin(Arc::new(bpm_deezer::DeezerBpmPlugin::new(bpm_deezer_min_interval_secs)));
        }
        if bpm_getsongbpm_enabled && let Some(key) = bpm_getsongbpm_api_key {
            scan.register_plugin(Arc::new(bpm_getsongbpm::GetSongBpmPlugin::new(key, bpm_getsongbpm_min_interval_secs)));
        }
    }
    for problem in &config_problems {
        session.warn("config", problem);
    }
    session.set_hotkeys(load_hotkeys());
    session.ensure_remote_playlists();
    session.check_for_update();
    let session = Arc::new(Mutex::new(session));
    spawn_plugin_health_timer(session.clone(), bus.clone());
    spawn_recently_played_merge_timer(session.clone());

    let mut last_played = load_last_played();
    siv.set_theme(ui::theme::load(&theme));
    siv.set_user_data(session.clone());
    siv.add_fullscreen_layer(ui::root_view(session.clone(), log_buf, load_layout(), last_played.clone()));

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
    let terminate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGHUP, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, terminate.clone())?;
    }
    while siv.is_running() {
        if terminate.load(std::sync::atomic::Ordering::Relaxed) {
            siv.quit();
            break;
        }
        let step_start = std::time::Instant::now();
        siv.step();
        // Sleeping after a scrolled frame lets the next scroll events queue up, so cursive handles them in one step and draws once.
        if ui::take_scrolled() {
            std::thread::sleep(ui::SCROLL_FRAME.saturating_sub(step_start.elapsed()));
        }
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
        if !events.is_empty() {
            let mut s = session.lock().unwrap();
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let prev_track = s.now_playing_id();
            for ev in &events {
                dirty |= s.on_event(ev).unwrap_or(false);
            }
            // For the terminal window title's marquee — read here, applied
            // to `siv` below once `s` is released.
            if let Some(now) = s.last_played().filter(|now| last_played.as_ref() != Some(now)) {
                save_last_played(&now);
                last_played = Some(now);
            }
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
        ui::deliver(&mut siv, &events);
        if dirty {
            siv.refresh();
        }
    }

    let layout = ui::saved_layout(&mut siv);
    let s = session.lock().unwrap();
    // Only persisted when it's an explicit override of the config default —
    // otherwise a later default change would be masked by today's value.
    let default_mode = if s.cfg.scan.bpm.enabled { ScanMode::CacheOnly } else { ScanMode::Disabled };
    let scan_mode = s.scan.as_ref().map(|d| d.mode()).filter(|&mode| mode != default_mode);
    let source_overrides: HashMap<&'static str, bool> = config_source_defaults
        .into_iter()
        .filter_map(|(name, default)| {
            let current = s.cfg.source_enabled(name).unwrap_or(default);
            (current != default).then_some((name, current))
        })
        .collect();
    let vis_fps = Some(s.cfg.vis.limit()).filter(|&fps| fps != config_vis_fps);
    let status_line = Some(s.cfg.status_line).filter(|&shown| shown != config_status_line);
    let show_hints = Some(s.cfg.show_hints).filter(|&shown| shown != config_show_hints);
    let auto_update = Some(s.cfg.auto_update).filter(|&on| on != config_auto_update);
    let waveform_enabled = Some(s.cfg.scan.waveform.enabled).filter(|&on| on != config_waveform_enabled);
    let liked_playlist = Some(s.cfg.liked_playlist.as_str()).filter(|&name| name != config_liked_playlist);
    let media_cache_dir = Some(s.cfg.media_cache_dir.as_path()).filter(|&dir| dir != config_media_cache_dir);
    let media_cache_move_from = Some(running_media_cache_dir.as_path()).filter(|&dir| dir != s.cfg.media_cache_dir);
    save_state(
        last_played.as_ref(),
        s.player_status().volume,
        vis_fps,
        status_line,
        show_hints,
        auto_update,
        waveform_enabled,
        liked_playlist,
        media_cache_dir,
        media_cache_move_from,
        scan_mode,
        &s.hotkeys().into_iter().collect(),
        &source_overrides,
        layout,
    );
    s.save_queue();
    drop(s);
    Ok(())
}
