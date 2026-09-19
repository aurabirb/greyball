//! `Session`: the headless engine. All application state lives here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::prelude::*;

use crate::catalog::Catalog;
use crate::config::Config;
use crate::event::{Bus, CoreEvent, PlayerEvent};
use crate::media_cache::MediaCache;
use crate::playlist_m3u::{
    M3uDoc, M3uEntry, ParsedRendition, PlaylistMeta, SoftMeta, parse_m3u, write_entry, write_header,
    write_m3u,
};
use crate::plugin::{Plugin, PluginHealth};
use crate::queue::{Queue, RepeatSetting};
use crate::resolver::{Resolution, Resolver, Target, local_path_from_uri};
use crate::search::Search;
use crate::traits::{
    BrowseNode, Error, MediaProvider, Player, PlayerState, PlayerStatus, Result, Source, Store,
};
use crate::types::{
    LinkReason, Playlist, PlaylistId, Quality, Rendition, SearchHit, SearchQuery, SourceId, Track,
    TrackId, parse_artist_title,
};
use crate::view_cache::{RemoteCtx, ViewCache};

/// Build the synthetic playback-fallback `Rendition` for `track`, if any of
/// its real renditions already has a `MediaCache` entry — routed through the
/// `"local"` source, which already reads straight off disk with no
/// `MediaProvider` involved (see `RodioPlayer::resolve_and_decode`'s
/// `is_local_source` fast path), so the cached file plays exactly like any
/// other local file.
pub fn cache_rendition(cache: &MediaCache, track: &Track) -> Option<Rendition> {
    let path = track.renditions.iter().find_map(|r| cache.cached_path(&r.source, &r.uri))?;
    Some(Rendition {
        source: SourceId::new("local"),
        uri: path.display().to_string(),
        duration_ms: 0,
        quality: Quality::Lossy { kbps: Some(96) },
        link: LinkReason::Manual,
        added_at: chrono::Utc::now(),
    })
}

/// A built-in app command that a single char normally activates
/// (`ui::keybindings::map`'s default table) but that the same remap
/// mechanism as a playlist hotkey can move to a different key. Structural
/// keys (screen switching, `/`, `:`, Enter, Space) aren't remappable and so
/// have no variant here — see `map`'s own doc for why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuiltinAction {
    Next,
    Previous,
    SeekForward,
    SeekBack,
    AddToPlaylistOrNew,
    Quit,
    ClearQueue,
    ToggleScan,
    ToggleShuffle,
    CyclePaneLayout,
    Enqueue,
    Wedge,
    Like,
    Unlike,
    OpenHotkeyMenu,
    OpenHelp,
}

impl BuiltinAction {
    /// Every remappable built-in action with its hardcoded default key —
    /// `map`'s fallback when nothing in the hotkeys table remaps it, and the
    /// full row list `:keys`/the hotkey menu shows alongside playlists.
    pub const ALL: &'static [(BuiltinAction, char)] = &[
        (BuiltinAction::Next, 'n'),
        (BuiltinAction::Previous, 'p'),
        (BuiltinAction::SeekForward, '.'),
        (BuiltinAction::SeekBack, ','),
        (BuiltinAction::AddToPlaylistOrNew, '+'),
        (BuiltinAction::Quit, 'Q'),
        (BuiltinAction::ClearQueue, 'E'),
        (BuiltinAction::ToggleScan, 'B'),
        (BuiltinAction::ToggleShuffle, 's'),
        (BuiltinAction::CyclePaneLayout, 'P'),
        (BuiltinAction::Enqueue, 'q'),
        (BuiltinAction::Wedge, 'w'),
        // Link/unlink are plumbing (merge/split catalog entries), not a
        // day-to-day hotkey action — they're `:link`/`:unlink` commands
        // instead (`ui::command`), leaving 'l'/'L' free for "like"/"unlike".
        (BuiltinAction::Like, 'l'),
        (BuiltinAction::Unlike, 'L'),
        (BuiltinAction::OpenHotkeyMenu, '`'),
        (BuiltinAction::OpenHelp, '?'),
    ];

    /// This action's hardcoded default key — panics only if `ALL` is missing
    /// an entry for a variant, which a test below guards against.
    pub fn default_key(self) -> char {
        Self::ALL
            .iter()
            .find(|(a, _)| *a == self)
            .map(|(_, k)| *k)
            .expect("BuiltinAction::ALL covers every variant")
    }

    /// Human-readable label for the `:keys`/hotkey-menu row and `state.toml`'s
    /// `[hotkeys]` serialization (`app::hotkey_target_to_string`).
    pub fn id(self) -> &'static str {
        match self {
            BuiltinAction::Next => "next",
            BuiltinAction::Previous => "previous",
            BuiltinAction::SeekForward => "seek-forward",
            BuiltinAction::SeekBack => "seek-back",
            BuiltinAction::AddToPlaylistOrNew => "add-to-playlist",
            BuiltinAction::Quit => "quit",
            BuiltinAction::ClearQueue => "clear-queue",
            BuiltinAction::ToggleScan => "toggle-scan",
            BuiltinAction::ToggleShuffle => "toggle-shuffle",
            BuiltinAction::CyclePaneLayout => "cycle-panes",
            BuiltinAction::Enqueue => "enqueue",
            BuiltinAction::Wedge => "wedge",
            BuiltinAction::Like => "like",
            BuiltinAction::Unlike => "unlike",
            BuiltinAction::OpenHotkeyMenu => "open-hotkey-menu",
            BuiltinAction::OpenHelp => "open-help",
        }
    }

    /// Inverse of `id` — for `state.toml`'s `hotkey_target_from_string`.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().map(|&(a, _)| a).find(|a| a.id() == id)
    }

    /// Display name for the `:keys`/hotkey-menu row.
    pub fn label(self) -> &'static str {
        match self {
            BuiltinAction::Next => "next track",
            BuiltinAction::Previous => "previous track",
            BuiltinAction::SeekForward => "seek forward 5s",
            BuiltinAction::SeekBack => "seek back 5s",
            BuiltinAction::AddToPlaylistOrNew => "add to playlist / new playlist",
            BuiltinAction::Quit => "quit",
            BuiltinAction::ClearQueue => "clear the queue",
            BuiltinAction::ToggleScan => "toggle background scan active/cache-only",
            BuiltinAction::ToggleShuffle => "toggle queue shuffle",
            BuiltinAction::CyclePaneLayout => "cycle embedded-pane layout",
            BuiltinAction::Enqueue => "enqueue selected track",
            BuiltinAction::Wedge => "wedge selected track to queue front",
            BuiltinAction::Like => "add selected track to Liked Songs",
            BuiltinAction::Unlike => "remove selected track from Liked Songs (confirms first)",
            BuiltinAction::OpenHotkeyMenu => "open hotkeys menu",
            BuiltinAction::OpenHelp => "open help/shortcuts screen",
        }
    }
}

/// What a hotkey (and `Command::TogglePlaylistMembership`) refers to: a
/// local (medley) playlist by id, a remote source's playlist folder —
/// mirrors the `TopRow` split `ui/src/view.rs`'s Playlists screen already
/// renders (local playlists first, then each source's browse folders) — or
/// a built-in app command, remapped off its hardcoded default key. One
/// `HashMap<char, HotkeyTarget>` (`Session::hotkeys`) covers all three, so
/// binding/stealing/persistence is a single mechanism regardless of which
/// kind of target a key names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyTarget {
    Local(PlaylistId),
    Remote(SourceId, BrowseNode),
    Builtin(BuiltinAction),
}

/// Every user intent.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Search(String),
    Play(TrackId),
    /// "Play this list, starting here" — the whole currently-visible list
    /// (search results, a playlist, Liked Songs, ...), remembered as a
    /// fallback: once the manual queue has nothing left to play,
    /// `PlayerEvent::Finished` advances into the rest of this list instead
    /// of stopping (see `Session::advance`). Doesn't touch the
    /// queue itself. `index` must be in bounds for `tracks`.
    PlayContext {
        tracks: Vec<TrackId>,
        index: usize,
        /// Set when `tracks` came from a remote paginated node (e.g. Spotify
        /// Liked Songs) that may still be loading — lets `advance`
        /// re-check the live list instead of stopping at a stale snapshot's end.
        remote: Option<(SourceId, BrowseNode)>,
        /// The originating context's display name (a playlist name, "Search
        /// results", "Queue", a remote folder's name, ...), for Now
        /// Playing's title — `None` when the call site has no natural name.
        name: Option<String>,
    },
    Enqueue(TrackId),
    /// Insert at the front of the queue instead of the back — plays next,
    /// ahead of anything already `Enqueue`d.
    Wedge(TrackId),
    PlayPause,
    Next,
    Previous,
    /// relative ms
    Seek(i64),
    /// relative %
    Volume(i8),
    NewPlaylist(String),
    AddToPlaylist {
        track: TrackId,
        playlist: PlaylistId,
    },
    /// Playlist-hotkey toggle (backtick-bound key pressed on a selected
    /// track): adds `track` to `playlist` if absent, else removes every
    /// occurrence of it. `playlist` may be local or remote — see
    /// `Session::toggle_playlist_membership`.
    TogglePlaylistMembership {
        track: TrackId,
        playlist: HotkeyTarget,
    },
    LinkPick(TrackId),
    Unlink(TrackId),
    /// `f`: idempotent add-only to whichever liked/favorites synthetic
    /// playlist is discoverable for the track's own source(s) — see
    /// `Session::liked_targets`. Never removes.
    Like(TrackId),
    /// `F`, after the UI's own confirmation dialog: removes from the same
    /// liked/favorites playlist(s) `Like` would add to.
    Unlike(TrackId),
    ExportM3u(PlaylistId),
    /// Export to an explicit path.
    ExportM3uTo {
        playlist: PlaylistId,
        path: PathBuf,
    },
    /// Import an enriched-M3U file, folding its tracks into the catalog.
    ImportM3u(PathBuf),
    AddUri(String),
    /// Add local audio files to a playlist by path (`:add`/`:open` line), or
    /// to the queue when no playlist is given (no open/selected playlist to
    /// add to). Paths are resolved against the process CWD; non-audio or
    /// missing files are skipped.
    AddFilesToPlaylist {
        playlist: Option<PlaylistId>,
        paths: Vec<PathBuf>,
    },
    /// Empty the queue (and stop playback).
    ClearQueue,
    /// Toggles the background scan driver's mode between active and
    /// cache-only (`:togglescan`). Never enters or leaves `Disabled` — that's
    /// config-only (`scan.bpm.enabled`). No-op if no scan plugin is
    /// registered or it's currently disabled.
    ToggleScan,
    /// Toggles queue shuffle (`s`/`:toggleshuffle`) — see `Queue::set_shuffle`
    /// for what turning it on/off actually does to the queue's order.
    ToggleShuffle,
    Quit,
}

/// The queue is really just an ordered list of tracks — the same shape as a
/// [`Playlist`] — so it's persisted across restarts through the same
/// `Store::upsert_playlist`/`get_playlist` path under this fixed id, rather
/// than inventing a separate storage format. [`Session::playlists`] filters
/// it out so it never shows up as a real playlist.
pub const QUEUE_PLAYLIST_ID: PlaylistId = PlaylistId(crate::types::Uuid::from_u128(0));
/// Play history used to be persisted the same way, under this id — it now
/// lives in an append-only M3U log instead (`history_path`,
/// `append_history_entry`, `load_history_file`), but the id stays reserved
/// and still filtered out of `Session::playlists` so a redb database from
/// before that change doesn't leak a leftover `__history__` entry into the
/// Playlists screen after an upgrade.
pub const HISTORY_PLAYLIST_ID: PlaylistId = PlaylistId(crate::types::Uuid::from_u128(1));
/// The Now Playing screen's context (`PlaybackContext`) is persisted the same
/// way the queue is — a `Playlist` under a fixed reserved id, restored by
/// `Session::new` — since it's the same shape (an ordered `TrackId` list plus
/// a name); `index` rides along encoded in `notes` (see
/// `save_now_playing_context`). Filtered out of `Session::playlists` like the
/// other two reserved ids.
pub const NOW_PLAYING_PLAYLIST_ID: PlaylistId = PlaylistId(crate::types::Uuid::from_u128(2));

/// Result of a command the front-end must react to beyond re-rendering.
#[derive(Debug, PartialEq)]
pub enum Dispatch {
    Ok,
    Modal(String),
    Quit,
}

/// The list `Command::PlayContext` last started playing from (search
/// results, a playlist, Liked Songs, ...) — consulted as a fallback once the
/// manual queue (`Session::queue`) has nothing left, so playback keeps
/// going through the rest of that list instead of stopping. The manual
/// queue always takes priority: anything explicitly enqueued (`q`) plays
/// before this continues. See `Session::advance`.
struct PlaybackContext {
    tracks: Vec<TrackId>,
    /// Index into `tracks` of the track that's playing, or that just
    /// finished (advanced only when this context itself supplies the next
    /// track — not when the manual queue intervenes in between).
    index: usize,
    /// See `Command::PlayContext::remote`.
    remote: Option<(SourceId, BrowseNode)>,
    /// See `Command::PlayContext::name`.
    name: Option<String>,
    /// Remaining draw order for shuffle mode: a randomized permutation of
    /// indices into `tracks` not yet played this pass, consumed from the
    /// back by `play_next_in_context`. Refilled with a fresh shuffle of
    /// every index once exhausted, so shuffle plays each track once before
    /// any repeats rather than picking fully at random every time. Ignored
    /// (and left to go stale) while shuffle is off.
    shuffle_bag: Vec<usize>,
}

/// One playlist-bound hotkey's row of the hotkeys column: `members` show its letter, `pending` italicize it.
pub struct HotkeyMembership {
    pub key: char,
    pub members: HashSet<TrackId>,
    pub pending: HashSet<TrackId>,
}

struct HotkeyMemo {
    table: Arc<Vec<HotkeyMembership>>,
    dirty: bool,
    /// A bound remote playlist hadn't finished loading as of `built`.
    loading: bool,
    built: Instant,
}

impl Default for HotkeyMemo {
    fn default() -> Self {
        Self { table: Arc::default(), dirty: true, loading: false, built: Instant::now() }
    }
}

const HOTKEY_MEMO_LOADING_RECHECK: Duration = Duration::from_secs(1);

/// Cooldown for `app/src/main.rs`'s background plugin-health timer — matches
/// the plugins' own probe cooldowns (Spotify's `AUTO_REFRESH_COOLDOWN`,
/// Soulseek's `CHECK_COOLDOWN`), so the timer never probes more often than a
/// plugin's own background check could actually produce a new result.
pub const PLUGIN_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Why `Session::bind_hotkey` refused.
#[derive(Debug)]
pub enum BindError {
    /// `key` belongs to this built-in action, which only moves by being remapped itself.
    BuiltinKey(HotkeyTarget),
    /// The target is a synthetic node (e.g. Liked Songs), not a real playlist.
    SyntheticPlaylist,
}

pub struct Session {
    pub bus: Bus,
    pub store: Arc<dyn Store>,
    pub catalog: Arc<Catalog>,
    pub search: Arc<Search>,
    pub queue: Arc<Queue>,
    pub sources: HashMap<SourceId, Arc<dyn Source>>,
    pub media: HashMap<SourceId, Arc<dyn MediaProvider>>,
    pub players: HashMap<SourceId, Arc<dyn Player>>,
    /// Every optional source plugin `app` built, registered or not — see
    /// `plugin::Plugin`. Kept even for a plugin that ended up `Fail` (and so
    /// has nothing in `sources`/`media`/`players`) so its message still
    /// shows in the warnings panel.
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// `:`-word → owning plugin, from each plugin's `Plugin::commands()`,
    /// built once here at construction — the seam `ui::command::resolve`
    /// consults for a word it doesn't otherwise recognize
    /// (`plugin_for_command`), instead of every plugin-specific command
    /// being hand-added to `ui::command`.
    plugin_commands: HashMap<String, Arc<dyn Plugin>>,
    pub cfg: Arc<Config>,
    /// Background scan driver (bpm, later genre/...), if any plugin is
    /// registered. Built and spawned in `session_builder::build_session`.
    pub scan: Option<Arc<crate::scan::ScanDriver>>,
    /// Same cache the player and scan driver decode audio into
    /// (`session_builder::build_session` hands all three the same `Arc`) —
    /// held here too so the UI can ask "is this track's audio already on
    /// disk" (`is_track_cached`), and `play_from_cache` plays straight from
    /// it when a track's own renditions are unavailable.
    media_cache: Arc<MediaCache>,
    /// Track id a background `play_from_cache` decode-on-demand is running
    /// for, if any — `CoreEvent::CacheFallbackReady` only acts on it when
    /// this still matches, so a later `play_track` call (the user moved on)
    /// silently supersedes a stale in-flight decode instead of it hijacking
    /// playback once it finishes.
    pending_cache_fallback: Option<TrackId>,
    /// Sources already tried for the currently-playing track this attempt —
    /// reset per `play_track`, so `LoadFailed` can retry the next-best
    /// rendition instead of the one that just failed.
    failed_playback_sources: Vec<SourceId>,
    /// Where the play-history M3U log lives — one entry appended per play,
    /// see `append_history_entry`. The whole `Session` already lives behind
    /// one `Arc<Mutex<Session>>` in `main.rs`, so every call in here is
    /// already serialized; nothing extra is needed to keep this the only
    /// writer of the file.
    history_path: PathBuf,

    // view state, updated by on_event:
    now_playing: Option<TrackId>,
    /// The `Player` actually driving `now_playing`, set in `start_playback` —
    /// can differ from `Track::best_rendition`'s player after a fallback.
    now_playing_player: Option<Arc<dyn Player>>,
    last_status: PlayerStatus,
    link_pick: Option<TrackId>,
    volume: f32,
    context: Option<PlaybackContext>,

    /// Playlist hotkeys (backtick-bound single keys, one target per key and
    /// one key per target) — UI-local, persisted to
    /// `state.toml` by `app` (`load_hotkeys`/`save_state`). See
    /// `bind_hotkey`/`hotkey_for`/`playlist_hotkey`.
    hotkeys: HashMap<char, HotkeyTarget>,

    /// Search results + remote-playlist browsing caches — see
    /// `view_cache::ViewCache`.
    view: ViewCache,

    /// Latest result of an in-flight remote-playlist hotkey toggle (add/
    /// remove over the network, see `toggle_remote_playlist_membership`),
    /// if any — `Arc<Mutex<_>>` so the background thread that runs the
    /// actual `Source` call can report back without holding `Session`'s own
    /// lock across the network round trip. Polled (not pushed) by the UI's
    /// hotkey-menu footer on every redraw — cheap, since it's just reading
    /// whatever string (if any) already landed here.
    membership_feedback: Arc<Mutex<Option<String>>>,

    /// Memoized hotkeys-column lookup — see `hotkey_memberships`.
    hotkey_memberships: Mutex<HotkeyMemo>,

    /// Most recent `Plugin::setup` outcome per plugin, until superseded — see
    /// `refresh_plugin_health`. Written by the UI's `run_plugin_setup` once
    /// `setup()` returns, so a real failure (an OAuth error, a listener-bind
    /// failure, ...) shows instead of `probe()`'s generic pre-login text.
    last_setup: HashMap<SourceId, PluginHealth>,

    /// Cache of `plugin_statuses`, recomputed by `refresh_plugin_health`
    /// instead of probing every plugin on every call — a probe can do disk
    /// or network I/O (Spotify's cached-token read, Soulseek's slskd ping)
    /// and this used to run under the session lock on every frame/layout/
    /// mouse event.
    plugin_health: Vec<(SourceId, PluginHealth)>,

    /// Latest plugin-command result (`ui::run_plugin_command`'s off-thread
    /// `Plugin::run_command` call), if any, waiting to be shown as a modal
    /// — same `Arc<Mutex<_>>`-report-without-the-session-lock shape as
    /// `membership_feedback`, since the command can block (an OAuth browser
    /// flow) just like `Plugin::setup`.
    plugin_command_result: Arc<Mutex<Option<String>>>,

    /// Bumped by every UI-visible mutation — the frame cache keys on this; see `view/README.md`.
    revision: u64,
    /// Bumped only by a list membership/order/identity/name change — see `view/README.md`.
    list_revision: u64,
}

impl Session {
    /// Wire `Catalog`, `Search`, `Queue` from the registries and return a
    /// `Session`. (`app::build_session` is a thin wrapper over this.)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        bus: Bus,
        store: Arc<dyn Store>,
        sources: HashMap<SourceId, Arc<dyn Source>>,
        media: HashMap<SourceId, Arc<dyn MediaProvider>>,
        players: HashMap<SourceId, Arc<dyn Player>>,
        plugins: Vec<Arc<dyn Plugin>>,
        media_cache: Arc<MediaCache>,
        history_path: PathBuf,
    ) -> Self {
        let catalog = Arc::new(Catalog::new(store.clone(), bus.clone()));
        let search = Arc::new(Search::new(
            sources.values().cloned().collect(),
            catalog.clone(),
            bus.clone(),
        ));
        let queue = Arc::new(Queue::new(bus.clone()));
        // Restore the queue saved on the previous exit (`save_queue`), if any.
        if let Ok(Some(saved)) = store.get_playlist(QUEUE_PLAYLIST_ID) {
            for id in saved.items {
                queue.append(id);
            }
        }
        // Restore play history from the tail of the on-disk M3U log (oldest
        // first, capped at `Queue`'s own MAX_HISTORY on the way in).
        queue.restore_history(load_history_file(&history_path));
        // Restore the Now Playing context saved on the previous exit (or
        // last advance), if any — same reserved-playlist mechanism as the
        // queue above. Empty `items` (nothing ever played) leaves no context.
        let context = store
            .get_playlist(NOW_PLAYING_PLAYLIST_ID)
            .ok()
            .flatten()
            .filter(|p| !p.items.is_empty())
            .map(|p| {
                let index = p.notes.parse::<usize>().unwrap_or(0).min(p.items.len() - 1);
                PlaybackContext {
                    tracks: p.items,
                    index,
                    remote: None,
                    name: (!p.name.is_empty()).then_some(p.name),
                    shuffle_bag: Vec::new(),
                }
            });
        let volume = cfg.volume.clamp(0.0, 1.0);
        let last_status = PlayerStatus {
            volume,
            ..PlayerStatus::default()
        };
        let mut plugin_commands = HashMap::new();
        for p in &plugins {
            for c in p.commands() {
                plugin_commands.insert(c.word, p.clone());
            }
        }
        let mut session = Self {
            bus,
            store,
            catalog,
            search,
            queue,
            sources,
            media,
            players,
            plugins,
            plugin_commands,
            cfg: Arc::new(cfg),
            scan: None,
            media_cache,
            pending_cache_fallback: None,
            failed_playback_sources: Vec::new(),
            history_path,
            now_playing: None,
            now_playing_player: None,
            last_status,
            link_pick: None,
            volume,
            context,
            hotkeys: HashMap::new(),
            view: ViewCache::default(),
            membership_feedback: Arc::new(Mutex::new(None)),
            hotkey_memberships: Mutex::new(HotkeyMemo::default()),
            last_setup: HashMap::new(),
            plugin_health: Vec::new(),
            plugin_command_result: Arc::new(Mutex::new(None)),
            revision: 0,
            list_revision: 0,
        };
        session.refresh_plugin_health();
        session
    }

    /// Monotonic counter the UI's frame cache keys on — see the field doc.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Monotonic counter the UI's filter/follow/Help/picker caches key on — see the field doc.
    pub fn list_revision(&self) -> u64 {
        self.list_revision
    }

    /// Bumps `revision` — called by every mutation of UI-visible session state.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Bumps `list_revision` (and `revision`, so a caller never has to remember both).
    fn touch_lists(&mut self) {
        self.list_revision = self.list_revision.wrapping_add(1);
        self.touch();
    }

    pub fn dispatch(&mut self, cmd: Command) -> Result<Dispatch> {
        self.touch();
        match cmd {
            Command::Search(text) => {
                self.touch_lists();
                self.view.begin_search(&text);
                self.search.run(SearchQuery::text(text));
                Ok(Dispatch::Ok)
            }
            Command::Play(id) => {
                self.play_now(id);
                Ok(Dispatch::Ok)
            }
            Command::PlayContext { tracks, index, remote, name } => {
                let Some(&id) = tracks.get(index) else {
                    return Ok(Dispatch::Ok);
                };
                self.touch_lists();
                self.context = Some(PlaybackContext { tracks, index, remote, name, shuffle_bag: Vec::new() });
                self.save_now_playing_context();
                // `play_now` never touches the manual queue's contents
                // beyond pulling `id` out of it if it happens to already be
                // queued — an explicitly `q`-enqueued track must still win
                // over this context on the next `Finished` (see
                // `Session::advance`), which only works if the queue stays
                // whatever the user actually built.
                self.play_now(id);
                Ok(Dispatch::Ok)
            }
            Command::Enqueue(id) => {
                self.queue.append(id);
                Ok(Dispatch::Ok)
            }
            Command::Wedge(id) => {
                self.queue.play_next(id);
                Ok(Dispatch::Ok)
            }
            Command::PlayPause => {
                if let Some(p) = self.active_player() {
                    p.toggle();
                    self.last_status = p.status();
                    self.last_status.volume = self.volume;
                } else {
                    // Nothing loaded yet — start playback from the queue
                    // (current item if one is set, otherwise the first).
                    self.queue.toggleplayback();
                }
                Ok(Dispatch::Ok)
            }
            Command::Next => {
                self.advance(true);
                Ok(Dispatch::Ok)
            }
            Command::ClearQueue => {
                self.queue.clear();
                Ok(Dispatch::Ok)
            }
            Command::ToggleScan => {
                if let Some(scan) = &self.scan {
                    scan.set_mode(scan.mode().cycle());
                }
                Ok(Dispatch::Ok)
            }
            Command::ToggleShuffle => {
                self.queue.set_shuffle(!self.queue.get_shuffle());
                Ok(Dispatch::Ok)
            }
            Command::Previous => {
                // The manual queue drains as it plays, so it never has
                // anything "earlier" to go back to by definition — previous
                // always falls back to the bounded play history, which is
                // structurally independent of the queue's own contents.
                let just_playing = self.now_playing;
                if self.queue.history_len() > 0 {
                    self.touch_lists();
                }
                if let Some(id) = self.queue.previous_from_history() {
                    // Wedge the track we're walking back over onto the
                    // queue's front, so a later `n` resumes forward through
                    // it instead of skipping straight past it.
                    if let Some(playing) = just_playing {
                        self.queue.play_next(playing);
                    }
                    self.play_track(id, false);
                }
                Ok(Dispatch::Ok)
            }
            Command::Seek(delta) => {
                if let Some(p) = self.active_player() {
                    let cur = p.status();
                    let target = (cur.position_ms as i64 + delta).max(0) as u32;
                    p.seek(target);
                    self.last_status = p.status();
                    self.last_status.volume = self.volume;
                }
                Ok(Dispatch::Ok)
            }
            Command::Volume(delta) => {
                let new = (self.volume + delta as f32 / 100.0).clamp(0.0, 1.0);
                self.volume = new;
                for p in self.players.values() {
                    p.set_volume(new);
                }
                self.last_status.volume = new;
                // MVP: persistence of volume to the state file is done by `app`.
                Ok(Dispatch::Ok)
            }
            Command::NewPlaylist(name) => {
                let p = Playlist {
                    id: PlaylistId::new(),
                    name,
                    notes: String::new(),
                    items: vec![],
                };
                self.save_playlist(&p)?;
                Ok(Dispatch::Ok)
            }
            Command::AddToPlaylist { track, playlist } => {
                let mut p = self
                    .store
                    .get_playlist(playlist)?
                    .ok_or(Error::NotFound)?;
                p.items.push(track);
                self.save_playlist(&p)?;
                Ok(Dispatch::Ok)
            }
            Command::TogglePlaylistMembership { track, playlist } => {
                self.toggle_playlist_membership(track, playlist)
            }
            Command::LinkPick(id) => match self.link_pick.take() {
                None => {
                    self.link_pick = Some(id);
                    Ok(Dispatch::Modal("link: pick a second row".to_string()))
                }
                Some(first) => {
                    self.catalog.link(first, id)?;
                    self.touch_lists();
                    Ok(Dispatch::Ok)
                }
            },
            Command::Unlink(id) => {
                let track = self.store.get_track(id)?.ok_or(Error::NotFound)?;
                // M1: unlink all but the first rendition.
                let mut sources: Vec<SourceId> = Vec::new();
                for r in track.renditions.iter().skip(1) {
                    if !sources.contains(&r.source) {
                        sources.push(r.source.clone());
                    }
                }
                for s in sources {
                    self.catalog.unlink(id, s)?;
                }
                self.touch_lists();
                Ok(Dispatch::Ok)
            }
            Command::Like(track) => self.set_liked(track, true),
            Command::Unlike(track) => self.set_liked(track, false),
            Command::ExportM3u(id) => {
                let name = self.store.get_playlist(id)?.ok_or(Error::NotFound)?.name;
                self.export_m3u_to(id, PathBuf::from(format!("{name}.m3u8")))
            }
            Command::ExportM3uTo { playlist, path } => self.export_m3u_to(playlist, path),
            Command::ImportM3u(path) => self.import_m3u(path),
            Command::AddUri(uri) => {
                let Some(source) = self.source_for_uri(&uri) else {
                    return Ok(Dispatch::Modal("not a playable URL".to_string()));
                };
                let source = source.clone();
                match source.resolve(&uri) {
                    Ok(hit) => {
                        let tid = self.catalog.ingest(hit)?;
                        self.touch_lists();
                        self.push_result(tid);
                        Ok(Dispatch::Ok)
                    }
                    Err(_) => Ok(Dispatch::Modal("not a playable URL".to_string())),
                }
            }
            Command::AddFilesToPlaylist { playlist, paths } => {
                self.add_files_to_playlist(playlist, paths)
            }
            Command::Quit => Ok(Dispatch::Quit),
        }
    }

    pub fn on_event(&mut self, ev: &CoreEvent) -> Result<bool> {
        // `Progress` is per-tick playback position, deliberately excluded —
        // the UI reads it live every frame instead of caching on `revision`.
        if !matches!(ev, CoreEvent::Player(PlayerEvent::Progress { .. })) {
            self.touch();
        }
        match ev {
            CoreEvent::SearchHit(tid) => {
                self.touch_lists();
                self.push_result(*tid);
                Ok(true)
            }
            CoreEvent::TrackUpdated(id) => {
                // Attribute-only: `revision` (bumped above) refreshes the frame; `list_revision` stays put.
                self.refresh_cached_track(*id);
                Ok(true)
            }
            CoreEvent::PlaylistsChanged => {
                self.touch_lists();
                self.invalidate_hotkey_memberships();
                Ok(true)
            }
            CoreEvent::QueueChanged => {
                self.touch_lists();
                Ok(true)
            }
            CoreEvent::SearchDone { .. } => Ok(true),
            CoreEvent::PlayRequested(id) => {
                self.play_track(*id, true);
                Ok(true)
            }
            CoreEvent::Player(pe) => self.on_player_event(pe),
            CoreEvent::CacheFallbackReady(id) => {
                // Plays the fallback rendition directly rather than
                // re-`play_track`ing: whatever made the decode-on-demand
                // necessary in the first place (a gap, no player, or a
                // `LoadFailed` on the "live" rendition) hasn't changed, so
                // re-resolving would just repeat that same failed attempt
                // before falling back again.
                if self.pending_cache_fallback == Some(*id)
                    && let Ok(Some(t)) = self.store.get_track(*id)
                {
                    self.pending_cache_fallback = None;
                    self.play_from_cache(&t, true);
                }
                Ok(true)
            }
            CoreEvent::PluginStatusChanged => {
                // Health can improve outside setup()/run_command() (e.g. an
                // auto-refresh) — rewire so that isn't stuck until a manual setup.
                self.rewire_all_plugins();
                self.refresh_plugin_health();
                // A source logging in later still needs its playlists kicked off.
                self.ensure_remote_playlists();
                Ok(true)
            }
            CoreEvent::SourceError { .. }
            | CoreEvent::PluginLoginSucceeded
            | CoreEvent::PluginCommandResult => Ok(true),
        }
    }

    // ---- plugins (Spotify/SoundCloud login status + deferred setup) ----

    /// Every registered plugin's id + cached health — see `plugin_health`.
    /// Cheap: just a clone of the cache, no probing.
    pub fn plugin_statuses(&self) -> &[(SourceId, PluginHealth)] {
        &self.plugin_health
    }

    /// Number of plugins currently reporting a non-`Ok` health — for the
    /// warnings button/tab, cheaper than filtering `plugin_statuses` at
    /// every call site.
    pub fn plugin_warning_count(&self) -> usize {
        self.plugin_health.iter().filter(|(_, h)| !h.is_ok()).count()
    }

    /// `probe()` only ever returns a few generic canned strings, so a real
    /// setup failure (`last_setup`) is more informative and wins until a
    /// fresh probe comes back `Ok` again, which always supersedes it.
    fn overlay_setup(&self, probed: Vec<(SourceId, PluginHealth)>) -> Vec<(SourceId, PluginHealth)> {
        probed
            .into_iter()
            .map(|(id, h)| {
                let health = if h.is_ok() { h } else { self.last_setup.get(&id).cloned().unwrap_or(h) };
                (id, health)
            })
            .collect()
    }

    /// Re-probes every plugin (on the session lock — only safe here, at
    /// startup or from an explicit user action, never on a hot path) and
    /// unconditionally rebuilds `plugin_health`. Called once at startup, by
    /// `open_warnings`, and by `CoreEvent::PluginStatusChanged`'s handler,
    /// which already knows something changed.
    pub fn refresh_plugin_health(&mut self) {
        self.touch();
        let probed: Vec<(SourceId, PluginHealth)> = self.plugins.iter().map(|p| (p.id(), p.probe())).collect();
        self.plugin_health = self.overlay_setup(probed);
    }

    /// Applies health values already probed off the session lock — by
    /// `app/src/main.rs`'s periodic background timer, since `probe()` can do
    /// real I/O (Spotify's cached-token read, Soulseek's slskd ping) and
    /// this runs every `PLUGIN_HEALTH_CHECK_INTERVAL`. Only replaces the
    /// cache (and reports a change) when the overlaid result actually
    /// differs, so an unchanged tick causes no rewire/redraw.
    pub fn apply_probed_plugin_health(&mut self, probed: Vec<(SourceId, PluginHealth)>) -> bool {
        let health = self.overlay_setup(probed);
        if health == self.plugin_health {
            return false;
        }
        self.plugin_health = health;
        self.touch();
        true
    }

    /// Record `id`'s last `Plugin::setup` result — called once `setup()`
    /// returns, by `run_plugin_setup`, which always sends
    /// `CoreEvent::PluginStatusChanged` right after; that event's handler
    /// does the actual re-probe, so this only needs to update `last_setup`.
    pub fn record_setup_result(&mut self, id: SourceId, health: PluginHealth) {
        self.touch();
        self.last_setup.insert(id, health);
    }

    /// A cloned handle to one plugin, to call its (blocking) `setup()` off
    /// the session lock — see `run_plugin_setup`'s doc for why this exists
    /// instead of a `Session` method that calls `setup()` itself.
    pub fn plugin(&self, id: &SourceId) -> Option<Arc<dyn Plugin>> {
        self.plugins.iter().find(|p| &p.id() == id).cloned()
    }

    /// Every plugin-registered `:`-command's word + help text, for `:help`.
    pub fn plugin_command_help(&self) -> Vec<(String, String)> {
        self.plugins
            .iter()
            .flat_map(|p| p.commands())
            .map(|c| (c.word, c.help))
            .collect()
    }

    /// The plugin that owns `word`, if any — `ui::command::resolve`'s hook
    /// before falling through to "unknown command", and the same cloned-
    /// handle-then-drop-the-lock handle `ui::run_plugin_command` runs the
    /// (possibly blocking) command through off the session lock, mirroring
    /// `plugin`/`run_plugin_setup`.
    pub fn plugin_for_command(&self, word: &str) -> Option<Arc<dyn Plugin>> {
        self.plugin_commands.get(word).cloned()
    }

    /// A cloned handle to `plugin_command_result` — `ui::run_plugin_command`
    /// clones this out before running the (possibly blocking) command, so
    /// its background thread can report the result without re-taking the
    /// session lock.
    pub fn plugin_command_result_handle(&self) -> Arc<Mutex<Option<String>>> {
        self.plugin_command_result.clone()
    }

    /// The last plugin-command result, if any, clearing it — called once per
    /// event-loop iteration so a result is shown (as a modal) exactly once.
    pub fn take_plugin_command_result(&self) -> Option<String> {
        self.plugin_command_result.lock().unwrap().take()
    }

    /// Install whatever `wiring` a plugin currently offers into the live
    /// session — used both at startup (still via this same path, so there's
    /// only one place that does it) and after a `setup()` completes. `None`
    /// fields leave that map's existing entry (if any) untouched rather than
    /// removing it — a `setup()` that only fixes, say, the player, mustn't
    /// silently drop an already-working source.
    pub fn apply_wiring(&mut self, id: &SourceId, wiring: crate::plugin::Wiring) {
        self.touch_lists();
        if let Some(s) = wiring.source {
            self.sources.insert(id.clone(), s);
            self.prune_synthetic_hotkeys();
        }
        // Also pushed to the scan driver's own live wiring, so a source
        // wired up after startup isn't stuck behind a walk-thread snapshot
        // frozen at spawn time — see `ScanDriver::update_wiring`.
        if let Some(scan) = &self.scan {
            scan.update_wiring(id.clone(), wiring.media.clone(), wiring.player.clone());
        }
        if let Some(m) = wiring.media {
            self.media.insert(id.clone(), m);
        }
        if let Some(p) = wiring.player {
            self.players.insert(id.clone(), p);
        }
    }

    /// Re-pulls and re-applies every plugin's current `wiring()`.
    fn rewire_all_plugins(&mut self) {
        for p in self.plugins.clone() {
            let id = p.id();
            let wiring = p.wiring();
            self.apply_wiring(&id, wiring);
        }
    }

    fn on_player_event(&mut self, pe: &PlayerEvent) -> Result<bool> {
        match pe {
            PlayerEvent::Loading { source, uri } | PlayerEvent::Playing { source, uri } => {
                if let Some(t) = self.store.track_by_rendition(source, uri)? {
                    self.now_playing = Some(t.id);
                    if matches!(pe, PlayerEvent::Playing { .. })
                        && let Some(scan) = &self.scan
                    {
                        scan.prioritize(t.id);
                    }
                }
                self.refresh_status();
                if matches!(pe, PlayerEvent::Playing { .. }) {
                    self.last_status.state = PlayerState::Playing;
                }
                Ok(true)
            }
            PlayerEvent::Progress {
                position_ms,
                duration_ms,
            } => {
                self.last_status.position_ms = *position_ms;
                self.last_status.duration_ms = *duration_ms;
                Ok(true)
            }
            PlayerEvent::Paused => {
                self.last_status.state = PlayerState::Paused;
                Ok(true)
            }
            PlayerEvent::Stopped => {
                self.last_status.state = PlayerState::Stopped;
                Ok(true)
            }
            PlayerEvent::Finished { source, uri } => {
                // Only advance if the finished rendition maps to the track the
                // queue currently considers playing (guards stale Finished).
                let finished = self.store.track_by_rendition(source, uri)?.map(|t| t.id);
                let current = self.queue.get_current();
                if finished.is_some() && finished == current {
                    self.advance(false);
                }
                Ok(true)
            }
            PlayerEvent::PreloadHint { source, uri } => {
                let hinted = self.store.track_by_rendition(source, uri)?.map(|t| t.id);
                if hinted.is_some() && hinted == self.queue.get_current() {
                    self.preload_upcoming();
                }
                Ok(false)
            }
            PlayerEvent::LoadFailed { source, uri } => {
                self.last_status.state = PlayerState::Stopped;
                // Only the track the queue still considers current is worth a
                // fallback retry — a stale failure for whatever played before
                // the user already moved on must not hijack playback.
                if let Some(t) = self.store.track_by_rendition(source, uri)?
                    && self.queue.get_current() == Some(t.id)
                {
                    self.failed_playback_sources.push(source.clone());
                    let retry = match Resolver::resolve_playback_excluding(&t, &self.failed_playback_sources) {
                        Resolution::Ready(r) => self.pick_player(&r).map(|p| (r, p)),
                        Resolution::Gap { .. } => None,
                    };
                    match retry {
                        Some((r, p)) => {
                            log::warn!(
                                "player: load failed for \"{}\" ({source}) — trying {} instead",
                                t.title,
                                r.source
                            );
                            self.start_playback(&t, &r, p, true);
                        }
                        None => {
                            log::warn!(
                                "player: load failed for \"{}\" ({source}) — trying local-cache fallback",
                                t.title
                            );
                            self.pending_cache_fallback = None;
                            if !self.play_from_cache(&t, true) {
                                self.bus.send(CoreEvent::SourceError {
                                    source: source.clone(),
                                    message: format!("playback failed for {uri}"),
                                });
                                self.advance(false);
                            }
                        }
                    }
                }
                Ok(true)
            }
            PlayerEvent::Materialized { source, uri } => {
                // The precise trigger for the currently-playing-track fast
                // path: rather than scanning polling `ScanFetchMode::CacheOnly`
                // every tick hoping a source finished fetching on its own,
                // the source announces it directly and this is the one place
                // that reacts — same `prioritize` a plain `Playing` already
                // does as a first (possibly premature) attempt.
                if let Some(t) = self.store.track_by_rendition(source, uri)?
                    && let Some(scan) = &self.scan
                {
                    log::debug!("app: materialized {source} {uri} -> prioritizing {:?}", t.id);
                    scan.prioritize(t.id);
                }
                Ok(true)
            }
        }
    }

    // ---- read-only snapshots ----

    /// All result ids, cheap (no store hits) — for cursor bounds and
    /// `Command::PlayContext`, which needs the whole list, not just what's
    /// currently visible.
    pub fn results_ids(&self) -> Vec<TrackId> {
        self.view.results_ids()
    }

    /// Cheap count of the resolved search results, with no clone — for the
    /// Search screen's "no results for X" check and title.
    pub fn results_len(&self) -> usize {
        self.view.results_len()
    }

    /// A window of the resolved search results (`offset..offset+limit`) —
    /// for rendering just the visible slice instead of cloning the whole
    /// (already in-memory, but still O(n)) results cache every redraw.
    pub fn results_window(&self, offset: usize, limit: usize) -> Vec<Track> {
        self.view.results_window(offset, limit)
    }

    pub fn results_query(&self) -> Option<&str> {
        self.view.results_query()
    }

    /// Append `tid` to the search results, deduped.
    fn push_result(&mut self, tid: TrackId) {
        self.view.push_result(tid, &self.store);
    }

    /// Patch any copy of `id` already sitting in a view cache (search
    /// results, a remote playlist) so a `TrackUpdated` (linking/unlinking,
    /// BPM analysis, ...) doesn't leave the UI showing stale data for it
    /// until the list is reloaded from scratch.
    fn refresh_cached_track(&mut self, id: TrackId) {
        self.view.refresh_cached_track(id, &self.store);
    }

    /// All ids on the Queue screen, in display order: whatever's currently
    /// playing (if any) first, then the upcoming queue. The playing track
    /// stays here — and thus at the top of the screen — until it actually
    /// finishes and playback advances past it, not the moment it starts.
    pub fn queue_ids(&self) -> Vec<TrackId> {
        self.queue.get_current().into_iter().chain(self.queue.snapshot()).collect()
    }

    /// Cheap count of `queue_ids`, for the Queue screen's title.
    pub fn queue_len(&self) -> usize {
        self.queue.get_current().is_some() as usize + self.queue.len()
    }

    /// A window of `queue_ids` (`offset..offset+limit`) — only that slice
    /// gets resolved against the store, same shape as `playing_context_window`.
    pub fn queue_window(&self, offset: usize, limit: usize) -> Vec<Track> {
        let ids = self.queue_ids();
        let window: Vec<TrackId> = ids.into_iter().skip(offset).take(limit).collect();
        self.load_tracks(&window)
    }

    /// All history ids, most-recently-played first, cheap (no store hits) —
    /// for cursor bounds on the `:hist` screen.
    pub fn history_ids(&self) -> Vec<TrackId> {
        let mut ids = self.queue.history_snapshot();
        ids.reverse();
        ids
    }

    /// A most-recent-first window of the bounded play history
    /// (`offset..offset+limit`) — for the `:hist` screen.
    pub fn history_window(&self, offset: usize, limit: usize) -> Vec<Track> {
        self.load_tracks(&self.queue.history_window(offset, limit))
    }

    pub fn playlists(&self) -> Vec<Playlist> {
        self.store
            .all_playlists()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| {
                p.id != QUEUE_PLAYLIST_ID
                    && p.id != HISTORY_PLAYLIST_ID
                    && p.id != NOW_PLAYING_PLAYLIST_ID
            })
            .collect()
    }

    /// Persist the current queue so it survives a restart (restored by
    /// `Session::new`). Called on exit — see `main.rs`.
    pub fn save_queue(&self) {
        let items = self.queue.snapshot();
        let _ = self.store.upsert_playlist(&Playlist {
            id: QUEUE_PLAYLIST_ID,
            name: "__queue__".to_string(),
            notes: String::new(),
            items,
        });
    }

    /// Persist `self.context` (the Now Playing screen's contents) so a fresh
    /// launch reopens the same view — restored by `Session::new`. Called
    /// whenever `self.context` changes (a new `Command::PlayContext`, or
    /// `play_next_in_context` advancing its index), same cadence as
    /// `append_history_entry`. `remote` isn't persisted — restoring it would
    /// need a live source reconnect that's out of scope here (this restores
    /// the view, not playback); a restored context just stops
    /// re-checking a still-loading remote list for more tracks until replayed.
    fn save_now_playing_context(&self) {
        let Some(ctx) = self.context.as_ref() else {
            return;
        };
        let _ = self.store.upsert_playlist(&Playlist {
            id: NOW_PLAYING_PLAYLIST_ID,
            name: ctx.name.clone().unwrap_or_default(),
            notes: ctx.index.to_string(),
            items: ctx.tracks.clone(),
        });
    }

    /// The best line for a foreign M3U player to play `track` back with:
    /// the resolver's normal pick, or — if every one of `track`'s own
    /// renditions is a gap (e.g. it was only ever played via the local-cache
    /// fallback) — whichever rendition's `MediaCache` file exists, so a
    /// track a fallback play actually got audio from doesn't vanish from an
    /// export/history entry just because none of its *real* renditions
    /// resolve. Shared by `export_m3u_to` and `append_history_entry` so both
    /// treat "nothing exportable" the same way.
    fn export_primary_uri(&self, track: &Track) -> Option<String> {
        match Resolver::resolve_track(track, &Target::ExportM3u) {
            Resolution::Ready(r) => Some(r.uri),
            Resolution::Gap { .. } => track
                .renditions
                .iter()
                .find_map(|r| self.media_cache.cached_path(&r.source, &r.uri))
                .map(|p| p.display().to_string()),
        }
    }

    /// Append one play-history record to `history_path`: full track
    /// metadata in the `#MEDLEY-*` tags (same shape `export_m3u_to` writes),
    /// plus `#MEDLEY-PLAYED-AT` for when this play started. Called right
    /// after `Queue::record_played` accepts a new (non-duplicate) entry, so
    /// this is the only place that ever writes the file — see the
    /// `history_path` doc comment for why that's enough to make it the sole
    /// writer even without an extra lock. `played`: the rendition playback
    /// actually started from (possibly the synthetic cache fallback) — used
    /// as a last-resort primary line so a track that really did play never
    /// silently drops out of history just because `export_primary_uri`
    /// itself comes up empty (e.g. its cache entry got evicted between the
    /// play starting and this write).
    fn append_history_entry(&self, track: &Track, played_at: DateTime<Utc>, played: &Rendition) {
        let primary = self.export_primary_uri(track).unwrap_or_else(|| played.uri.clone());
        let mut entry = build_entry(track, primary);
        entry.played_at = Some(played_at);

        let is_new = !self.history_path.exists();
        let mut text = String::new();
        if is_new {
            text.push_str(&write_header(&PlaylistMeta {
                id: None,
                name: "__history__".to_string(),
                notes: String::new(),
            }));
        }
        text.push_str(&write_entry(&entry));

        use std::io::Write;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path);
        match file.and_then(|mut f| f.write_all(text.as_bytes())) {
            Ok(()) => {}
            Err(e) => log::warn!(
                "history: failed to append to {}: {e}",
                self.history_path.display()
            ),
        }
    }

    /// All of a local playlist's track ids, cheap (no per-track store hits —
    /// `Playlist.items` comes back as part of the one playlist read) — for
    /// cursor bounds and `Command::PlayContext`.
    pub fn playlist_track_ids(&self, id: PlaylistId) -> Vec<TrackId> {
        self.store.get_playlist(id).ok().flatten().map(|p| p.items).unwrap_or_default()
    }

    /// Cheap count of a local playlist's tracks, for its title.
    pub fn playlist_len(&self, id: PlaylistId) -> usize {
        self.store
            .get_playlist(id)
            .ok()
            .flatten()
            .map(|p| p.items.len())
            .unwrap_or(0)
    }

    /// A window of a local playlist's tracks (`offset..offset+limit`) — only
    /// that slice gets resolved against the store.
    pub fn playlist_window(&self, id: PlaylistId, offset: usize, limit: usize) -> Vec<Track> {
        let Some(p) = self.store.get_playlist(id).ok().flatten() else {
            return vec![];
        };
        let window: Vec<TrackId> = p.items.iter().skip(offset).take(limit).copied().collect();
        self.load_tracks(&window)
    }

    /// The whole track list `Command::PlayContext` last started playing
    /// from (a playlist, search results, Liked Songs, the queue, ...) — the
    /// Now Playing screen's data source. Stays put once set: it's not
    /// cleared by pausing/stopping, only replaced by the next
    /// `Command::PlayContext`, so it still reflects the *last* played list
    /// even with nothing currently playing. Empty before anything has ever
    /// been played this session. For cursor bounds and `Command::PlayContext`
    /// itself, cheap (no store hits) — see `playing_context_window` for the
    /// resolved slice.
    pub fn playing_context_ids(&self) -> Vec<TrackId> {
        self.context.as_ref().map(|c| c.tracks.clone()).unwrap_or_default()
    }

    /// Cheap count of `playing_context_ids`, for the Now Playing screen's title.
    pub fn playing_context_len(&self) -> usize {
        self.context.as_ref().map(|c| c.tracks.len()).unwrap_or(0)
    }

    /// The originating context's display name, for the Now Playing screen's
    /// title — `None` before anything's played, or if the call site that
    /// issued `Command::PlayContext` had no natural name for it.
    pub fn playing_context_name(&self) -> Option<String> {
        self.context.as_ref().and_then(|c| c.name.clone())
    }

    /// A window of the playing context's tracks (`offset..offset+limit`) —
    /// only that slice gets resolved against the store, same shape as
    /// `playlist_window`.
    pub fn playing_context_window(&self, offset: usize, limit: usize) -> Vec<Track> {
        let ids = self.playing_context_ids();
        let window: Vec<TrackId> = ids.into_iter().skip(offset).take(limit).collect();
        self.load_tracks(&window)
    }

    /// Every registered source id, sorted for a stable display order.
    pub fn source_ids(&self) -> Vec<SourceId> {
        let mut ids: Vec<SourceId> = self.sources.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Borrows this session's source/store/catalog/bus for the
    /// `view_cache::RemoteCtx`-taking `ViewCache` methods below.
    fn remote_ctx(&self) -> RemoteCtx<'_> {
        RemoteCtx {
            sources: &self.sources,
            store: &self.store,
            catalog: &self.catalog,
            bus: &self.bus,
        }
    }

    /// A source's top-level playlist folders that have landed so far — a pure read, never fetches.
    pub fn remote_playlists(&self, source: &SourceId) -> Vec<(String, BrowseNode)> {
        self.view.remote_playlists(source)
    }

    /// Kicks a single-flight background folder fetch for every registered source.
    pub fn ensure_remote_playlists(&self) {
        let ctx = self.remote_ctx();
        for source in self.source_ids() {
            self.view.ensure_remote_playlists(&source, ctx);
        }
    }

    /// All of a remote playlist's ingested track ids, cheap (reads straight
    /// off the already-resolved cache, no store hits) — for cursor bounds
    /// and `Command::PlayContext`.
    pub fn remote_playlist_track_ids(&self, source: &SourceId, node: &BrowseNode) -> Vec<TrackId> {
        self.view.remote_playlist_track_ids(source, node, self.remote_ctx())
    }

    /// Whether `node` (a browse folder under `source`) is a synthetic,
    /// not-a-real-playlist entry — e.g. Spotify's "Liked Songs". Used to
    /// exclude such folders from the hotkey-playlists column even if one
    /// happens to have a hotkey bound to it.
    pub fn is_synthetic_playlist(&self, source: &SourceId, node: &BrowseNode) -> bool {
        self.sources.get(source).map(|s| s.is_synthetic(node)).unwrap_or(false)
    }

    /// Cheap count of a remote playlist's ingested tracks so far.
    pub fn remote_playlist_len(&self, source: &SourceId, node: &BrowseNode) -> usize {
        self.view.remote_playlist_len(source, node, self.remote_ctx())
    }

    /// Is `source`'s top-level playlist-folder list still landing?
    pub fn remote_playlists_loading(&self, source: &SourceId) -> bool {
        self.view.remote_playlists_loading(source)
    }

    /// Is more of this remote playlist still landing? (see `ViewCache::remote_playlist_loading`)
    pub fn remote_playlist_loading(&self, source: &SourceId, node: &BrowseNode) -> bool {
        self.view.remote_playlist_loading(source, node)
    }

    /// A window of a remote playlist's ingested tracks (`offset..offset+limit`)
    /// — only that slice is cloned out of the cache, never the whole thing.
    pub fn remote_playlist_window(
        &self,
        source: &SourceId,
        node: &BrowseNode,
        offset: usize,
        limit: usize,
    ) -> Vec<Track> {
        self.view.remote_playlist_window(source, node, offset, limit, self.remote_ctx())
    }

    pub fn now_playing(&self) -> Option<Track> {
        self.now_playing
            .and_then(|id| self.store.get_track(id).ok().flatten())
    }

    /// Just the id, with no store round-trip — for comparing against a row
    /// already in hand (`RowItem::is_current`) instead of re-resolving the
    /// whole now-playing `Track` (a disk read) once per row on every redraw.
    pub fn now_playing_id(&self) -> Option<TrackId> {
        self.now_playing
    }

    /// Whether queue shuffle is currently on (`s`/`:toggleshuffle`) — for
    /// the status line's shuffle indicator.
    pub fn shuffle(&self) -> bool {
        self.queue.get_shuffle()
    }

    /// All current playlist hotkeys, `(key, target)`.
    pub fn hotkeys(&self) -> Vec<(char, HotkeyTarget)> {
        self.hotkeys.iter().map(|(&k, p)| (k, p.clone())).collect()
    }

    /// The target bound to `key`, if any.
    pub fn hotkey_for(&self, key: char) -> Option<HotkeyTarget> {
        self.hotkeys.get(&key).cloned()
    }

    /// The key bound to `target`, if any.
    pub fn playlist_hotkey(&self, target: &HotkeyTarget) -> Option<char> {
        self.hotkeys.iter().find(|&(_, p)| p == target).map(|(&k, _)| k)
    }

    /// The key that actually activates `target` right now: its explicit
    /// table entry if any, else — for a `Builtin` target only, since a
    /// playlist has no key until one is bound — its hardcoded default. Once
    /// a builtin is explicitly remapped away, this stops falling back (its
    /// entry then lives at the new key instead), so a vacated default key
    /// reads as free rather than still "belonging" to the old action.
    pub fn effective_hotkey(&self, target: &HotkeyTarget) -> Option<char> {
        self.playlist_hotkey(target).or_else(|| match target {
            HotkeyTarget::Builtin(action) => Some(action.default_key()),
            _ => None,
        })
    }

    /// Binds `key` to `target`, dropping `target`'s previous key and stealing `key` from any
    /// other playlist; returns who it was stolen from. Refuses a built-in's key and a
    /// synthetic target — see `BindError`.
    pub fn bind_hotkey(
        &mut self,
        key: char,
        target: HotkeyTarget,
    ) -> std::result::Result<Option<HotkeyTarget>, BindError> {
        self.touch_lists();
        if let HotkeyTarget::Remote(source, node) = &target
            && self.is_synthetic_playlist(source, node)
        {
            return Err(BindError::SyntheticPlaylist);
        }
        let stolen = bind_hotkey(&mut self.hotkeys, key, target).map_err(BindError::BuiltinKey)?;
        self.invalidate_hotkey_memberships();
        Ok(stolen)
    }

    /// Clears `target`'s hotkey, if it has one.
    pub fn unbind_hotkey(&mut self, target: &HotkeyTarget) {
        self.touch_lists();
        self.hotkeys.retain(|_, p| p != target);
        self.invalidate_hotkey_memberships();
    }

    /// Replaces the whole hotkey map — `app` calls this once at startup with what `state.toml` persisted.
    pub fn set_hotkeys(&mut self, hotkeys: HashMap<char, HotkeyTarget>) {
        self.touch_lists();
        self.hotkeys = hotkeys;
        self.prune_synthetic_hotkeys();
    }

    /// Drops bindings to synthetic nodes; re-run as sources register, since only a wired source can tell.
    fn prune_synthetic_hotkeys(&mut self) {
        let sources = &self.sources;
        let bound = self.hotkeys.len();
        self.hotkeys.retain(|key, target| {
            let synthetic = matches!(target, HotkeyTarget::Remote(sid, node)
                if sources.get(sid).is_some_and(|s| s.is_synthetic(node)));
            if synthetic {
                log::warn!("hotkey '{key}': dropped, bound to a synthetic playlist");
            }
            !synthetic
        });
        if self.hotkeys.len() != bound {
            self.invalidate_hotkey_memberships();
        }
    }

    /// Sets a source's persisted `enabled` bit; takes effect next restart, like editing config.toml.
    pub fn set_source_enabled(&mut self, source: &str, enabled: bool) {
        self.touch();
        Arc::make_mut(&mut self.cfg).set_source_enabled(source, enabled);
    }

    /// Live scan on/off — reaches `ScanMode::Disabled`, which `B` deliberately never does.
    pub fn set_scan_enabled(&mut self, enabled: bool) {
        self.touch();
        if let Some(scan) = &self.scan {
            scan.set_mode(if enabled { crate::scan::ScanMode::CacheOnly } else { crate::scan::ScanMode::Disabled });
        }
    }

    /// Latest async remote-playlist-toggle result, if any — see
    /// `membership_feedback`'s field doc.
    pub fn membership_feedback(&self) -> Option<String> {
        self.membership_feedback.lock().unwrap().clone()
    }

    /// Clears the async toggle-result message — called when the hotkey menu
    /// (re)opens, mirroring how the UI's own `hotkey_feedback` is cleared.
    pub fn clear_membership_feedback(&mut self) {
        if self.membership_feedback.lock().unwrap().take().is_some() {
            self.touch();
        }
    }

    /// Whether pressing play on `track` right now would be instant — any of
    /// its renditions already has a `MediaCache` entry (the same lookup
    /// `play_from_cache` itself does), so the `*` badge never disagrees with
    /// what playback can actually use.
    pub fn is_track_cached(&self, track: &Track) -> bool {
        track.renditions.iter().any(|r| self.media_cache.cached_path(&r.source, &r.uri).is_some())
    }

    pub fn player_status(&self) -> PlayerStatus {
        let mut s = self.last_status.clone();
        s.volume = self.volume;
        s
    }

    /// Real 5-band magnitude of whatever's actually playing — see
    /// `Player::levels`. Silence if nothing's loaded or the active player
    /// doesn't expose real audio (e.g. Spotify).
    pub fn audio_levels(&self) -> [f32; 5] {
        self.active_player().map(|p| p.levels()).unwrap_or([0.0; 5])
    }

    /// Pasted raw URI/URL: first source whose `recognizes()` returns true.
    pub fn source_for_uri(&self, uri: &str) -> Option<&Arc<dyn Source>> {
        self.sources.values().find(|s| s.recognizes(uri))
    }

    // ---- internals ----

    /// `record`: log `id` into the bounded play history — true for forward
    /// plays, false when replaying a history entry itself (walking further
    /// back via `previous_from_history` must not re-add what it just read).
    fn play_track(&mut self, id: TrackId, record: bool) {
        // Any fresh play call supersedes an outstanding decode-on-demand from
        // a previous one — see `pending_cache_fallback`'s doc.
        self.pending_cache_fallback = None;
        self.failed_playback_sources.clear();
        let Some(track) = self.store.get_track(id).ok().flatten() else {
            return;
        };
        match Resolver::resolve_track(&track, &Target::Playback) {
            Resolution::Ready(r) => {
                if let Some(p) = self.pick_player(&r) {
                    self.start_playback(&track, &r, p, record);
                } else if !self.play_from_cache(&track, record) {
                    let message = format!("no player registered for {}", r.source);
                    self.bus.send(CoreEvent::SourceError {
                        source: r.source.clone(),
                        message,
                    });
                }
            }
            Resolution::Gap { reason } => {
                if !self.play_from_cache(&track, record) {
                    log::warn!("play_track gap: {reason}");
                }
            }
        }
    }

    /// Actually start `p` playing `r` for `track` — the common tail of a
    /// normal resolve and a `play_from_cache` fallback.
    fn start_playback(&mut self, track: &Track, r: &Rendition, p: Arc<dyn Player>, record: bool) {
        // Only one player should ever be producing audio. `load` replaces
        // the sink within `p` itself, but a track can route to a *different*
        // `Player` instance than the one currently playing (e.g. Spotify vs.
        // the shared Rodio player) — stop everyone else first so it doesn't
        // keep playing underneath the new track. `Player::stop` blocks until
        // that player has actually gone silent (not just posted a Stop
        // command) — the two independent audio backends racing here (one
        // still tearing down while the other starts) was the rare
        // simultaneous-double-playback bug.
        for other in self.players.values() {
            if !Arc::ptr_eq(other, &p) {
                other.stop();
            }
        }
        p.load(r, false, 0, true);
        self.now_playing = Some(track.id);
        self.last_status = p.status();
        self.now_playing_player = Some(p);
        self.last_status.volume = self.volume;
        if record && let Some(played_at) = self.queue.record_played(track.id) {
            self.append_history_entry(track, played_at, r);
            self.touch_lists();
        }
    }

    /// Playback fallback: if any of `track`'s renditions already has a
    /// `MediaCache` entry, play it straight away through the `"local"`
    /// source (see `cache_rendition`) and return `true`. Otherwise, check
    /// whether a source can hand us already-local
    /// bytes without touching the network (`ScanFetchMode::CacheOnly` — e.g.
    /// Spotify's own librespot file cache from a now-dead live session), and
    /// if so decode+cache them on a background thread, retrying once that
    /// lands (`CoreEvent::CacheFallbackReady`). Returns `false` (with
    /// nothing left to try) when neither has anything.
    fn play_from_cache(&mut self, track: &Track, record: bool) -> bool {
        if let Some(r) = cache_rendition(&self.media_cache, track) {
            if let Some(p) = self.pick_player(&r) {
                self.start_playback(track, &r, p, record);
                return true;
            }
            return false;
        }
        self.spawn_cache_fallback_decode(track.clone());
        false
    }

    /// Background half of `play_from_cache`'s decode-on-demand path — see
    /// its doc. Populates `self.media_cache` for `track` from whatever's
    /// already locally cached (no network), then wakes `on_event` via
    /// `CoreEvent::CacheFallbackReady` to retry playback.
    fn spawn_cache_fallback_decode(&mut self, track: Track) {
        self.pending_cache_fallback = Some(track.id);
        let media = self.media.clone();
        let players = self.players.clone();
        let media_cache = self.media_cache.clone();
        let bus = self.bus.clone();
        std::thread::spawn(move || {
            let found = track.renditions.iter().find_map(|r| {
                crate::scan::open_scan_audio(r, &media, &players, &media_cache, crate::scan::ScanFetchMode::CacheOnly)
                    .map(|audio| (r, audio))
            });
            let Some((r, audio)) = found else {
                log::debug!(
                    "play_from_cache: \"{}\" has no already-cached audio to decode",
                    track.title
                );
                return;
            };
            match crate::audio_decode::decode_and_cache(&media_cache, &r.source, &r.uri, audio) {
                Some(_) => bus.send(CoreEvent::CacheFallbackReady(track.id)),
                None => log::debug!("play_from_cache: decode failed for \"{}\"", track.title),
            }
        });
    }

    /// Start playing `id` right now. If it's already sitting in the manual
    /// queue, pull it out of line first — `current` and the upcoming queue
    /// are fully independent, so nothing needs "replacing in place" the way
    /// the old index-into-one-Vec shape did.
    fn play_now(&mut self, id: TrackId) {
        self.queue.remove_track(id);
        self.queue.set_current(Some(id));
        self.play_track(id, true);
    }

    /// Decide, and start, whatever plays next — used both for the manual
    /// `Command::Next` (`manual: true`) and for a track finishing on its own
    /// (`manual: false`). Priority, strictly in this order:
    /// 1. `RepeatTrack`, but only on auto-advance (`!manual`) — replay the
    ///    current track instead of consuming anything. A manual `Next`
    ///    always moves on and drops back to `RepeatPlaylist` (unaffected by
    ///    this queue redesign — matches the previous behavior exactly).
    /// 2. The manual queue's front item, unconditionally — anything
    ///    `q`-enqueued always plays before the context below continues, and
    ///    since `current`/`queue` no longer share one index this can't
    ///    desync from what's actually queued.
    /// 3. `PlaybackContext` — the list the current track was played from
    ///    (search results, a playlist, Liked Songs) continues.
    /// 4. Nothing left either way: stop.
    fn advance(&mut self, manual: bool) {
        let repeat = self.queue.get_repeat();
        if repeat == RepeatSetting::RepeatTrack {
            if manual {
                self.queue.set_repeat(RepeatSetting::RepeatPlaylist);
            } else if let Some(id) = self.queue.get_current() {
                self.play_track(id, true);
                return;
            }
        }
        if let Some(id) = self.queue.pop_front() {
            self.queue.set_current(Some(id));
            self.play_track(id, true);
            return;
        }
        if self.play_next_in_context() {
            return;
        }
        self.queue.stop();
    }

    /// Play the next track in `self.context`, if there is one, advancing its
    /// index. Returns `false` (and leaves `self.context` untouched) when
    /// there's no context or it's already at its end. Under shuffle, "next"
    /// is drawn from a no-repeat shuffle bag over `ctx.tracks` instead of
    /// `ctx.index + 1` — see `next_shuffled_context_index`. Shuffle never
    /// touches the manual queue (`Session::queue`), only this context-driven
    /// path, so an ad-hoc/manually-queued track list always advances in
    /// order regardless of the shuffle setting.
    fn play_next_in_context(&mut self) -> bool {
        let Some(next_index) = self.next_context_index() else {
            return false;
        };
        let shuffle = self.queue.get_shuffle();
        let ctx = self.context.as_mut().unwrap();
        if shuffle {
            ctx.shuffle_bag.pop();
        }
        let next_id = ctx.tracks[next_index];
        ctx.index = next_index;
        self.save_now_playing_context();
        self.play_now(next_id);
        true
    }

    /// The index `play_next_in_context` moves to next, without consuming
    /// it. Under shuffle that's the top of the bag, dealt here when empty so
    /// an early `upcoming_track` and the later advance agree.
    fn next_context_index(&mut self) -> Option<usize> {
        let ctx = self.context.as_ref()?;
        if ctx.tracks.is_empty() {
            return None;
        }
        if self.queue.get_shuffle() {
            self.deal_shuffle_bag();
            return self.context.as_ref()?.shuffle_bag.last().copied();
        }
        let next_index = ctx.index + 1;
        // The snapshot ended, but if it came from a remote node still
        // loading in the background, re-check the live list before
        // giving up — it may have grown past what was captured at play
        // time.
        if ctx.tracks.get(next_index).is_none()
            && let Some((source, node)) = ctx.remote.clone()
        {
            let live = self.remote_playlist_track_ids(&source, &node);
            let ctx = self.context.as_mut().unwrap();
            if live.len() > ctx.tracks.len() {
                ctx.tracks = live;
            }
        }
        let ctx = self.context.as_ref().unwrap();
        ctx.tracks.get(next_index).map(|_| next_index)
    }

    /// Refill an exhausted `ctx.shuffle_bag` with a fresh shuffle of every
    /// index in `ctx.tracks` — a no-repeat shuffle bag, so every track in
    /// the context gets a turn before any repeats. Excludes the
    /// currently-playing index from a freshly-dealt bag when there's more
    /// than one track, so refilling never immediately replays what just
    /// finished.
    fn deal_shuffle_bag(&mut self) {
        let ctx = self.context.as_mut().unwrap();
        if ctx.shuffle_bag.is_empty() {
            let mut bag: Vec<usize> = (0..ctx.tracks.len()).collect();
            bag.shuffle(&mut rand::rng());
            if bag.len() > 1
                && let Some(pos) = bag.iter().position(|&i| i == ctx.index)
            {
                bag.swap(pos, 0);
            }
            ctx.shuffle_bag = bag;
        }
    }

    /// What `advance(false)` will play, decided without starting it.
    fn upcoming_track(&mut self) -> Option<TrackId> {
        if self.queue.get_repeat() == RepeatSetting::RepeatTrack
            && let Some(id) = self.queue.get_current()
        {
            return Some(id);
        }
        if let Some(&id) = self.queue.window(0, 1).first() {
            return Some(id);
        }
        let index = self.next_context_index()?;
        Some(self.context.as_ref()?.tracks[index])
    }

    /// Warm the upcoming track on the active player, if that's who will play it.
    fn preload_upcoming(&mut self) {
        let Some(track) = self.upcoming_track().and_then(|id| self.store.get_track(id).ok().flatten()) else {
            return;
        };
        if let Resolution::Ready(r) = Resolver::resolve_track(&track, &Target::Playback)
            && let (Some(next), Some(active)) = (self.pick_player(&r), self.active_player())
            && Arc::ptr_eq(&next, &active)
        {
            active.preload(&r);
        }
    }

    fn active_player(&self) -> Option<Arc<dyn Player>> {
        self.now_playing_player.clone()
    }

    /// Route a rendition to a player: prefer the one registered under its
    /// `source`, but honour [`Player::accepts`] — if that player is missing or
    /// rejects the rendition, fall back to the first registered player that
    /// accepts it.
    fn pick_player(&self, r: &Rendition) -> Option<Arc<dyn Player>> {
        if let Some(p) = self.players.get(&r.source)
            && p.accepts(r)
        {
            return Some(p.clone());
        }
        self.players.values().find(|p| p.accepts(r)).cloned()
    }

    fn refresh_status(&mut self) {
        if let Some(p) = self.active_player() {
            self.last_status = p.status();
        }
        self.last_status.volume = self.volume;
    }

    fn load_tracks(&self, ids: &[TrackId]) -> Vec<Track> {
        ids.iter()
            .filter_map(|id| self.store.get_track(*id).ok().flatten())
            .collect()
    }

    /// Resolves `ids` fresh — for a UI cache (e.g. the local filter) that holds ids, not `Track`s.
    pub fn tracks_for(&self, ids: &[TrackId]) -> Vec<Track> {
        self.load_tracks(ids)
    }

    /// Build an [`M3uDoc`] from the resolver, [`write_m3u`] it, and write the
    /// text atomically to `path`.
    fn export_m3u_to(&self, id: PlaylistId, path: PathBuf) -> Result<Dispatch> {
        let p = self.store.get_playlist(id)?.ok_or(Error::NotFound)?;
        let tracks = self.load_tracks(&p.items);

        let mut entries: Vec<M3uEntry> = Vec::new();
        let mut gaps: Vec<String> = Vec::new();
        for tid in &p.items {
            let Some(t) = tracks.iter().find(|t| t.id == *tid) else {
                gaps.push(format!("track {} not in catalog", tid.0));
                continue;
            };
            match self.export_primary_uri(t) {
                Some(uri) => entries.push(build_entry(t, uri)),
                None => gaps.push(format!("nothing exportable for \"{}\"", t.title)),
            }
        }
        let n = entries.len();

        let doc = M3uDoc {
            playlist: PlaylistMeta {
                id: Some(p.id.0),
                name: p.name.clone(),
                notes: p.notes.clone(),
            },
            entries,
        };
        let text = write_m3u(&doc);

        // atomic-ish: write a unique "<path>.<pid>.<nanos>.tmp" then rename over
        // "<path>". The temp name is unique so two concurrent exports of the
        // same playlist can't collide, and it is removed on any error so the
        // failure path never leaks it (M2b bugfix).
        let tmp = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut s = path.clone().into_os_string();
            s.push(format!(".{}.{}.tmp", std::process::id(), nanos));
            PathBuf::from(s)
        };
        if let Err(e) = std::fs::write(&tmp, text.as_bytes()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Other(e.to_string()));
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Other(e.to_string()));
        }

        let disp = path.display();
        log::info!("export_m3u: {disp} — {n} track(s), {} skipped", gaps.len());
        if gaps.is_empty() {
            Ok(Dispatch::Modal(format!("exported {disp} ({n} tracks)")))
        } else {
            Ok(Dispatch::Modal(format!(
                "exported {disp} ({n} tracks, {} skipped): {}",
                gaps.len(),
                gaps.join("; ")
            )))
        }
    }

    /// Parse an enriched-M3U file and fold its tracks + playlist into the
    /// catalog.
    fn import_m3u(&mut self, path: PathBuf) -> Result<Dispatch> {
        let text = std::fs::read_to_string(&path).map_err(|e| Error::Other(e.to_string()))?;
        let doc = parse_m3u(&text)?;
        // The synthesised `uri` is promised to be absolute. `path` may be a bare
        // filename (parent == ""), which would resolve relative primary lines
        // against the process CWD. Resolve `base_dir` to an absolute directory
        // first.
        let base_dir: PathBuf = abs_parent_dir(&path);

        let name = if doc.playlist.name.trim().is_empty() {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "playlist".to_string())
        } else {
            doc.playlist.name.clone()
        };

        let (mut new_c, mut merged_c, mut skipped) = (0usize, 0usize, 0usize);
        let mut items: Vec<TrackId> = Vec::new();

        for entry in &doc.entries {
            // 2. rendition tuples: start from the #MEDLEY-RENDITION lines.
            let mut specs: Vec<RenditionSpec> =
                entry.renditions.iter().map(spec_from_parsed).collect();

            // 2b. reconcile the primary URI.
            let known = specs.iter().any(|s| s.uri == entry.primary_uri);
            if !known && !entry.primary_uri.is_empty() {
                specs.push(self.synthesise_rendition(&entry.primary_uri, &base_dir));
            }
            if specs.is_empty() {
                skipped += 1;
                continue;
            }

            // 3. title / artists / duration.
            let (artists, title) = match &entry.extinf {
                Some((_, t)) if !t.trim().is_empty() => parse_artist_title(t),
                _ => parse_artist_title(&uri_stem(&entry.primary_uri)),
            };
            let dur_ms = entry
                .extinf
                .as_ref()
                .map(|(s, _)| if *s < 0 { 0 } else { (*s as u32).saturating_mul(1000) })
                .unwrap_or(0);

            let before: HashSet<TrackId> =
                self.store.all_tracks()?.iter().map(|t| t.id).collect();

            let mut canonical: Option<TrackId> = None;
            for spec in &specs {
                let hit = SearchHit {
                    source: SourceId::from(spec.source.as_str()),
                    uri: spec.uri.clone(),
                    title: title.clone(),
                    artists: artists.clone(),
                    duration_ms: dur_ms,
                    isrc: entry.meta.isrc.clone(),
                    album: entry.meta.album.clone(),
                    quality: spec.quality.clone(),
                };
                let tid = self.catalog.ingest(hit)?;
                canonical.get_or_insert(tid);
            }
            let Some(canonical) = canonical else {
                skipped += 1;
                continue;
            };
            if before.contains(&canonical) {
                merged_c += 1;
            } else {
                new_c += 1;
            }

            // 4. patch soft metadata (never overwrite what the catalog has).
            let m = entry.meta.clone();
            self.catalog.patch(canonical, |t| {
                if t.year.is_none() {
                    t.year = m.year;
                }
                if let Some(v) = m.bpm {
                    t.attrs.entry("bpm".into()).or_insert_with(|| format!("{v:.0}"));
                }
                if let Some(v) = &m.key {
                    t.attrs.entry("key".into()).or_insert_with(|| v.clone());
                }
                if t.isrc.is_none() {
                    t.isrc = m.isrc.clone();
                }
                if t.album.is_none() {
                    t.album = m.album.clone();
                }
                for tag in &m.tags {
                    if !t.tags.contains(tag) {
                        t.tags.push(tag.clone());
                    }
                }
            })?;

            // 5. collect in entry order (dedup: keep first position).
            if !items.contains(&canonical) {
                items.push(canonical);
            }
        }

        // 6. playlist upsert.
        let pid = match doc.playlist.id {
            Some(uuid) => {
                let pid = PlaylistId(uuid);
                let mut pl = self.store.get_playlist(pid)?.unwrap_or(Playlist {
                    id: pid,
                    name: name.clone(),
                    notes: doc.playlist.notes.clone(),
                    items: vec![],
                });
                pl.name = name.clone();
                pl.notes = doc.playlist.notes.clone();
                pl.items = items.clone();
                self.save_playlist(&pl)?;
                pid
            }
            None => {
                let pl = Playlist {
                    id: PlaylistId::new(),
                    name: name.clone(),
                    notes: doc.playlist.notes.clone(),
                    items: items.clone(),
                };
                let pid = pl.id;
                self.save_playlist(&pl)?;
                pid
            }
        };

        let _ = pid;

        log::info!(
            "import_m3u: \"{name}\" — {} track(s) ({new_c} new, {merged_c} merged, {skipped} skipped)",
            items.len()
        );
        Ok(Dispatch::Modal(format!(
            "imported \"{name}\": {} tracks ({new_c} new, {merged_c} merged, {skipped} skipped)",
            items.len()
        )))
    }

    /// `:add <path>...` — fold local audio files into the catalog and append the
    /// resulting tracks to `playlist`, or to the queue when `playlist` is
    /// `None` (no playlist open/selected — added to the queue instead of
    /// erroring, see `TogglePlaylistMembership`'s "no dead ends" precedent).
    /// Paths resolve against the process CWD; non-audio paths and missing
    /// files are skipped (and logged).
    fn add_files_to_playlist(
        &mut self,
        playlist: Option<PlaylistId>,
        paths: Vec<PathBuf>,
    ) -> Result<Dispatch> {
        let mut pl = playlist
            .map(|id| self.store.get_playlist(id)?.ok_or(Error::NotFound))
            .transpose()?;
        let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let (mut added, mut skipped) = (0usize, 0usize);

        for path in &paths {
            let raw = path.to_string_lossy();
            let spec = self.synthesise_rendition(&raw, &base_dir);
            if spec.source != "local" {
                log::warn!("add: not a local audio file, skipping: {raw}");
                skipped += 1;
                continue;
            }
            if !Path::new(&spec.uri).exists() {
                log::warn!("add: file not found, skipping: {raw}");
                skipped += 1;
                continue;
            }
            let (artists, title) = parse_artist_title(&uri_stem(&spec.uri));
            let tid = self.catalog.ingest(SearchHit {
                source: SourceId::from(spec.source.as_str()),
                uri: spec.uri.clone(),
                title,
                artists,
                duration_ms: 0,
                isrc: None,
                album: None,
                quality: spec.quality.clone(),
            })?;
            match &mut pl {
                Some(pl) => {
                    if pl.items.contains(&tid) {
                        log::info!("add: already in \"{}\", skipping: {raw}", pl.name);
                        skipped += 1;
                        continue;
                    }
                    pl.items.push(tid);
                }
                None => self.queue.append(tid),
            }
            added += 1;
        }

        match pl {
            Some(pl) => {
                if added > 0 {
                    self.save_playlist(&pl)?;
                }
                log::info!("add: \"{}\" — {added} added, {skipped} skipped", pl.name);
                Ok(Dispatch::Modal(if skipped > 0 {
                    format!("added {added} track(s) to \"{}\" ({skipped} skipped)", pl.name)
                } else {
                    format!("added {added} track(s) to \"{}\"", pl.name)
                }))
            }
            None => {
                log::info!("add: queue — {added} added, {skipped} skipped");
                Ok(Dispatch::Modal(if skipped > 0 {
                    format!("queued {added} track(s) ({skipped} skipped)")
                } else {
                    format!("queued {added} track(s)")
                }))
            }
        }
    }

    /// Local playlists flip synchronously in the store; a remote one settles on a background
    /// thread (`ViewCache::toggle_remote_membership`). Both announce via `PlaylistsChanged`.
    fn toggle_playlist_membership(&mut self, track: TrackId, target: HotkeyTarget) -> Result<Dispatch> {
        match target {
            HotkeyTarget::Local(playlist) => {
                let mut p = self.store.get_playlist(playlist)?.ok_or(Error::NotFound)?;
                toggle_membership(&mut p.items, track);
                self.save_playlist(&p)?;
            }
            HotkeyTarget::Remote(source, node) => self.toggle_remote_playlist_membership(track, source, node),
            HotkeyTarget::Builtin(_) => {}
        }
        Ok(Dispatch::Ok)
    }

    fn toggle_remote_playlist_membership(&mut self, track: TrackId, source: SourceId, node: BrowseNode) {
        let Some(t) = self.store.get_track(track).ok().flatten() else {
            return;
        };
        let Some(src) = self.sources.get(&source).cloned() else {
            return;
        };
        let name = t.display_name();
        let refusal = if src.is_synthetic(&node) {
            // A toggle could silently unlike; Liked Songs only changes through Like/Unlike.
            Some(format!("Can't toggle {name:?}: Liked Songs isn't a hotkey playlist — use like/unlike"))
        } else if !t.renditions.iter().any(|r| r.source == source) {
            Some(format!("Can't toggle {name:?}: track isn't on {source}"))
        } else {
            None
        };
        if let Some(msg) = refusal {
            log::error!("toggle_playlist_membership: {msg}");
            *self.membership_feedback.lock().unwrap() = Some(msg);
            return;
        }
        let uri = t.renditions.iter().find(|r| r.source == source).map(|r| r.uri.clone()).unwrap_or_default();
        let feedback = self.membership_feedback.clone();
        let started = self.view.toggle_remote_membership(t, uri, src, node, self.remote_ctx(), move |msg| {
            *feedback.lock().unwrap() = Some(msg);
        });
        if started {
            self.invalidate_hotkey_memberships();
        } else {
            *self.membership_feedback.lock().unwrap() = Some(format!("Still updating {name:?} in that playlist"));
        }
    }

    /// Tracks with an add/remove still in flight on this remote playlist — its rows render as pending.
    pub fn remote_pending_ids(&self, source: &SourceId, node: &BrowseNode) -> Vec<TrackId> {
        self.view.remote_pending_ids(source, node)
    }

    /// The hotkeys column's lookup table, one entry per playlist-bound key. Rebuilt only after a
    /// membership-changing event (`invalidate_hotkey_memberships`), or once per
    /// `HOTKEY_MEMO_LOADING_RECHECK` while a bound remote playlist is still loading — each
    /// rebuild is what nudges that playlist's background load along.
    pub fn hotkey_memberships(&self) -> Arc<Vec<HotkeyMembership>> {
        let mut memo = self.hotkey_memberships.lock().unwrap();
        let stale = memo.dirty || (memo.loading && memo.built.elapsed() >= HOTKEY_MEMO_LOADING_RECHECK);
        if stale {
            let mut loading = false;
            let mut table: Vec<HotkeyMembership> = self
                .hotkeys
                .iter()
                .filter_map(|(&key, target)| match target {
                    HotkeyTarget::Local(id) => Some(HotkeyMembership {
                        key,
                        members: self.playlist_track_ids(*id).into_iter().collect(),
                        pending: HashSet::new(),
                    }),
                    HotkeyTarget::Remote(sid, node) if !self.is_synthetic_playlist(sid, node) => {
                        let members = self.remote_playlist_track_ids(sid, node).into_iter().collect();
                        loading |= self.remote_playlist_loading(sid, node);
                        let pending = self.remote_pending_ids(sid, node).into_iter().collect();
                        Some(HotkeyMembership { key, members, pending })
                    }
                    HotkeyTarget::Remote(..) | HotkeyTarget::Builtin(_) => None,
                })
                .collect();
            table.sort_unstable_by_key(|m| m.key);
            *memo = HotkeyMemo { table: Arc::new(table), dirty: false, loading, built: Instant::now() };
        }
        memo.table.clone()
    }

    /// The one write path for a user playlist, so everything derived from its contents hears about it.
    fn save_playlist(&self, playlist: &Playlist) -> Result<()> {
        self.store.upsert_playlist(playlist)?;
        self.invalidate_hotkey_memberships();
        self.bus.send(CoreEvent::PlaylistsChanged);
        Ok(())
    }

    fn invalidate_hotkey_memberships(&self) {
        self.hotkey_memberships.lock().unwrap().dirty = true;
    }

    /// Every (source, liked/favorites node, rendition uri) triple for `track`
    /// whose source exposes one (`Source::liked_songs_node`) — `set_liked`'s
    /// source resolution, one call per matching rendition. A track with two
    /// renditions on the same source (shouldn't normally happen, but isn't
    /// enforced) only yields one entry for it.
    fn liked_targets(&self, track: &Track) -> Vec<(Arc<dyn Source>, BrowseNode, String)> {
        let mut seen: Vec<SourceId> = Vec::new();
        track
            .renditions
            .iter()
            .filter_map(|r| {
                if seen.contains(&r.source) {
                    return None;
                }
                let src = self.sources.get(&r.source)?;
                let node = src.liked_songs_node()?;
                seen.push(r.source.clone());
                Some((src.clone(), node, r.uri.clone()))
            })
            .collect()
    }

    /// `Command::Like`/`Command::Unlike`'s shared dispatch: auto-discovers
    /// `track`'s liked/favorites target(s) (`liked_targets`) instead of
    /// taking one from a hotkey-bound `HotkeyTarget`, then adds (`like`) or
    /// removes it there on a background thread — otherwise the same
    /// never-block-the-UI-thread, report-through-`membership_feedback`
    /// pattern as `toggle_remote_playlist_membership`.
    fn set_liked(&mut self, track: TrackId, like: bool) -> Result<Dispatch> {
        let Some(t) = self.store.get_track(track).ok().flatten() else {
            return Ok(Dispatch::Ok);
        };
        let targets = self.liked_targets(&t);
        let name = t.display_name();
        if targets.is_empty() {
            let verb = if like { "like" } else { "unlike" };
            let msg = format!("Can't {verb} {name:?}: no liked-songs source for this track");
            log::error!("set_liked: {msg}");
            *self.membership_feedback.lock().unwrap() = Some(msg);
            return Ok(Dispatch::Ok);
        }
        let feedback = self.membership_feedback.clone();
        let bus = self.bus.clone();
        std::thread::spawn(move || {
            for (src, node, uri) in targets {
                let result = if like {
                    src.add_to_playlist(&node, &uri)
                } else {
                    src.remove_from_playlist(&node, &uri)
                };
                let msg = match result {
                    Ok(()) if like => format!("Liked {name:?}"),
                    Ok(()) => format!("Removed {name:?} from Liked Songs"),
                    Err(e) => {
                        log::error!("set_liked[{}]: {e}", src.id());
                        let verb = if like { "like" } else { "unlike" };
                        format!("Can't {verb} {name:?}: {e}")
                    }
                };
                *feedback.lock().unwrap() = Some(msg);
                // No event fired this without a keypress before — the UI only
                // noticed by polling `membership_feedback` on every redraw.
                bus.send(CoreEvent::PlaylistsChanged);
            }
        });
        Ok(Dispatch::Ok)
    }

    /// Turn a bare primary URI with no matching `#MEDLEY-RENDITION` into one
    /// rendition. `"local"` / `"external"` are medley-defined pseudo source
    /// ids (documented exception to the "core never names a source" rule).
    fn synthesise_rendition(&self, primary: &str, base_dir: &Path) -> RenditionSpec {
        // recognised by a registered source.
        if let Some(src) = self.source_for_uri(primary) {
            return RenditionSpec {
                source: src.id().as_str().to_string(),
                uri: primary.to_string(),
                quality: Quality::Unknown,
            };
        }
        // looks like a local path with an audio extension.
        if let Some(abs) = local_path_of(primary, base_dir) {
            let name = abs.to_string_lossy();
            if crate::audio::audio_ext(&name) {
                return RenditionSpec {
                    source: "local".to_string(),
                    uri: abs.display().to_string(),
                    quality: crate::audio::audio_quality(&name),
                };
            }
        }
        // anything else.
        RenditionSpec {
            source: "external".to_string(),
            uri: primary.to_string(),
            quality: Quality::Unknown,
        }
    }
}

/// One rendition candidate, ready to become a [`SearchHit`]. Locality (and,
/// for a local one, its path) lives entirely in `source`/`uri` — see
/// `resolver::is_local_source`/`local_path_from_uri`.
struct RenditionSpec {
    source: String,
    uri: String,
    quality: Quality,
}

fn spec_from_parsed(p: &ParsedRendition) -> RenditionSpec {
    // A local rendition's `uri` is the bare filesystem path going forward;
    // tolerate a legacy/foreign `file://`-prefixed one on read.
    let uri = if p.source == "local" {
        local_path_from_uri(&p.uri).to_string()
    } else {
        p.uri.clone()
    };
    RenditionSpec {
        source: p.source.clone(),
        uri,
        quality: p.quality.clone(),
    }
}

/// Absolute directory that contains `path`, even when `path` doesn't exist yet.
/// Canonicalizes the file if it exists, otherwise canonicalizes the parent dir
/// (falling back to CWD) and joins the file name. Used so `import_m3u` resolves
/// relative primary lines against an absolute base.
fn abs_parent_dir(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        if let Some(parent) = canon.parent() {
            return parent.to_path_buf();
        }
        return canon;
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(p) => std::fs::canonicalize(p).unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(p))
                .unwrap_or_else(|_| p.to_path_buf())
        }),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// Resolve a bare primary line to a local filesystem path, if it looks like one.
fn local_path_of(s: &str, base_dir: &Path) -> Option<PathBuf> {
    if let Some(rest) = s.strip_prefix("file://") {
        return Some(PathBuf::from(rest));
    }
    if s.contains("://") {
        return None; // some other URL scheme
    }
    let p = Path::new(s);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        Some(base_dir.join(p))
    }
}

/// Filename stem of a URI / path, for `parse_artist_title` when `#EXTINF` is
/// absent (same idea as the http source).
fn uri_stem(uri: &str) -> String {
    let last = uri
        .trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(uri);
    match last.rsplit_once('.') {
        Some((name, _)) if !name.is_empty() => name.to_string(),
        _ => last.to_string(),
    }
}

/// Build one [`M3uEntry`] from a track + its chosen primary line.
/// Load play-history entries from the on-disk M3U log at `path`, oldest
/// first (matching the file's append order) — used once at startup by
/// `Session::new`. Tolerates a missing file (nothing played yet, or the very
/// first run) and a malformed one (both just come back empty). An entry with
/// no `#MEDLEY-TRACK-ID` is skipped; one missing `#MEDLEY-PLAYED-AT` (a
/// foreign/hand-edited line) falls back to "now" rather than dropping the
/// whole record.
fn load_history_file(path: &Path) -> Vec<(TrackId, DateTime<Utc>)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = parse_m3u(&text) else {
        return Vec::new();
    };
    doc.entries
        .into_iter()
        .filter_map(|e| e.track_id.map(|tid| (TrackId(tid), e.played_at.unwrap_or_else(Utc::now))))
        .collect()
}

/// `Command::TogglePlaylistMembership`'s core logic, pulled out so it's
/// unit-testable without a `Store`: adds `track` if absent, else removes
/// every occurrence (a track could be duplicated by hand-editing/import).
fn toggle_membership(items: &mut Vec<TrackId>, track: TrackId) {
    if items.contains(&track) {
        items.retain(|&t| t != track);
    } else {
        items.push(track);
    }
}

/// `Session::bind_hotkey`'s core logic, pulled out so it's unit-testable:
/// binds `key` to `target` in `hotkeys`, dropping `target`'s previous key
/// (if different) and stealing `key` from whatever target held it. Returns
/// the target `key` was stolen from, if any (never `target` itself —
/// rebinding a target to the key it already has isn't a steal).
/// Whatever currently answers to `key`: an explicit table entry if one
/// exists, else — if `key` is a built-in action's default and that action
/// hasn't been explicitly remapped elsewhere — that action, implicitly.
fn effective_target_at(hotkeys: &HashMap<char, HotkeyTarget>, key: char) -> Option<HotkeyTarget> {
    if let Some(t) = hotkeys.get(&key) {
        return Some(t.clone());
    }
    BuiltinAction::ALL.iter().find_map(|&(action, default)| {
        let target = HotkeyTarget::Builtin(action);
        (default == key && !hotkeys.values().any(|t| *t == target)).then_some(target)
    })
}

/// `Session::bind_hotkey`'s core logic, pulled out so it's unit-testable —
/// see that method's doc for the built-in-key refusal and steal/rebind rules.
fn bind_hotkey(
    hotkeys: &mut HashMap<char, HotkeyTarget>,
    key: char,
    target: HotkeyTarget,
) -> std::result::Result<Option<HotkeyTarget>, HotkeyTarget> {
    if let Some(occupant) = effective_target_at(hotkeys, key)
        && occupant != target
        && matches!(occupant, HotkeyTarget::Builtin(_))
    {
        return Err(occupant);
    }
    hotkeys.retain(|&k, p| *p != target || k == key);
    Ok(hotkeys.insert(key, target.clone()).filter(|p| *p != target))
}

fn build_entry(t: &Track, primary: String) -> M3uEntry {
    let secs = if t.duration_ms > 0 {
        (t.duration_ms as f64 / 1000.0).round() as i64
    } else {
        t.renditions
            .iter()
            .map(|r| r.duration_ms)
            .find(|d| *d > 0)
            .map(|d| (d as f64 / 1000.0).round() as i64)
            .unwrap_or(-1)
    };
    let title = t.display_name();
    let meta = SoftMeta {
        isrc: t.isrc.clone(),
        album: t.album.clone(),
        year: t.year,
        bpm: t.attrs.get("bpm").and_then(|v| v.parse().ok()),
        key: t.attrs.get("key").cloned(),
        tags: t.tags.clone(),
    };
    M3uEntry {
        extinf: Some((secs, title)),
        track_id: Some(t.id.0),
        played_at: None,
        meta,
        renditions: t.renditions.iter().map(ParsedRendition::from_rendition).collect(),
        primary_uri: primary,
    }
}

