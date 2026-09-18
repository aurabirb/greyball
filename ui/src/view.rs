//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.
//!
//! Holds no application state: only the cursor, the visible screen, and the
//! text being typed into the search / `:` line. Everything drawn is read fresh
//! from [`core::Session`] snapshots; every committed key becomes a
//! [`core::Command`] handed to `Session::dispatch`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cursive::{
    Cursive, Printer, Rect, Vec2, View,
    direction::Direction,
    event::{Event, EventResult, Key, MouseButton, MouseEvent},
    theme::{BaseColor, Color, ColorStyle},
    view::CannotFocus,
    views::Dialog,
};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use core::{
    Axis, BrowseNode, Command, CoreEvent, Dispatch, HotkeyTarget, LogBuf, PaneLayoutConfig,
    PaneMode, PlayerState, Playlist, PlaylistId, Plugin, PluginHealth, ScanMode, Session, SetupKind,
    Side, SourceId, TOGGLABLE_SOURCES, TrackId,
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::command::{self, Pane};
use crate::keybindings::{self, Action};
use crate::row::RowItem;
use crate::SessionHandle;

/// `1` key / tab — the track list `Command::PlayContext` last started
/// playing from (a playlist, search results, Liked Songs, ...), i.e.
/// `Session::playing_context_ids`/`_window`, with the currently-playing
/// track highlighted (`tracks_to_rows`'s usual `current` flag). Stays on
/// the *last* played list even once playback stops/pauses — it's replaced
/// only by the next `Command::PlayContext`. Has its own `cursor`/
/// `list_offset` slot, entirely independent of the Playlists screen's
/// `open_playlist`/`open_remote` browsing state — it used to alias
/// `PLAYLISTS` wholesale (via `norm_screen`), which made this tab show
/// whichever playlist happened to be *browsed* on the Playlists screen (or
/// the bare playlist list, if none was), not whatever was actually playing.
pub(crate) const NOW_PLAYING: usize = 0;
pub(crate) const QUEUE: usize = 1;
pub(crate) const PLAYLISTS: usize = 2;
/// `:hist` or the `4` key. Also reachable as a dockable pane —
/// `Pane::History` — for viewing it alongside another screen; see
/// `list_screen_for_pane`.
pub(crate) const HIST: usize = 3;
/// `/` or the `3` key.
pub(crate) const SEARCH: usize = 4;
/// Number of screens — sizes `cursor` below.
const N_SCREENS: usize = 5;

/// Identity today — every screen (including `NOW_PLAYING`) now keeps its
/// own `cursor`/`list_offset` slot and content. Kept as a named pass-through
/// (rather than deleting every call site) since it's still the documented
/// place to normalize a `screen` value if a future screen ever needs to
/// alias another one the way `NOW_PLAYING` used to alias `PLAYLISTS`.
fn norm_screen(screen: usize) -> usize {
    screen
}

/// The numbered-screen index a track-list pane shares its list/cursor/
/// scroll state with — `None` for a pane with no such list (Log/Settings/
/// Vis). `Pane::Queue`/`Pane::History` deliberately reuse `QUEUE`/`HIST`'s
/// state wholesale rather than keeping a separate copy, so e.g. scrolling
/// the docked Queue pane and later switching to it as the main screen (or
/// vice versa) picks up exactly where you left off.
/// Resolves `initial_screen` (config's `"now_playing"` default, or whatever
/// the user set) to the screen index `MedleyView::new` should start on. The
/// restored `PlaybackContext`, if any, is what makes `NOW_PLAYING` show
/// something meaningful — see `Session::new`'s restore of
/// `NOW_PLAYING_PLAYLIST_ID`. Pulled out of `MedleyView::new` so the mapping
/// is unit-testable without a real `Session`.
fn startup_screen(initial_screen: &str) -> usize {
    match initial_screen {
        "queue" => QUEUE,
        "playlists" => PLAYLISTS,
        "hist" => HIST,
        "now_playing" => NOW_PLAYING,
        _ => SEARCH,
    }
}

/// Cursor stepping shared by every index-into-a-list screen/modal (the main
/// tracklist, a focused Queue/History pane, and the warnings/hotkey-menu/
/// playlist-picker modals) — one place for the clamp-to-`len` arithmetic
/// used by their arrow-key, mouse-wheel, PageUp/PageDown and Shift-J/
/// Shift-K arms alike (see `Scrollable`/`CursorWindow` below, which pairs
/// this with `follow_cursor_offset` for the ones that also keep a scroll
/// window).
fn stepped_cursor(cur: usize, len: usize, up: bool, step: usize) -> usize {
    if up {
        cur.saturating_sub(step)
    } else if len == 0 {
        cur
    } else {
        (cur + step).min(len - 1)
    }
}

/// Visible row count for a fullscreen modal list starting at `list_top`,
/// given the whole-screen height — mirrors the row-truncation check the
/// modal draw functions use (`y + 2 >= printer.size.y`), so the window this
/// computes always matches what actually gets drawn.
fn modal_list_h(screen_h: usize, list_top: usize) -> usize {
    screen_h.saturating_sub(list_top).saturating_sub(2)
}

/// Shared "jump N rows, clamped to what's actually there" behavior for a
/// scrollable list/pane — one implementation per distinct underlying state
/// shape, so every PageUp/PageDown and Shift-J/Shift-K arm routes through
/// the same math instead of re-deriving it per screen. `CursorWindow` below
/// is the shape used by an index-into-a-list cursor with a viewport window
/// that follows it (the main tracklist, a focused Queue/History pane, and
/// the warnings/hotkey-menu/playlist-picker modals). The other shape in
/// this file — a raw scroll offset with no separate selection (Log/
/// Settings' line-scroll, the help modal) — has only two owners, each
/// already (or newly) consolidated into its own single function
/// (`scroll_pane`, `jump_help`) rather than a second `Scrollable` impl:
/// Log's offset also carries pin state (`log_pin_after_scroll`) that has to
/// be recomputed from the *just-mutated* offset before the length used to
/// clamp it can even be known, so it can't share a single generic
/// bump-then-clamp call the way the cursor shape's four owners can.
trait Scrollable {
    /// Move `step` rows up (`true`) or down (`false`) through `len` rows of
    /// content shown in a `view_h`-row viewport.
    fn jump(&mut self, up: bool, step: usize, len: usize, view_h: usize);
}

/// One screen/modal's cursor + the viewport offset that follows it —
/// borrowed just long enough to run `jump`/`follow`. See `Scrollable`.
struct CursorWindow<'a> {
    cursor: &'a mut usize,
    offset: &'a mut usize,
}

impl CursorWindow<'_> {
    /// Resync `offset` to `cursor` without moving `cursor` itself — used
    /// after something *other* than nav (a resize, a filter cycling the row
    /// list) leaves the cursor outside the window.
    fn follow(&mut self, view_h: usize) {
        *self.offset = follow_cursor_offset(*self.cursor, *self.offset, view_h);
    }
}

impl Scrollable for CursorWindow<'_> {
    fn jump(&mut self, up: bool, step: usize, len: usize, view_h: usize) {
        *self.cursor = stepped_cursor(*self.cursor, len, up, step);
        self.follow(view_h);
    }
}

fn list_screen_for_pane(pane: Pane) -> Option<usize> {
    match pane {
        Pane::Queue => Some(QUEUE),
        Pane::History => Some(HIST),
        Pane::Log | Pane::Settings | Pane::Vis => None,
    }
}

/// Rows reserved at the very top of the terminal (the title/tab bar) and
/// bottom (the command/hint line + player-status line) — `split` carves
/// pane/main-content space out of what's left, in every `Side`, so these
/// never get resized, squeezed, or overlapped by a pane.
const TAB_BAR_ROWS: usize = 1;
const BOTTOM_BAR_ROWS: usize = 2;
/// Rows per `PageUp`/`PageDown`/Shift-J/Shift-K press on a raw-scroll-offset
/// pane (Log, Settings, the help modal) — see `scroll_pane`/`jump_help`.
const PAGE_SCROLL_STEP: usize = 10;
/// Rows per `PageUp`/`PageDown`/Shift-J/Shift-K jump on any index-into-a-list
/// cursor screen/modal (the main tracklist, a focused Queue/History pane,
/// warnings, hotkey menu, playlist picker) — see `Scrollable`/`CursorWindow`.
const LIST_JUMP_STEP: usize = 10;
/// Rows per mouse-wheel tick on a list/pane's single-row nav.
const WHEEL_STEP: usize = 3;
/// Two clicks on the same row within this long count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
/// Row the warnings modal's plugin list starts on (row 0 = title, row 1 =
/// blank spacer) — shared between `draw_warnings` and its click handling so
/// they can't drift apart.
const WARNINGS_LIST_TOP: usize = 2;
/// Cap on how many plugin messages the warnings modal's bottom section shows
/// — keeps a handful of failing plugins from crowding the navigable list
/// entirely off-screen. See `draw_warnings`.
const WARNINGS_MESSAGES_MAX: usize = 5;
/// Row the hotkey-menu modal's playlist list starts on — same shape as
/// `WARNINGS_LIST_TOP`.
const HOTKEY_LIST_TOP: usize = 2;
/// Row the "Add to Playlist" picker's list starts on — same shape as
/// `HOTKEY_LIST_TOP`.
const PLAYLIST_PICKER_LIST_TOP: usize = 2;
/// Row the help screen's content starts on (row 0 = title, no blank
/// spacer — the content is long enough as it is).
const HELP_LIST_TOP: usize = 1;

/// `Action::CyclePaneLayout`'s rotation, one `(side, stack)` step per press:
/// a column on the right (rows stacked top-to-bottom) -> a bar on the
/// bottom (panes side by side) -> a column on the left (stacked) -> a bar
/// on top (side by side) -> back to the start. Each pairing is the one that
/// reads naturally for that side (see `split`'s own per-side stacking).
pub(crate) const PANE_LAYOUT_CYCLE: [(Side, Axis); 4] = [
    (Side::Right, Axis::Vertical),
    (Side::Bottom, Axis::Horizontal),
    (Side::Left, Axis::Vertical),
    (Side::Top, Axis::Horizontal),
];

#[derive(Clone, PartialEq)]
enum Editing {
    None,
    Search,
    CommandLine,
    /// Collecting a `SetupKind::TextInput` value (e.g. a pasted SoundCloud
    /// OAuth token) for the warnings-panel plugin selected. Cancels back to
    /// `None` on `Esc`, same as `Search`/`CommandLine` — see the "active
    /// text field" block at the top of `on_event`, which is generic over
    /// every `Editing` variant already.
    PluginSetup(SourceId),
    /// Screen-local fuzzy filter (`/` on any track-list screen other than
    /// Search itself) — narrows the currently-viewed list to rows matching
    /// `self.buffer`, live as it's typed. Distinct from `Search`, which
    /// jumps to the Search screen and runs a real `Command::Search`; this
    /// never touches `Session` at all. Esc clears `filter_query` and shows
    /// the full list again, same as it clears any other `Editing` buffer.
    Filter,
}

/// Which pane navigation keys (arrows/j-k/PgUp/PgDn) go to.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Main,
    Pane(Pane),
    /// The bottom-row warnings button — `Enter` opens the warnings modal.
    Warnings,
}

/// One row of the Playlists screen's top-level list: a local (medley) playlist,
/// or a folder from a source's browse tree (e.g. one of the user's Spotify
/// playlists).
enum TopRow {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode),
}

impl TopRow {
    /// This row's `HotkeyTarget` — what a playlist hotkey binds to.
    fn target(&self) -> HotkeyTarget {
        match self {
            TopRow::Local(id) => HotkeyTarget::Local(*id),
            TopRow::Remote(sid, _, node) => HotkeyTarget::Remote(sid.clone(), node.clone()),
        }
    }
}

/// Display name for a `TopRow` — a local playlist's own name (looked up in
/// `playlists`, fetched once by the caller) or `[source] name` for a remote
/// browse folder.
fn top_row_name(row: &TopRow, playlists: &[Playlist]) -> String {
    match row {
        TopRow::Local(id) => playlists
            .iter()
            .find(|p| p.id == *id)
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        TopRow::Remote(sid, name, _) => format!("[{sid}] {name}"),
    }
}

/// A remote browse folder/playlist (e.g. a Spotify playlist) as tracked by
/// `open_remote`/`RememberedPlaylist::Remote` — source, display name, node.
type RemoteOpen = (SourceId, String, BrowseNode);

/// Which kind of playlist view was open on the Playlists screen when it was
/// left for another screen — either can be remembered so switching back
/// restores it, see `remembered_playlist`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RememberedPlaylist {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode),
}

/// Pure state transition backing `leave_playlists`: leaving the Playlists
/// screen always clears `open_playlist`/`open_remote`, but carries whichever
/// one was actually open forward into the remembered slot (replacing
/// whatever was remembered before).
fn playlists_left(
    open_playlist: Option<PlaylistId>,
    open_remote: Option<RemoteOpen>,
    remembered: Option<RememberedPlaylist>,
) -> (Option<PlaylistId>, Option<RemoteOpen>, Option<RememberedPlaylist>) {
    let remembered = match (open_playlist, open_remote) {
        (Some(id), _) => Some(RememberedPlaylist::Local(id)),
        (None, Some((sid, name, node))) => Some(RememberedPlaylist::Remote(sid, name, node)),
        (None, None) => remembered,
    };
    (None, None, remembered)
}

/// What `open_playlist`/`open_remote` should become when switching back to
/// the Playlists screen, given whichever `RememberedPlaylist` was left
/// behind — `None` if nothing was remembered, or if a remembered *local*
/// playlist no longer exists in `playlists` (deleted/renamed away in the
/// meantime), so a stale id never resurfaces a dead view. A remembered
/// remote node is always restored as-is: opening one never validates it
/// exists either (`open_playlist_uri`/`activate`'s `TopRow::Remote` arm just
/// set it directly), browsing lazily surfaces any staleness instead.
fn resolve_remembered_playlist(
    remembered: Option<RememberedPlaylist>,
    playlists: &[Playlist],
) -> Option<RememberedPlaylist> {
    remembered.filter(|r| match r {
        RememberedPlaylist::Local(id) => playlists.iter().any(|p| p.id == *id),
        RememberedPlaylist::Remote(..) => true,
    })
}


/// One column's rendered cell: text plus a color override, from
/// `render_cell` — `color: None` means draw it in the row's normal color
/// like every other cell (the common case for every column but `tags`
/// today).
#[derive(Clone)]
struct Cell {
    text: String,
    color: Option<Color>,
}

impl Cell {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), color: None }
    }
}

/// A rendered row plus whether it is the now-playing track. Every column is
/// a `Cell` (text + optional color) produced by `render_cell`, so a future
/// per-column renderer needs no changes here — just another `Column` arm.
struct Row {
    tags: Cell,
    main: Cell,
    /// Hotkey chars of every hotkey-bound (non-synthetic) playlist this
    /// row's track belongs to, sorted and concatenated (e.g. "Cgm") — blank
    /// if none match, or if a relevant playlist's membership hasn't loaded
    /// into `ViewCache` yet.
    hotkeys: Cell,
    source: Cell,
    duration: Cell,
    current: bool,
}

/// A `Row` with only its main column set — every non-track list row (a
/// placeholder message, a playlist/folder entry on the Playlists screen's
/// top level) shares this shape.
fn plain_row(main: impl Into<String>) -> Row {
    Row {
        tags: Cell::plain(""),
        main: Cell::plain(main),
        source: Cell::plain(""),
        duration: Cell::plain(""),
        hotkeys: Cell::plain(""),
        current: false,
    }
}

/// `MedleyView::filter_cache`'s contents — the last `filtered_tracks`
/// computation, plus the key it was computed under. Reused verbatim while
/// the key still matches, so a cursor move (which touches none of these)
/// costs a clone instead of a full re-filter/re-sort.
struct FilterCache {
    screen: usize,
    /// `(open_playlist, open_remote)` identity, so switching between two
    /// same-length playlists on the Playlists screen (same `screen` index)
    /// doesn't reuse a cache built for the other one.
    list_id: (Option<PlaylistId>, Option<(SourceId, BrowseNode)>),
    query: String,
    source_len: usize,
    result: Vec<core::Track>,
}

/// The `/`-filter's rank for one row against `query`, low-to-high, `None`
/// if it doesn't match at all: tier 0 (case-insensitive substring/full
/// match) always sorts above tier 1 (fuzzy-only), and within a tier, higher
/// fuzzy score sorts first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct FilterRank(u8, std::cmp::Reverse<i64>);

fn rank_filter(matcher: &SkimMatcherV2, text: &str, query: &str) -> Option<FilterRank> {
    let text = text.to_lowercase();
    let query = query.to_lowercase();
    let score = matcher.fuzzy_match(&text, &query)?;
    let tier = if text.contains(&query) { 0 } else { 1 };
    Some(FilterRank(tier, std::cmp::Reverse(score)))
}

pub struct MedleyView {
    session: SessionHandle,
    screen: usize,
    cursor: [usize; N_SCREENS],
    /// First visible row of each screen's list, persisted across redraws so
    /// the cursor can move freely *within* the current window before the
    /// window itself scrolls — recomputing this fresh from `cursor` every
    /// frame (the old approach) pins the cursor to the last visible row the
    /// moment it scrolls at all, so e.g. pressing Up at the bottom moves the
    /// whole window instead of the highlight. A wheel scroll moves this
    /// directly and nothing else — no reclamp follows, so the window can sit
    /// arbitrarily far from `cursor` indefinitely (mirrors sort-tab-
    /// features's `ListView`, whose `scroller.scroll_up/down` never touch
    /// `selected` either). Every other site that changes `cursor` — or the
    /// active screen/list identity, making the old position meaningless —
    /// calls `clamp_scroll` right after, which is the only place this ever
    /// follows the cursor back into view.
    list_offset: [usize; N_SCREENS],
    /// The main list's rect as of the last layout pass (matches `draw`'s
    /// `main_rect`) — `clamp_scroll` and mouse hit-testing need it, but only
    /// `required_size` (given the constraint size) can compute it.
    last_main_rect: Rect,
    /// Whole-terminal size as of the last layout pass — the fullscreen
    /// modals (warnings/hotkey-menu/playlist-picker) render over the entire
    /// screen rather than `last_main_rect`, so their own offset clamping
    /// needs this instead.
    last_screen_size: Vec2,
    editing: Editing,
    buffer: String,
    /// Committed screen-local filter query (`Editing::Filter`, Enter to
    /// commit) — `None` while nothing is filtering. Read through
    /// `active_filter`, which prefers the live `buffer` while still typing.
    /// Cleared whenever the underlying list identity changes (switching
    /// screen, opening/leaving a playlist) since a stale filter over a
    /// now-different list would just be confusing.
    filter_query: Option<String>,
    /// Fuzzy matcher backing the `/`-filter — built once and reused across
    /// keystrokes/redraws rather than per lookup.
    filter_matcher: SkimMatcherV2,
    /// Memoized result of the last `filtered_tracks` computation — filtering
    /// and ranking the whole list is too expensive to redo on every redraw
    /// (e.g. every j/k cursor move, which changes nothing about the filter
    /// itself). Recomputed only when the screen, query, or source list's
    /// length changes; cursor movement just re-indexes the cached result.
    filter_cache: std::sync::Mutex<Option<FilterCache>>,
    /// Last queue/wedge result ("Queued: 23 tracks" / "Wedged: 1 tracks"),
    /// shown on the hint line in place of the static hint until the next
    /// keypress — mirrors `hotkey_feedback`'s convention.
    queue_feedback: Option<String>,
    /// UI-local: which local playlist's tracks are shown on the playlists screen.
    open_playlist: Option<PlaylistId>,
    /// UI-local: which remote browse folder (e.g. a Spotify playlist) is
    /// shown on the playlists screen. Mutually exclusive with `open_playlist`.
    open_remote: Option<(SourceId, String, BrowseNode)>,
    /// UI-local: whichever playlist (local or remote) was open on the
    /// Playlists screen the last time it was left for another screen —
    /// restored into `open_playlist`/`open_remote` on switching back
    /// (`Action::Screen`), so the screen doesn't reset to the top-level list
    /// on every tab round-trip. Cleared (not just left stale) by explicitly
    /// backing out to the top-level list (Esc) so that gesture still means
    /// "forget this", and skipped on restore if a remembered local playlist
    /// no longer exists (deleted/renamed away in the meantime) to avoid
    /// resurrecting a dead id.
    remembered_playlist: Option<RememberedPlaylist>,
    /// UI-local: which optional panes are currently open, in stack order
    /// (first = nearest the main content). Toggled by `:log` / `:settings`.
    open_panes: Vec<Pane>,
    /// Shared default placement — screen vs. embedded, which side, which
    /// stacking axis — for any pane that hasn't been individually overridden
    /// via `pane_mode_overrides`. Seeded from `Config.panes`; `:panes`
    /// (with no pane name) changes it live (MVP: not persisted back to the
    /// config file). `side`/`stack` stay shared across all embedded panes
    /// even after per-pane `mode` overrides — only *screen vs. embedded* is
    /// independent per pane (see `pane_mode`), not layout geometry.
    pane_cfg: PaneLayoutConfig,
    /// Per-pane override of `pane_cfg.mode` — `:panes <pane> <screen|
    /// embedded>`. Absent means "use the shared default". Read via
    /// `pane_mode`; takes effect the next time that pane is toggled, not
    /// retroactively on a pane already open.
    pane_mode_overrides: HashMap<Pane, PaneMode>,
    /// Each embedded pane's rect as of the last layout pass, mirroring
    /// `last_main_rect` — lets `clamp_scroll` size a focused list-pane's
    /// (Queue/History) own visible window without re-deriving `split()`.
    last_pane_rects: Vec<(Pane, Rect)>,
    /// Recent log lines, shared with `app`'s logger. Read-only here.
    log: Arc<LogBuf>,
    /// UI-local: the text of the last committed `Command::Search`, so an
    /// empty result list can say "no results for X" instead of looking
    /// identical to a screen nobody has searched on yet.
    last_query: Option<String>,
    /// UI-local: how many (wrapped) rows up from the live tail the embedded
    /// Log pane is scrolled. 0 = normal, always-following tail. `PageUp` /
    /// `PageDown` adjust it; clamped against the real content at draw time
    /// (draw is the only place that knows the pane's current width/height).
    log_scroll: usize,
    /// `Some(n)` while the Log pane is scrolled away from the tail
    /// (`log_scroll != 0`): the length of `log`'s snapshot at the moment it
    /// was left, so lines arriving afterwards queue up out of view instead
    /// of shifting what's on screen. `None` while following the tail
    /// (`log_scroll == 0`), and whenever it's cleared. Maintained by
    /// `scroll_pane` (see `log_pin_after_scroll`); read by the Log draw
    /// sites via `log_visible_len`.
    log_pin: Option<usize>,
    /// Selected row within the Settings pane's entry list.
    settings_cursor: usize,
    /// Scroll window into the Settings pane's list — see `warnings_offset`.
    settings_offset: usize,
    /// Which pane currently receives nav keys; `Tab` cycles it.
    focus: Focus,
    /// The Vis pane's background worker + last computed frame.
    vis: Arc<crate::vis::Vis>,
    /// `PaneMode::Screen`'s pane, shown fullscreen in place of the normal 3
    /// screens; `None` the rest of the time. Esc is the only way out.
    screen_pane: Option<Pane>,
    /// The plugin-warnings modal — a fullscreen overlay like `screen_pane`,
    /// but independent of the pane system (opened from the bottom-row
    /// button / `Focus::Warnings`, not `:log`-style toggling).
    warnings_open: bool,
    /// Selected row within the warnings modal.
    warnings_cursor: usize,
    /// Scroll window into the warnings modal's list — kept following
    /// `warnings_cursor` the same way `list_offset` follows `cursor` for the
    /// main list (see `modal_list_h`/`follow_cursor_offset`).
    warnings_offset: usize,
    /// (when, screen, row index) of the last left-click on a list row, so a
    /// second click on the *same* row within `DOUBLE_CLICK_WINDOW` can be
    /// recognized as a double-click. See `click_row`.
    last_click: Option<(Instant, usize, usize)>,
    /// The "Hotkeys" management modal (backtick, off the Playlists screen)
    /// — a fullscreen overlay like `warnings_open`/`screen_pane`, listing
    /// every built-in action with its bound key, if any. Playlist hotkeys
    /// are bound from the Playlists screen instead (see
    /// `draw_playlist_hotkey_modal`).
    hotkey_menu_open: bool,
    /// Selected row within the hotkey-menu modal.
    hotkey_menu_cursor: usize,
    /// Scroll window into the hotkey-menu modal's list, in visual (on-screen)
    /// row space, not `hotkey_menu_cursor`'s logical `hotkey_rows` space.
    hotkey_menu_offset: usize,
    /// The "press a key to bind" sub-popup — `Some(target)` while waiting for
    /// the next raw keypress to become that target's new binding; `None` the
    /// rest of the time. Drawn on top of the hotkey menu when a built-in is
    /// being bound (`hotkey_menu_open` also true), or standalone (see
    /// `draw_playlist_hotkey_modal`) when a playlist's hotkey is being set
    /// from the Playlists screen (`hotkey_menu_open` false).
    hotkey_capture: Option<HotkeyTarget>,
    /// Last bind/unbind result shown in the hotkey menu's footer, or on the
    /// Playlists screen's own hint line for a standalone `hotkey_capture`
    /// (e.g. "Bound 'a' to Chill (moved from Focus)") until the next
    /// keypress or the modal closes.
    hotkey_feedback: Option<String>,
    /// The help/shortcuts screen (`?` or `:help`) — a fullscreen overlay
    /// like `warnings_open`/`hotkey_menu_open`, listing `command::HELP` and
    /// the current keybindings.
    help_open: bool,
    /// Rows scrolled down from the top of the help screen's content.
    help_scroll: usize,
    /// The "Add to Playlist" picker (`+`, a track selected) — a fullscreen
    /// overlay like `hotkey_menu_open`, listing every local playlist;
    /// Enter/click adds `playlist_picker_track` to the highlighted one.
    playlist_picker_open: bool,
    /// Selected row within the playlist picker.
    playlist_picker_cursor: usize,
    /// Scroll window into the playlist picker's list — see `warnings_offset`.
    playlist_picker_offset: usize,
    /// The track being added, captured when the picker opens (`+` on that
    /// track's row) so it stays fixed even if the underlying list scrolls.
    playlist_picker_track: Option<TrackId>,
    /// (last now-playing text seen, when its marquee scroll started) for the
    /// tab bar's own marquee (`draw_tab_bar`), shown next to the tabs when
    /// the screen is too narrow for their detail text. A `Mutex` rather than
    /// a plain field only because `draw` takes `&self` (mirrors
    /// `filter_cache`'s interior-mutability cache below) — single-threaded,
    /// never contended. Mirrors `app::title::WindowTitle`'s own
    /// `full`/`scroll_start` pair so both marquees use the same timing.
    tab_marquee: std::sync::Mutex<(String, Instant)>,
}

impl MedleyView {
    pub fn new(session: SessionHandle, initial_screen: &str, log: Arc<LogBuf>) -> Self {
        let screen = startup_screen(initial_screen);
        let pane_cfg = session.lock().unwrap().cfg.panes;
        let vis = crate::vis::Vis::spawn(session.clone());
        Self {
            session,
            screen,
            cursor: [0; N_SCREENS],
            list_offset: [0; N_SCREENS],
            last_main_rect: Rect::from_size((0, 0), (0, 0)),
            last_screen_size: Vec2::new(0, 0),
            editing: Editing::None,
            buffer: String::new(),
            filter_query: None,
            filter_matcher: SkimMatcherV2::default(),
            filter_cache: std::sync::Mutex::new(None),
            queue_feedback: None,
            open_playlist: None,
            open_remote: None,
            remembered_playlist: None,
            open_panes: Vec::new(),
            pane_cfg,
            pane_mode_overrides: HashMap::new(),
            last_pane_rects: Vec::new(),
            log,
            last_query: None,
            log_scroll: 0,
            log_pin: None,
            settings_cursor: 0,
            settings_offset: 0,
            focus: Focus::Main,
            vis,
            screen_pane: None,
            warnings_open: false,
            warnings_cursor: 0,
            warnings_offset: 0,
            last_click: None,
            hotkey_menu_open: false,
            hotkey_menu_cursor: 0,
            hotkey_menu_offset: 0,
            hotkey_capture: None,
            hotkey_feedback: None,
            help_open: false,
            help_scroll: 0,
            playlist_picker_open: false,
            playlist_picker_cursor: 0,
            playlist_picker_offset: 0,
            playlist_picker_track: None,
            tab_marquee: std::sync::Mutex::new((String::new(), Instant::now())),
        }
    }

    /// `Main`, then each open pane (in stack order), then the warnings
    /// button — last, and only present when there's at least one warning
    /// to show (it's otherwise not drawn at all, so it can't be focused).
    fn focus_order(&self) -> Vec<Focus> {
        // `open_panes` only ever holds panes currently placed `Embedded`
        // (see `toggle_pane`) — no need to also check a mode here now that
        // placement is per-pane rather than one global switch.
        let mut order = if self.open_panes.is_empty() {
            vec![Focus::Main]
        } else {
            std::iter::once(Focus::Main)
                .chain(self.open_panes.iter().map(|&p| Focus::Pane(p)))
                .collect()
        };
        if self.warn_count() > 0 {
            order.push(Focus::Warnings);
        }
        order
    }

    fn cycle_focus(&mut self) {
        let order = self.focus_order();
        let idx = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = order[(idx + 1) % order.len()];
    }

    fn clamp_focus(&mut self) {
        if !self.focus_order().contains(&self.focus) {
            self.focus = Focus::Main;
        }
    }

    /// Where focus should land when it can no longer stay on the warnings
    /// button — closing the modal, or a nav key arriving while it's
    /// focused. No "previously selected pane" is tracked, so this is
    /// always `Main`; a single fallback point so both call sites agree if
    /// that ever changes.
    fn fallback_focus(&self) -> Focus {
        Focus::Main
    }

    /// `Screen`-mode: `pane` fullscreen, title/content on top, an `Esc to
    /// close` hint on the bottom row.
    fn draw_screen_pane(&self, pane: Pane, printer: &Printer) {
        let h = printer.size.y.saturating_sub(1);
        let content = printer.windowed(Rect::from_size((0, 0), Vec2::new(printer.size.x, h)));
        match pane {
            Pane::Vis => self.vis.draw(&content, true),
            Pane::Log => {
                let (lines, scroll) = self.log_render_lines();
                draw_pane(pane, &content, &lines, scroll, true);
            }
            Pane::Settings => {
                let pane_cfg = self.pane_cfg;
                let entries = self.with_session(|s| settings_entries(s, pane_cfg));
                draw_settings_pane(&content, &entries, self.settings_offset, self.settings_cursor, true);
            }
            // `toggle_pane` never routes these two here — a `Screen`-mode
            // Queue/History switches `self.screen` instead (see its doc).
            Pane::Queue | Pane::History => unreachable!("Queue/History never become screen_pane"),
        }
        let hint = match pane {
            Pane::Vis => "  [Esc] close",
            Pane::Settings => "  [Esc] close   [↑/↓ j/k] move   [Enter/Space] toggle",
            _ => "  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll",
        };
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, h), &pad(hint, p.size.x));
        });
    }

    /// "{id}: {msg}" for every plugin currently reporting a non-`Ok` health
    /// — the informational, non-clickable section at the bottom of the
    /// warnings modal. See `draw_warnings`.
    fn warnings_messages(&self) -> Vec<String> {
        self.with_session(|s| {
            s.plugin_statuses()
                .into_iter()
                .filter_map(|(id, health)| health.message().map(|m| format!("{id}: {m}")))
                .collect()
        })
    }

    /// Rows the bottom messages section reserves (a blank separator plus up
    /// to `WARNINGS_MESSAGES_MAX` message lines) — 0 when there are none, so
    /// it only eats into the navigable plugin list when it has something to
    /// show.
    fn warnings_messages_h(&self) -> usize {
        let n = self.warnings_messages().len();
        if n == 0 { 0 } else { 1 + n.min(WARNINGS_MESSAGES_MAX) }
    }

    /// Visible plugin rows in the warnings modal's navigable list, given the
    /// whole-screen height — `modal_list_h`'s row count minus whatever the
    /// bottom messages section (`warnings_messages_h`) currently reserves.
    fn warnings_list_h(&self, screen_h: usize) -> usize {
        modal_list_h(screen_h, WARNINGS_LIST_TOP).saturating_sub(self.warnings_messages_h())
    }

    /// Fullscreen plugin-warnings modal (row 0 title, row 1 blank, then the
    /// plugin list from `WARNINGS_LIST_TOP`, one row per plugin, followed by
    /// a separate non-navigable section listing every plugin's message) —
    /// see `Focus::Warnings` / the bottom-row button that opens it.
    fn draw_warnings(&self, printer: &Printer) {
        let statuses = self.with_session(|s| s.plugin_statuses());
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Plugin warnings", p.size.x));
        });

        if statuses.is_empty() {
            printer.print((0, WARNINGS_LIST_TOP), "(no plugins registered)");
        }
        let list_h = self.warnings_list_h(printer.size.y);
        for (i, (id, health)) in statuses.iter().enumerate().skip(self.warnings_offset).take(list_h) {
            let y = WARNINGS_LIST_TOP + (i - self.warnings_offset);
            let icon = match health {
                PluginHealth::Ok => "✓",
                PluginHealth::Warn(_) => "⚠",
                PluginHealth::Fail(_) => "✗",
            };
            let line = pad(&format!("{icon} {id}"), printer.size.x);
            if i == self.warnings_cursor {
                printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
            } else {
                printer.print((0, y), &line);
            }
        }

        let messages = self.warnings_messages();
        if !messages.is_empty() {
            let messages_top = WARNINGS_LIST_TOP + list_h + 1;
            for (j, msg) in messages.iter().take(WARNINGS_MESSAGES_MAX).enumerate() {
                printer.print((0, messages_top + j), &pad(msg, printer.size.x));
            }
        }

        if let Editing::PluginSetup(id) = &self.editing {
            let prompt = self
                .with_session(|s| s.plugin(id))
                .map(|p| match p.setup_kind() {
                    SetupKind::TextInput { prompt } => prompt,
                    SetupKind::Action => String::new(),
                })
                .unwrap_or_default();
            let y1 = printer.size.y.saturating_sub(2);
            let y2 = printer.size.y.saturating_sub(1);
            printer.with_color(ColorStyle::highlight_inactive(), |p| {
                p.print((0, y1), &pad(&prompt, p.size.x));
            });
            printer.print((0, y2), &pad(&format!("> {}  [Esc] cancel", self.buffer), printer.size.x));
        } else {
            let bottom = printer.size.y.saturating_sub(1);
            printer.with_color(ColorStyle::highlight_inactive(), |p| {
                p.print((0, bottom), &pad("  [Enter] run setup   [Esc] close", p.size.x));
            });
        }
    }

    /// Draws `lines` as a scrollable cursor list starting at row `list_top`
    /// — the selection/scroll-cutoff/highlight logic shared by
    /// `draw_hotkey_menu` and `draw_playlist_picker` (both are "title + list
    /// of rows with a cursor" modals that differ only in what the rows say
    /// and the footer).
    fn draw_rows(&self, printer: &Printer, lines: &[String], cursor: usize, offset: usize, list_top: usize) {
        let h = modal_list_h(printer.size.y, list_top);
        for (i, line) in lines.iter().enumerate().skip(offset).take(h) {
            let y = list_top + (i - offset);
            let line = pad(line, printer.size.x);
            if i == cursor {
                printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
            } else {
                printer.print((0, y), &line);
            }
        }
    }

    /// Fullscreen "Hotkeys" modal (backtick, off the Playlists screen) —
    /// same shape as `draw_warnings`: row 0 title, row 1 blank, then the
    /// built-in action list from `hotkey_rows` at `HOTKEY_LIST_TOP`, each row
    /// showing `{name:<name_w} {key}` (`-` if unbound, though every built-in
    /// always has at least its default). `name_w` is sized off the terminal
    /// width rather than a fixed column. Playlist hotkeys are set from the
    /// Playlists screen instead — see `draw_playlist_hotkey_modal`. While
    /// `hotkey_capture` is set, the footer becomes a "press a key" prompt
    /// instead of the usual hint/feedback line.
    fn draw_hotkey_menu(&self, printer: &Printer) {
        let rows = self.hotkey_rows();
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Hotkeys", p.size.x));
        });

        let name_w = printer.size.x.saturating_sub(3);
        let lines: Vec<String> = rows
            .iter()
            .map(|&action| {
                let key = self
                    .with_session(|s| s.effective_hotkey(&HotkeyTarget::Builtin(action)))
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let name = pad(&truncate_ellipsis(action.label(), name_w), name_w);
                format!("{name} {key}")
            })
            .collect();
        self.draw_rows(printer, &lines, self.hotkey_menu_cursor, self.hotkey_menu_offset, HOTKEY_LIST_TOP);

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            let line = if let Some(target) = &self.hotkey_capture {
                let name = rows
                    .iter()
                    .find(|&&a| HotkeyTarget::Builtin(a) == *target)
                    .map(|a| a.label().to_string())
                    .unwrap_or_else(|| "?".to_string());
                format!("  press a key to bind to {name:?}   [Esc] cancel")
            } else if let Some(msg) = &self.hotkey_feedback {
                format!("  {msg}")
            } else {
                "  select a row and press Enter   [Backspace] clear   [Esc] close".to_string()
            };
            p.print((0, bottom), &pad(&line, p.size.x));
        });
    }

    /// Standalone "press a key to bind" modal for a Playlists-screen row —
    /// backtick's local override of `Action::OpenHotkeyMenu` there (see
    /// `on_event`), reusing `hotkey_capture`/`bind_captured_key` directly
    /// rather than going through the built-ins-only hotkey menu at all.
    fn draw_playlist_hotkey_modal(&self, printer: &Printer) {
        let target = self.hotkey_capture.clone().expect("only drawn while capturing");
        let name = self.hotkey_row_name_for(&target);
        let current = self.with_session(|s| s.playlist_hotkey(&target));
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Set Hotkey", p.size.x));
        });
        printer.print((0, HOTKEY_LIST_TOP), &format!("press a key to bind to {name:?}"));

        let bottom = printer.size.y.saturating_sub(1);
        let hint = match current {
            Some(k) => format!("  currently '{k}'   [Backspace] clear   [Esc] cancel"),
            None => "  [Esc] cancel".to_string(),
        };
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad(&hint, p.size.x));
        });
    }

    /// Fullscreen "Add to Playlist" picker (`+` with a track selected) —
    /// same shape as `draw_hotkey_menu`: row 0 title, row 1 blank, then the
    /// playlist list from `PLAYLIST_PICKER_LIST_TOP`.
    fn draw_playlist_picker(&self, printer: &Printer) {
        let playlists = self.with_session(|s| s.playlists());
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Add to Playlist", p.size.x));
        });

        if playlists.is_empty() {
            printer.print(
                (0, PLAYLIST_PICKER_LIST_TOP),
                "(no playlists — :newplaylist <name> to make one)",
            );
        }
        let lines: Vec<String> = playlists.iter().map(|p| p.name.clone()).collect();
        self.draw_rows(
            printer,
            &lines,
            self.playlist_picker_cursor,
            self.playlist_picker_offset,
            PLAYLIST_PICKER_LIST_TOP,
        );

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad("  [Enter] add to selected playlist   [Esc] cancel", p.size.x));
        });
    }

    /// The help screen's content lines — shared by `draw_help` and
    /// `clamp_help_scroll` so they can never disagree on what's being
    /// scrolled.
    fn help_lines(&self) -> Vec<String> {
        let plugin_commands = self.with_session(|s| s.plugin_command_help());
        let (playlist_hotkeys, builtin_remaps) = self.with_session(|s| {
            let playlists = s.playlists();
            let rows = self.top_rows(s);
            let hotkeys = s.hotkeys();
            let playlist_hotkeys = hotkeys
                .iter()
                .filter_map(|(ch, target)| {
                    rows.iter()
                        .find(|r| r.target() == *target)
                        .map(|r| (*ch, top_row_name(r, &playlists)))
                })
                .collect::<Vec<_>>();
            let builtin_remaps = hotkeys
                .into_iter()
                .filter_map(|(ch, target)| match target {
                    HotkeyTarget::Builtin(action) => Some((ch, action.label().to_string())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            (playlist_hotkeys, builtin_remaps)
        });
        build_help_lines(&playlist_hotkeys, &builtin_remaps, &plugin_commands)
    }

    /// Fullscreen help/shortcuts modal (`?` or `:help`) — same shape as
    /// `draw_warnings`/`draw_hotkey_menu`: row 0 title, then scrollable
    /// content from `HELP_LIST_TOP` built by `build_help_lines`.
    fn draw_help(&self, printer: &Printer) {
        let lines = self.help_lines();

        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Help / Shortcuts", p.size.x));
        });

        let bottom = printer.size.y.saturating_sub(1);
        let h = bottom.saturating_sub(HELP_LIST_TOP);
        let max_scroll = lines.len().saturating_sub(h);
        let scroll = self.help_scroll.min(max_scroll);
        for (i, line) in lines.iter().skip(scroll).take(h).enumerate() {
            printer.print((0, HELP_LIST_TOP + i), line);
        }

        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad("  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll", p.size.x));
        });
    }

    /// Move `help_scroll` by `step` rows (up/down/PageUp-PageDown/
    /// Shift-J-Shift-K alike all funnel through this), clamped to the
    /// actual scrollable range for the current help content and screen size
    /// (`bound_offset` — same formula `draw_help` already applies at render
    /// time), so scrolling past either end can't inflate the stored value
    /// beyond what scrolling back would ever need to undo.
    fn jump_help(&mut self, up: bool, step: usize) {
        self.help_scroll =
            if up { self.help_scroll.saturating_sub(step) } else { self.help_scroll.saturating_add(step) };
        let len = self.help_lines().len();
        let h = self.last_screen_size.y.saturating_sub(1).saturating_sub(HELP_LIST_TOP);
        self.help_scroll = bound_offset(self.help_scroll, len, h);
    }

    /// `pane`'s own placement — `pane_mode_overrides` if `:panes <pane> ...`
    /// has set one, else the shared default.
    fn pane_mode(&self, pane: Pane) -> PaneMode {
        self.pane_mode_overrides.get(&pane).copied().unwrap_or(self.pane_cfg.mode)
    }

    /// Open/close `pane`, per its own `pane_mode` — `:log`, `:settings`,
    /// bare `:vis`, `:queue`, `:history`.
    ///
    /// `Queue`/`History` in `Screen` mode don't go through `screen_pane`
    /// like Log/Settings/Vis do — they instead just switch the main screen
    /// to their numbered-screen form (`view::QUEUE`/`HIST`), reusing all of
    /// its existing rendering/navigation rather than teaching `screen_pane`
    /// a second, row-based fullscreen path. This makes it a "switch to"
    /// rather than a true on/off toggle for those two: there's no prior
    /// screen to restore on a second press, unlike `screen_pane`'s Esc.
    /// Leave the Playlists screen for elsewhere, remembering whichever local
    /// playlist was open (if any) so switching back to Playlists can restore
    /// it — see `remembered_playlist`. Every "switch away" site funnels
    /// through this instead of clearing `open_playlist`/`open_remote`
    /// directly, so none of them forget to update the memory.
    fn leave_playlists(&mut self) {
        let (open, remote, remembered) =
            playlists_left(self.open_playlist, self.open_remote.clone(), self.remembered_playlist.clone());
        self.open_playlist = open;
        self.open_remote = remote;
        self.remembered_playlist = remembered;
    }

    fn toggle_pane(&mut self, pane: Pane) {
        if self.pane_mode(pane) == PaneMode::Screen {
            if let Some(screen) = list_screen_for_pane(pane) {
                self.screen = screen;
                self.leave_playlists();
            } else {
                self.screen_pane = if self.screen_pane == Some(pane) { None } else { Some(pane) };
            }
        } else if let Some(i) = self.open_panes.iter().position(|&p| p == pane) {
            self.open_panes.remove(i);
        } else {
            self.open_panes.push(pane);
        }
        self.clamp_focus();
        self.clamp_scroll(); // covers the screen-switch branch above; a no-op otherwise
    }

    /// Sync cursive's own redraw rate to whether/how fast the Vis pane needs
    /// to animate — its worker only bothers computing while `enabled`, this
    /// is what actually gets the blitted frame back on screen periodically.
    fn vis_fps_cb(&self) -> EventResult {
        let vis_open = self.open_panes.contains(&Pane::Vis) || self.screen_pane == Some(Pane::Vis);
        self.vis.set_enabled(vis_open);
        let fps = if vis_open { crate::vis::FPS } else { crate::BASELINE_FPS };
        EventResult::with_cb(move |siv| siv.set_fps(fps))
    }

    /// Line-scroll for Log/Vis only — Settings has its own row cursor
    /// (`jump_settings`) and a focused Queue/History pane is a track list,
    /// not lines; `on_event` routes both elsewhere before ever reaching here.
    fn scroll_pane(&mut self, pane: Pane, up: bool, step: usize) {
        if pane == Pane::Settings {
            self.jump_settings(up, step);
            return;
        }
        let s = match pane {
            Pane::Log => &mut self.log_scroll,
            Pane::Settings => unreachable!("handled above"),
            Pane::Vis => return, // nothing to scroll, it's live
            Pane::Queue | Pane::History => return,
        };
        if up {
            *s += step;
        } else {
            *s = s.saturating_sub(step);
        }
        // Pin (or release) the Log pane's view against `log`'s current
        // length — see `log_pin_after_scroll` for why.
        self.log_pin = log_pin_after_scroll(self.log_scroll, self.log_pin, self.log.snapshot().len());
        self.clamp_pane_scroll(pane);
    }

    /// Settings pane's row cursor — a `CursorWindow` list like Queue/History
    /// rather than Log's wrapped-line scroll, since each entry is one row.
    fn jump_settings(&mut self, up: bool, step: usize) {
        let pane_cfg = self.pane_cfg;
        let n = self.with_session(|s| settings_entries(s, pane_cfg).len());
        let h = self.pane_content_dims(Pane::Settings).map_or(0, |(_, h)| h);
        CursorWindow { cursor: &mut self.settings_cursor, offset: &mut self.settings_offset }.jump(up, step, n, h);
    }

    /// Enter/Space on the Settings pane's selected row.
    fn toggle_selected_setting(&mut self) {
        let cursor = self.settings_cursor;
        let pane_cfg = self.pane_cfg;
        let Some(entry) = self.with_session(|s| settings_entries(s, pane_cfg).into_iter().nth(cursor)) else {
            return;
        };
        match entry {
            SettingsEntry::Source { name, enabled } => {
                self.with_session_mut(|s| s.set_source_enabled(name, !enabled));
                self.queue_feedback = Some(format!(
                    "  {name}: {} (restart to apply)",
                    if enabled { "disabled" } else { "enabled" }
                ));
            }
            SettingsEntry::Scan { enabled, available: true } => {
                self.with_session_mut(|s| s.set_scan_enabled(!enabled));
            }
            SettingsEntry::Scan { available: false, .. } | SettingsEntry::Info(_) => {}
        }
    }

    /// The (width, content-row-count) `draw_pane` actually renders `pane`
    /// into right now — fullscreen if `screen_pane` shows it that way, else
    /// its docked rect from `last_pane_rects` — mirroring `draw_screen_pane`/
    /// the docked block in `draw` exactly, so the clamp below matches what's
    /// really on screen. `None` if `pane` isn't currently visible at all.
    fn pane_content_dims(&self, pane: Pane) -> Option<(usize, usize)> {
        if self.screen_pane == Some(pane) {
            Some((self.last_screen_size.x, self.last_screen_size.y.saturating_sub(2)))
        } else {
            self.last_pane_rects
                .iter()
                .find(|&&(p, _)| p == pane)
                .map(|&(_, rect)| (rect.width(), rect.height().saturating_sub(1)))
        }
    }

    /// Keep `log_scroll` inside the actual scrollable range for `pane`'s
    /// current content and on-screen size (`bound_offset` — same formula
    /// `draw_pane` already applies at render time), so scrolling past
    /// either end can't inflate the stored value beyond what scrolling back
    /// would ever need to undo.
    fn clamp_pane_scroll(&mut self, pane: Pane) {
        let Some((width, h)) = self.pane_content_dims(pane) else { return };
        let lines = match pane {
            Pane::Log => self.log_render_lines().0,
            Pane::Settings | Pane::Vis | Pane::Queue | Pane::History => return,
        };
        let wrapped_len: usize = lines.iter().map(|l| wrap(l, width).len()).sum();
        self.log_scroll = bound_offset(self.log_scroll, wrapped_len, h);
    }

    /// The Log pane's content/scroll for this frame: the snapshot lines
    /// actually visible (per `log_visible_len`'s pin logic) and the raw
    /// scroll offset — single source of truth for every place that renders
    /// or clamps the Log pane, so they can't drift apart.
    fn log_render_lines(&self) -> (Vec<String>, usize) {
        let snapshot = self.log.snapshot();
        let len = log_visible_len(self.log_scroll, self.log_pin, snapshot.len());
        (snapshot[..len].to_vec(), self.log_scroll)
    }

    // ---- session access -------------------------------------------------
    //
    // LOCK DISCIPLINE: `session` is a non-reentrant `std::sync::Mutex`. Never
    // hold the guard across a second `lock()` on this (UI) thread, and never
    // across a cursive call that can draw or run callbacks. Always go through
    // `with_session` — one lock, one closure, guard dropped on return — so a
    // single statement can never take the lock twice. (Regression guard: a
    // `.session.lock()` outside this method is a bug — it once deadlocked the
    // UI.)
    fn with_session<R>(&self, f: impl FnOnce(&Session) -> R) -> R {
        let guard = self.session.lock().unwrap();
        f(&guard)
    }

    fn with_session_mut<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut guard = self.session.lock().unwrap();
        f(&mut guard)
    }

    /// The live hotkey remap table, collected into the shape
    /// `keybindings::map`/`hotkey_toggle` take — every call site that needs
    /// either goes through this so there's one place reading it off `Session`.
    fn hotkeys_map(&self) -> HashMap<char, HotkeyTarget> {
        self.with_session(|s| s.hotkeys()).into_iter().collect()
    }

    // ---- snapshot helpers -------------------------------------------------

    /// The Playlists screen's combined top-level list: local playlists first,
    /// then each registered source's browse folders (e.g. Spotify playlists),
    /// grouped by source. `Session::remote_playlists` caches its network
    /// fetch, so calling this on every redraw is cheap.
    fn top_rows(&self, s: &Session) -> Vec<TopRow> {
        let mut rows: Vec<TopRow> = s.playlists().into_iter().map(|p| TopRow::Local(p.id)).collect();
        for sid in s.source_ids() {
            for (name, node) in s.remote_playlists(&sid) {
                rows.push(TopRow::Remote(sid.clone(), name, node));
            }
        }
        rows
    }

    /// The global hotkey menu's row list — every `core::BuiltinAction`.
    /// Playlist hotkeys are bound from the Playlists screen instead (see
    /// `draw_playlist_hotkey_modal`), so this menu no longer lists them.
    fn hotkey_rows(&self) -> Vec<core::BuiltinAction> {
        core::BuiltinAction::ALL.iter().map(|&(a, _)| a).collect()
    }

    /// Track ids visible on `screen`, in display order. Empty when the
    /// screen shows playlists rather than tracks. Takes an explicit screen
    /// (rather than always `self.screen`) so a docked Queue/History pane's
    /// own list can be resolved independently of whatever the main content
    /// is currently showing — see `active_screen`.
    fn visible_track_ids(&self, s: &Session, screen: usize) -> Vec<TrackId> {
        let screen = norm_screen(screen);
        if let Some(tracks) = self.filtered_tracks(s, screen) {
            return tracks.iter().map(|t| t.id).collect();
        }
        match screen {
            NOW_PLAYING => s.playing_context_ids(),
            SEARCH => s.results_ids(),
            QUEUE => s.queue_ids(),
            HIST => s.history_ids(),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlist_track_ids(id)
                } else if let Some((sid, _, node)) = &self.open_remote {
                    s.remote_playlist_track_ids(sid, node)
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// Every track already loaded for `screen`'s list, unwindowed — the
    /// input to the local `/`-filter (`filtered_tracks`) only; every other
    /// caller keeps using the windowed `_window`/`_ids` accessors above.
    /// Reuses those same accessors at `(0, len)` rather than a new store
    /// API, so this never resolves more than what's already available to
    /// the screen (no extra network search of any kind).
    fn all_tracks_for_screen(&self, s: &Session, screen: usize) -> Vec<core::Track> {
        let screen = norm_screen(screen);
        match screen {
            NOW_PLAYING => s.playing_context_window(0, s.playing_context_len()),
            QUEUE => s.queue_window(0, s.queue_len()),
            HIST => s.history_window(0, s.queue.history_len()),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlist_window(id, 0, s.playlist_len(id))
                } else if let Some((sid, _, node)) = &self.open_remote {
                    s.remote_playlist_window(sid, node, 0, s.remote_playlist_len(sid, node))
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// Cheap count of `all_tracks_for_screen`'s source list — no per-track
    /// store resolution, just whatever `_len` accessor that screen already
    /// has (mirrors `all_tracks_for_screen`'s own `match`). Used only to
    /// detect whether the filter cache has gone stale, so it must stay
    /// cheap enough to call on every redraw.
    fn filterable_source_len(&self, s: &Session, screen: usize) -> usize {
        match norm_screen(screen) {
            NOW_PLAYING => s.playing_context_len(),
            QUEUE => s.queue_len(),
            HIST => s.queue.history_len(),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlist_len(id)
                } else if let Some((sid, _, node)) = &self.open_remote {
                    s.remote_playlist_len(sid, node)
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Whether `/` on `screen` should filter it locally rather than jump to
    /// Search — every track-list screen, but not the Playlists screen's own
    /// top-level list of playlists/folders (nothing there is a track row).
    fn filterable_screen(&self, screen: usize) -> bool {
        match norm_screen(screen) {
            NOW_PLAYING | QUEUE | HIST => true,
            PLAYLISTS => self.open_playlist.is_some() || self.open_remote.is_some(),
            _ => false,
        }
    }

    /// The active local filter query: the live `buffer` while still typing
    /// it (`Editing::Filter`), else whatever was last committed with Enter.
    /// `None` while nothing is filtering.
    fn active_filter(&self) -> Option<&str> {
        match &self.editing {
            Editing::Filter => Some(self.buffer.as_str()),
            _ => self.filter_query.as_deref(),
        }
    }

    /// `screen`'s tracks narrowed and ranked by the active local filter
    /// (case-insensitive substring matches first, then looser fuzzy hits —
    /// see `rank_filter`) — `None` when nothing is filtering, or on a screen
    /// the filter doesn't apply to (Search already has its own remote
    /// query). Purely local: reads only what `all_tracks_for_screen`
    /// already has on hand, never touches `Session`'s search state or the
    /// network.
    ///
    /// Memoized in `filter_cache`: this is called several times per redraw
    /// (cursor bounds, row rendering, ...) and again on every cursor move
    /// even though nothing about the filter changed, so re-filtering and
    /// re-sorting the whole list from scratch each time made j/k
    /// noticeably slow on a big list. Recomputed only when the screen, the
    /// query text, or the source list's length has actually changed.
    fn filtered_tracks(&self, s: &Session, screen: usize) -> Option<Vec<core::Track>> {
        let query = self.active_filter()?;
        if query.is_empty() || !self.filterable_screen(screen) {
            return None;
        }
        let screen = norm_screen(screen);
        let source_len = self.filterable_source_len(s, screen);
        let list_id = (self.open_playlist, self.open_remote.clone().map(|(sid, _, node)| (sid, node)));

        if let Some(cache) = self.filter_cache.lock().unwrap().as_ref()
            && cache.screen == screen
            && cache.list_id == list_id
            && cache.query == query
            && cache.source_len == source_len
        {
            return Some(cache.result.clone());
        }

        let tracks = self.all_tracks_for_screen(s, screen);
        let mut ranked: Vec<(core::Track, FilterRank)> = tracks
            .into_iter()
            .filter_map(|t| rank_filter(&self.filter_matcher, &t.main(), query).map(|r| (t, r)))
            .collect();
        ranked.sort_by(|a, b| a.1.cmp(&b.1));
        let result: Vec<core::Track> = ranked.into_iter().map(|(t, _)| t).collect();

        *self.filter_cache.lock().unwrap() = Some(FilterCache {
            screen,
            list_id,
            query: query.to_string(),
            source_len,
            result: result.clone(),
        });
        Some(result)
    }

    /// The track selected on `screen` — usually `active_screen()` (whichever
    /// list keyboard nav currently targets), but `self.screen` for things
    /// that only ever care about the main content (e.g. `Command::Seek`'s
    /// track-agnostic bindings don't call this at all, but a few `:`-command
    /// resolutions want "the row the user is looking at" specifically).
    fn selected_track(&self, s: &Session, screen: usize) -> Option<TrackId> {
        let screen = norm_screen(screen);
        let ids = self.visible_track_ids(s, screen);
        ids.get(self.cursor[screen]).copied()
    }

    /// Which screen index keyboard nav/selection currently targets: the
    /// focused pane's own list if it has one (a docked Queue/History), else
    /// the main content's. Rendering the main content itself is unaffected
    /// by this — it always shows `self.screen`, regardless of focus.
    fn active_screen(&self) -> usize {
        match self.focus {
            Focus::Pane(p) => list_screen_for_pane(p).unwrap_or(self.screen),
            Focus::Main | Focus::Warnings => self.screen,
        }
    }

    /// Plays row `idx` of `screen`'s track list, same as pressing Enter on
    /// it while selected — the shared target for both `Event::Key(Key::Enter)`
    /// and a double-click (`click_row`). A no-op if `idx` isn't actually a
    /// track row (e.g. the Playlists screen's own top-level list of
    /// playlists, which Enter opens via `Action::Activate` instead).
    fn play_track_at(&mut self, screen: usize, idx: usize) -> EventResult {
        let screen = norm_screen(screen);
        let (tracks, sel, name) = self.with_session(|s| {
            let tracks = self.visible_track_ids(s, screen);
            let sel = tracks.get(idx).copied();
            (tracks, sel, self.context_name(s, screen))
        });
        let Some(id) = sel else {
            return EventResult::consumed();
        };
        let index = tracks.iter().position(|t| *t == id).unwrap_or(0);
        // Only the Playlists screen's own remote-browse state is ever
        // meaningful here — a docked Queue/History pane plays from its own
        // plain track list.
        let remote = if screen == PLAYLISTS {
            self.open_remote.as_ref().map(|(sid, _, node)| (sid.clone(), node.clone()))
        } else {
            None
        };
        self.run(Command::PlayContext { tracks, index, remote, name })
    }

    /// `screen`'s track list's display name — whatever this call site
    /// already knows to be the source of the tracks it's about to hand
    /// `Command::PlayContext` (a playlist name, a remote folder's name,
    /// "Search results", ...), for Now Playing's title. `None` where there's
    /// no natural name (e.g. re-jumping within the Now Playing list itself
    /// just keeps its current name).
    fn context_name(&self, s: &Session, screen: usize) -> Option<String> {
        match screen {
            NOW_PLAYING => s.playing_context_name(),
            SEARCH => Some("Search results".to_string()),
            QUEUE => Some("Queue".to_string()),
            HIST => Some("History".to_string()),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlists().into_iter().find(|p| p.id == id).map(|p| p.name)
                } else {
                    self.open_remote.as_ref().map(|(_, name, _)| name.clone())
                }
            }
            _ => None,
        }
    }

    /// Selects row `idx` of `screen`'s list; if this is a second click on
    /// the very same row within `DOUBLE_CLICK_WINDOW`, activates it — the
    /// mouse counterpart of highlighting a row then pressing Enter. Goes
    /// through the very same `keybindings::map("Enter", ...)` dispatch the
    /// real Enter key uses, so a double-click on a non-track row (e.g. the
    /// Playlists screen's own top-level list of playlists) still opens it
    /// via `Action::Activate`, instead of `play_track_at` alone silently
    /// no-op'ing on rows that aren't tracks.
    fn click_row(&mut self, screen: usize, idx: usize) -> EventResult {
        let screen = norm_screen(screen);
        self.cursor[screen] = idx;
        self.clamp_scroll();
        let now = Instant::now();
        if is_double_click(self.last_click, now, screen, idx) {
            self.last_click = None; // don't let a third click chain into another
            let sel = self.with_session(|s| self.selected_track(s, screen));
            match keybindings::map("Enter", sel, &self.hotkeys_map()) {
                Action::PlayFromContext(_) => self.play_track_at(screen, idx),
                action => self.handle_action(action),
            }
        } else {
            self.last_click = Some((now, screen, idx));
            EventResult::consumed()
        }
    }

    /// The local playlist selected/open on the Playlists screen. `None` while
    /// a remote (e.g. Spotify) folder is open or selected — those aren't
    /// exportable as a local m3u.
    fn selected_playlist(&self, s: &Session) -> Option<PlaylistId> {
        if norm_screen(self.screen) != PLAYLISTS {
            return None;
        }
        if let Some(id) = self.open_playlist {
            return Some(id);
        }
        if self.open_remote.is_some() {
            return None;
        }
        match self.top_rows(s).get(self.cursor[PLAYLISTS]) {
            Some(TopRow::Local(id)) => Some(*id),
            _ => None,
        }
    }

    /// The playlist (local *or* remote) selected/open on the Playlists
    /// screen — same idea as `selected_playlist`, but widened to
    /// `HotkeyTarget` since, unlike `ExportM3u`, binding a hotkey to a
    /// remote playlist makes perfect sense.
    fn selected_hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        if norm_screen(self.screen) != PLAYLISTS {
            return None;
        }
        if let Some(id) = self.open_playlist {
            return Some(HotkeyTarget::Local(id));
        }
        if let Some((sid, _, node)) = &self.open_remote {
            return Some(HotkeyTarget::Remote(sid.clone(), node.clone()));
        }
        self.top_rows(s).into_iter().nth(self.cursor[PLAYLISTS]).map(|r| r.target())
    }

    /// `offset`/`limit` bound what actually gets resolved into `Row`s — only
    /// the visible window, not the whole underlying list, however big it's
    /// grown (Spotify Liked Songs can run into the thousands). The returned
    /// `String` is just `screen`'s *detail* — already punctuated (e.g.
    /// `" (3 tracks)"`, `": foo"`), empty when there's none — never the
    /// screen's name itself; callers combine it with `screen_name`/
    /// `draw_tab_bar` so the name is spelled out in exactly one place.
    fn rows(&self, s: &Session, screen: usize, offset: usize, limit: usize) -> (String, Vec<Row>) {
        let screen = norm_screen(screen);
        if let Some(matched) = self.filtered_tracks(s, screen) {
            let query = self.active_filter().unwrap_or_default();
            let total = matched.len();
            let rows = if total == 0 {
                vec![plain_row(format!("no matches for {query:?}"))]
            } else {
                tracks_to_rows(s, matched.into_iter().skip(offset).take(limit).collect())
            };
            let detail = format!(" — filter {query:?} ({total} match{})", if total == 1 { "" } else { "es" });
            return (detail, rows);
        }
        match screen {
            NOW_PLAYING => {
                let len = s.playing_context_len();
                if len == 0 {
                    // Nothing has ever been played this session — nothing
                    // to show a tracklist of yet.
                    let rows = vec![plain_row(
                        "nothing played yet — press Enter on a track to start playing",
                    )];
                    (String::new(), rows)
                } else {
                    let detail = match s.playing_context_name() {
                        Some(name) => format!(": {name}"),
                        None => format!(" ({len} tracks)"),
                    };
                    (detail, tracks_to_rows(s, s.playing_context_window(offset, limit)))
                }
            }
            SEARCH => {
                let detail = if self.editing == Editing::Search {
                    format!(" {}", self.search_line())
                } else {
                    format!(": {}", self.search_line())
                };
                let rows = if s.results_len() == 0 {
                    match &self.last_query {
                        // A search ran and came back empty — say so, instead
                        // of looking identical to "nobody searched yet".
                        Some(q) => vec![plain_row(format!(
                            "no results for {q:?} — check the Log pane (:log) for source errors"
                        ))],
                        None => vec![],
                    }
                } else {
                    tracks_to_rows(s, s.results_window(offset, limit))
                };
                (detail, rows)
            }
            QUEUE => {
                let detail = format!(" ({} tracks)", s.queue_len());
                (detail, tracks_to_rows(s, s.queue_window(offset, limit)))
            }
            HIST => {
                let detail = format!(" ({} tracks)", s.queue.history_len());
                (detail, tracks_to_rows(s, s.history_window(offset, limit)))
            }
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    let name = s
                        .playlists()
                        .into_iter()
                        .find(|p| p.id == id)
                        .map(|p| p.name)
                        .unwrap_or_default();
                    (
                        format!(": {name}  (Esc to go back)"),
                        tracks_to_rows(s, s.playlist_window(id, offset, limit)),
                    )
                } else if let Some((sid, name, node)) = &self.open_remote {
                    let mut rows = tracks_to_rows(s, s.remote_playlist_window(sid, node, offset, limit));
                    // Room left on this page (i.e. the real tail was
                    // reached) — append tracks still mid-add so they don't
                    // look like the add silently failed while the source's
                    // playlist fetch catches up (see `pending_remote_adds`).
                    for track_name in s.pending_remote_adds(sid, node) {
                        if rows.len() >= limit {
                            break;
                        }
                        rows.push(plain_row(format!("{track_name}  (adding…)")));
                    }
                    (
                        format!(": [{sid}] {name}  (Esc to go back)"),
                        rows,
                    )
                } else {
                    // Was unwindowed — mismatched draw()'s `idx = i + offset`.
                    let rows = self
                        .top_rows(s)
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(|row| {
                            let key = s.playlist_hotkey(&row.target());
                            let mut r = match &row {
                                TopRow::Local(id) => {
                                    let p = s.playlists().into_iter().find(|p| p.id == *id);
                                    let name = p.as_ref().map(|p| p.name.clone()).unwrap_or_default();
                                    let count = p.map(|p| p.items.len()).unwrap_or(0);
                                    plain_row(format!("{name}  ({count} tracks)"))
                                }
                                TopRow::Remote(sid, name, _) => plain_row(format!("[{sid}] {name}")),
                            };
                            r.hotkeys = Cell::plain(key.map(String::from).unwrap_or_default());
                            r
                        })
                        .collect();
                    (String::new(), rows)
                }
            }
            _ => (String::new(), vec![]),
        }
    }

    /// The current screen's full list length — cheap (`_len` accessors, no
    /// window resolved), for the scrollbar thumb. Mirrors `rows`'s screen
    /// dispatch but each arm reports a count instead of resolving rows.
    fn list_len(&self, s: &Session, screen: usize) -> usize {
        let screen = norm_screen(screen);
        if self.filterable_screen(screen) {
            return self.visible_track_ids(s, screen).len();
        }
        match screen {
            NOW_PLAYING => s.playing_context_len(),
            SEARCH => s.results_len(),
            QUEUE => s.queue_len(),
            HIST => s.queue.history_len(),
            PLAYLISTS => self.top_rows(s).len(),
            _ => 0,
        }
    }

    fn search_line(&self) -> String {
        if self.editing == Editing::Search {
            // The command bar below already echoes `self.buffer` as it's typed.
            "(Esc to cancel)".to_string()
        } else {
            String::new()
        }
    }

    // ---- key handling ---------------------------------------------------

    fn clamp_cursor(&mut self, len: usize) {
        let c = &mut self.cursor[norm_screen(self.screen)];
        if len == 0 {
            *c = 0;
        } else if *c >= len {
            *c = len - 1;
        }
    }

    /// The screen index Shift-J/Shift-K and PageUp/PageDown's cursor-jump
    /// should act on: `self.screen` while `Focus::Main`, or a focused
    /// Queue/History pane's own list — `None` while focus is on a non-list
    /// pane (Log/Settings/Vis, jumped via `scroll_pane` instead) or the
    /// warnings button.
    fn active_list_screen(&self) -> Option<usize> {
        match self.focus {
            Focus::Main => Some(norm_screen(self.screen)),
            Focus::Pane(pane) => list_screen_for_pane(pane),
            Focus::Warnings => None,
        }
    }

    /// Shift-J/Shift-K and PageUp/PageDown on the main tracklist or a
    /// focused Queue/History pane — `Ignored` when `active_list_screen`
    /// finds nothing scrollable there (so the key can fall through to
    /// e.g. a hotkey lookup instead of being silently swallowed).
    fn jump_list(&mut self, up: bool, step: usize) -> EventResult {
        let Some(screen) = self.active_list_screen() else { return EventResult::Ignored };
        let len = self.with_session(|s| self.list_len(s, screen));
        let view_h = match self.focus {
            Focus::Pane(pane) => self
                .last_pane_rects
                .iter()
                .find(|(p, _)| *p == pane)
                .map(|&(_, rect)| rect.height().saturating_sub(1))
                .unwrap_or(0),
            _ => self.list_h(),
        };
        CursorWindow { cursor: &mut self.cursor[screen], offset: &mut self.list_offset[screen] }
            .jump(up, step, len, view_h);
        EventResult::consumed()
    }

    /// `clamp_cursor`, generalized to an explicit `screen` (for a focused
    /// docked Queue/History pane, whose list isn't `self.screen`) and
    /// folding in the forward step + length lookup — used by `Down` on a
    /// focused list-pane.
    fn bump_pane_cursor(&mut self, screen: usize, step: usize) {
        let screen = norm_screen(screen);
        self.cursor[screen] = self.cursor[screen].saturating_add(step);
        let len = self.with_session(|s| self.visible_track_ids(s, screen).len());
        let c = &mut self.cursor[screen];
        if len == 0 {
            *c = 0;
        } else if *c >= len {
            *c = len - 1;
        }
    }

    /// Visible list rows, from the last layout pass. `last_main_rect`
    /// already excludes the top/bottom chrome bars (see `split`), so its
    /// full height is available list content — no further reservation.
    fn list_h(&self) -> usize {
        self.last_main_rect.height().saturating_sub(1)
    }

    /// Keep `list_offset[screen]` a valid window around `cursor[screen]`:
    /// scroll up just enough to bring the cursor back into view if it moved
    /// above the window, or down just enough if it moved below — otherwise
    /// leave it alone, so the cursor is free to move within an already-
    /// visible window without the window itself shifting. Call after
    /// anything that might move the cursor (keyboard nav) or change
    /// `last_main_rect`. Deliberately NOT called after a mouse wheel scroll
    /// (`handle_mouse`) — that's the one case where the window should move
    /// without the cursor following it.
    fn clamp_scroll(&mut self) {
        self.clamp_scroll_for(self.screen, self.list_h());
        // A focused docked list-pane (Queue/History) has its own cursor and
        // scroll window, sized to its own rect (`last_pane_rects`) rather
        // than the main content's — clamp that one too, if applicable.
        if let Focus::Pane(pane) = self.focus
            && let Some(screen) = list_screen_for_pane(pane)
            && let Some(&(_, rect)) = self.last_pane_rects.iter().find(|(p, _)| *p == pane)
        {
            self.clamp_scroll_for(screen, rect.height().saturating_sub(1));
        }
    }

    /// Reset the current screen's cursor/scroll to the top — called on
    /// every `/`-filter keystroke, since narrowing (or widening) the list
    /// can leave the old position meaningless. Simplest option consistent
    /// with the rest of this file's "reclamp after anything that changes
    /// list identity" convention, rather than trying to keep a specific
    /// track selected across an edit.
    fn reset_filter_selection(&mut self) {
        let screen = norm_screen(self.screen);
        self.cursor[screen] = 0;
        self.list_offset[screen] = 0;
    }

    fn clamp_scroll_for(&mut self, screen: usize, list_h: usize) {
        let screen = norm_screen(screen);
        self.list_offset[screen] =
            follow_cursor_offset(self.cursor[screen], self.list_offset[screen], list_h);
    }

    /// Bounds-only safety clamp for `list_offset[screen]` — keeps it inside
    /// `0..=len.saturating_sub(list_h)` so a list that shrank out from under
    /// an existing scroll position (e.g. a track removed, a playlist
    /// emptied) can't leave the window pointing past the end and rendering
    /// blank rows. Deliberately never reads `cursor` — unlike
    /// `clamp_scroll_for`, it must NOT pull the window back to the
    /// selection, so it's safe to call unconditionally on every layout pass
    /// (including a plain terminal resize) without undoing an intentional
    /// wheel-scroll away from the cursor. See `required_size`.
    fn clamp_offset_bounds(&mut self, screen: usize, list_h: usize) {
        let screen = norm_screen(screen);
        let len = self.with_session(|s| self.list_len(s, screen));
        self.list_offset[screen] = bound_offset(self.list_offset[screen], len, list_h);
    }

    /// Move `warnings_cursor` by `step` rows (arrow/wheel/PageUp-PageDown/
    /// Shift-J-Shift-K alike all funnel through this), keeping
    /// `warnings_offset` following it via `CursorWindow` — see `Scrollable`.
    fn jump_warnings(&mut self, up: bool, step: usize) {
        let n = self.with_session(|s| s.plugin_statuses().len());
        let h = self.warnings_list_h(self.last_screen_size.y);
        CursorWindow { cursor: &mut self.warnings_cursor, offset: &mut self.warnings_offset }
            .jump(up, step, n, h);
    }

    /// Same idea as `jump_warnings`, for the hotkey-menu modal.
    fn jump_hotkey_menu(&mut self, up: bool, step: usize) {
        let n = self.hotkey_rows().len();
        self.hotkey_menu_cursor = stepped_cursor(self.hotkey_menu_cursor, n, up, step);
        self.follow_hotkey_menu_offset();
    }

    /// Same idea as `jump_warnings`, for the "Add to Playlist" picker.
    fn jump_playlist_picker(&mut self, up: bool, step: usize) {
        let n = self.with_session(|s| s.playlists().len());
        let h = modal_list_h(self.last_screen_size.y, PLAYLIST_PICKER_LIST_TOP);
        CursorWindow { cursor: &mut self.playlist_picker_cursor, offset: &mut self.playlist_picker_offset }
            .jump(up, step, n, h);
    }

    /// Resync `warnings_offset` to `warnings_cursor` without moving the
    /// cursor — called on a resize, unlike `jump_warnings` which is nav-only.
    fn follow_warnings_offset(&mut self) {
        let h = self.warnings_list_h(self.last_screen_size.y);
        CursorWindow { cursor: &mut self.warnings_cursor, offset: &mut self.warnings_offset }.follow(h);
    }

    /// Same idea as `follow_warnings_offset`, for the hotkey menu.
    fn follow_hotkey_menu_offset(&mut self) {
        let h = modal_list_h(self.last_screen_size.y, HOTKEY_LIST_TOP);
        self.hotkey_menu_offset = follow_cursor_offset(self.hotkey_menu_cursor, self.hotkey_menu_offset, h);
    }

    /// Same idea as `follow_warnings_offset`, for the "Add to Playlist" picker.
    fn follow_playlist_picker_offset(&mut self) {
        let h = modal_list_h(self.last_screen_size.y, PLAYLIST_PICKER_LIST_TOP);
        CursorWindow { cursor: &mut self.playlist_picker_cursor, offset: &mut self.playlist_picker_offset }
            .follow(h);
    }

    // ---- plugin warnings (Spotify/SoundCloud login status) --------------

    fn open_warnings(&mut self) {
        self.warnings_open = true;
        self.warnings_cursor = 0;
        self.warnings_offset = 0;
    }

    /// Number of plugins currently reporting a non-`Ok` health — the source
    /// of truth for whether the bottom-row warnings button exists at all.
    fn warn_count(&self) -> usize {
        self.with_session(|s| s.plugin_statuses().iter().filter(|(_, h)| !h.is_ok()).count())
    }

    /// `Enter` (or a click) on the selected warnings-modal row: run the
    /// plugin's `setup()` directly if it needs no input, or start collecting
    /// one via `Editing::PluginSetup` if it does.
    fn activate_selected_warning(&mut self) {
        let Some((id, _)) =
            self.with_session(|s| s.plugin_statuses().into_iter().nth(self.warnings_cursor))
        else {
            return;
        };
        let Some(plugin) = self.with_session(|s| s.plugin(&id)) else {
            return;
        };
        match plugin.setup_kind() {
            SetupKind::Action => self.run_plugin_setup(id, None),
            SetupKind::TextInput { .. } => {
                self.buffer.clear();
                self.editing = Editing::PluginSetup(id);
            }
        }
    }

    // ---- help / shortcuts -------------------------------------------------

    fn open_help(&mut self) {
        self.help_open = true;
        self.help_scroll = 0;
    }

    // ---- add-to-playlist picker --------------------------------------

    fn open_playlist_picker(&mut self, track: TrackId) {
        self.playlist_picker_open = true;
        self.playlist_picker_cursor = 0;
        self.playlist_picker_offset = 0;
        self.playlist_picker_track = Some(track);
    }

    /// Enter (or a click) on the selected picker row: adds
    /// `playlist_picker_track` to that playlist and closes the picker.
    /// A no-op (just closes) if the list is empty or the track got lost.
    fn commit_playlist_picker(&mut self) -> EventResult {
        let track = self.playlist_picker_track.take();
        let playlist =
            self.with_session(|s| s.playlists().into_iter().nth(self.playlist_picker_cursor).map(|p| p.id));
        self.playlist_picker_open = false;
        self.focus = self.fallback_focus();
        match (track, playlist) {
            (Some(track), Some(playlist)) => self.run(Command::AddToPlaylist { track, playlist }),
            _ => EventResult::consumed(),
        }
    }

    // ---- playlist hotkeys ------------------------------------------------

    fn open_hotkey_menu(&mut self) {
        self.hotkey_menu_open = true;
        self.hotkey_menu_cursor = 0;
        self.hotkey_menu_offset = 0;
        self.hotkey_capture = None;
        self.hotkey_feedback = None;
        self.with_session(|s| s.clear_membership_feedback());
    }

    /// Enter on the selected hotkey-menu row: opens the "press a key to
    /// bind" sub-popup for that built-in action.
    fn open_hotkey_capture(&mut self) {
        let Some(&action) = self.hotkey_rows().get(self.hotkey_menu_cursor) else {
            return;
        };
        self.hotkey_capture = Some(HotkeyTarget::Builtin(action));
        self.hotkey_feedback = None;
    }

    /// Opens the standalone "press a key to bind" modal for `target` on the
    /// Playlists screen — backtick's local override of `Action::
    /// OpenHotkeyMenu` there (see `on_event`), bypassing the hotkey menu
    /// entirely since it no longer handles playlist targets.
    fn open_playlist_hotkey_modal(&mut self, target: HotkeyTarget) {
        self.hotkey_capture = Some(target);
        self.hotkey_feedback = None;
    }

    /// This row's display name, looked up fresh — used by `bind_captured_key`
    /// for both the target being bound and whatever it stole a key from.
    fn hotkey_row_name_for(&self, target: &HotkeyTarget) -> String {
        match target {
            HotkeyTarget::Builtin(action) => action.label().to_string(),
            HotkeyTarget::Local(_) | HotkeyTarget::Remote(..) => self.with_session(|s| {
                let playlists = s.playlists();
                self.top_rows(s).into_iter().find(|r| &r.target() == target).map(|r| top_row_name(&r, &playlists))
            }).unwrap_or_default(),
        }
    }

    /// Binds `key` to whichever target `hotkey_capture` names, reports the
    /// result (bound, moved-from-another-row, or refused because `key` is a
    /// still-unremapped or explicitly-remapped built-in) in `hotkey_feedback`,
    /// and closes the sub-popup.
    fn bind_captured_key(&mut self, key: char) -> EventResult {
        let Some(target) = self.hotkey_capture.take() else {
            return EventResult::consumed();
        };
        let result = self.with_session_mut(|s| s.bind_hotkey(key, target.clone()));
        let name = self.hotkey_row_name_for(&target);
        self.hotkey_feedback = Some(match result {
            Ok(Some(stolen_from)) => {
                let stolen_name = self.hotkey_row_name_for(&stolen_from);
                format!("Bound '{key}' to {name} (moved from {stolen_name})")
            }
            Ok(None) => format!("Bound '{key}' to {name}"),
            Err(blocking) => {
                let blocking_name = self.hotkey_row_name_for(&blocking);
                format!("Can't bind '{key}': already used by built-in {blocking_name}")
            }
        });
        EventResult::consumed()
    }

    /// Backspace on the selected hotkey-menu row: clears that row's binding,
    /// if it has one — reverts a built-in to its default key rather than
    /// leaving it unreachable.
    fn clear_selected_hotkey(&mut self) -> EventResult {
        let Some(&action) = self.hotkey_rows().get(self.hotkey_menu_cursor) else {
            return EventResult::consumed();
        };
        self.clear_hotkey(HotkeyTarget::Builtin(action))
    }

    /// Backspace on the standalone playlist hotkey modal: clears whichever
    /// target `hotkey_capture` names instead of binding a new key, and
    /// closes the modal — same gesture/underlying `unbind_hotkey` as
    /// `clear_selected_hotkey`, just reached from the capture step directly
    /// since the Playlists screen has no row-list step of its own.
    fn clear_captured_hotkey(&mut self) -> EventResult {
        let Some(target) = self.hotkey_capture.take() else {
            return EventResult::consumed();
        };
        self.clear_hotkey(target)
    }

    /// Shared by `clear_selected_hotkey`/`clear_captured_hotkey`: unbinds
    /// `target` and reports the result in `hotkey_feedback`.
    fn clear_hotkey(&mut self, target: HotkeyTarget) -> EventResult {
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        self.hotkey_feedback = key.map(|k| format!("Unbound '{k}'"));
        EventResult::consumed()
    }

    /// Run a plugin's `setup()` on a background thread — never on the
    /// caller's (this one, the UI/event thread): `setup()` is allowed to
    /// block (an OAuth browser flow, a network call), and this thread only
    /// re-takes the session lock briefly, once at the start (to clone the
    /// `Arc<dyn Plugin>` handle) and once at the end (to `apply_wiring` the
    /// result) — never held across the blocking call itself. Mirrors
    /// `core::app::ensure_remote_playlist_tracks`'s lock-briefly/work-
    /// unlocked/lock-briefly pattern.
    fn run_plugin_setup(&self, id: SourceId, input: Option<String>) {
        let session = self.session.clone();
        let bus = self.with_session(|s| s.bus.clone());
        thread::spawn(move || {
            let Some(plugin) = session.lock().unwrap().plugin(&id) else {
                return;
            };
            let health = plugin.setup(input);
            let succeeded = health.is_ok();
            let wiring = plugin.wiring();
            let mut guard = session.lock().unwrap();
            guard.apply_wiring(&id, wiring);
            // See `Session::plugin_statuses` — this is what lets a genuine
            // setup failure surface instead of `probe()`'s generic text.
            guard.record_setup_result(id, health);
            drop(guard);
            // The UI's cue to redraw the warnings panel / pick up whatever
            // just got registered — see `CoreEvent::PluginStatusChanged`.
            bus.send(CoreEvent::PluginStatusChanged);
            if succeeded {
                bus.send(CoreEvent::PluginLoginSucceeded);
            }
        });
    }

    /// Run a plugin-registered `:`-command (e.g. `:spotify addlogin`) on a
    /// background thread — same never-block-the-UI-thread reasoning as
    /// `run_plugin_setup`. Also sends `PluginStatusChanged` so the fresh
    /// auth state gets rewired in, not just the modal shown below.
    fn run_plugin_command(&self, plugin: Arc<dyn Plugin>, word: String, arg: Option<String>) -> EventResult {
        let feedback = self.with_session(|s| s.plugin_command_result_handle());
        let bus = self.with_session(|s| s.bus.clone());
        thread::spawn(move || {
            let msg = plugin.run_command(&word, arg);
            bus.send(CoreEvent::PluginStatusChanged);
            *feedback.lock().unwrap() = Some(msg);
            bus.send(CoreEvent::PluginCommandResult);
        });
        EventResult::consumed()
    }

    /// Mouse handling for the main list — kept entirely separate from the
    /// keyboard path in `on_event`. `None` means "not ours" (outside
    /// `last_main_rect`, or an event we don't handle) so the caller can fall
    /// through to whatever else might want it. Always tried regardless of
    /// current focus — a click *sets* focus to wherever it landed (here:
    /// `Focus::Main`), it doesn't require already being there; see
    /// `on_event`'s mouse dispatch, which tries this then each open pane's
    /// own rect in turn.
    fn handle_mouse(&mut self, offset: Vec2, position: Vec2, event: MouseEvent) -> Option<EventResult> {
        let local = position.checked_sub(offset)?;
        let rect = self.last_main_rect;
        let (rx, ry) = (rect.top_left().x, rect.top_left().y);
        if local.x < rx || local.x >= rx + rect.width() || local.y < ry || local.y >= ry + rect.height() {
            return None;
        }
        self.focus = Focus::Main;
        let screen = norm_screen(self.screen);
        match event {
            // Scrolls the *window* only — `cursor` is untouched, so it can
            // end up off-screen indefinitely (the user's own explicit
            // choice: a wheel scroll is "let me look elsewhere", not "move
            // the selection"). Nothing reclamps this — only an actual
            // cursor change (nav keys, a click, the list changing under it)
            // calls `clamp_scroll` and brings the window back.
            MouseEvent::WheelUp => {
                let off = &mut self.list_offset[screen];
                *off = off.saturating_sub(WHEEL_STEP);
                Some(EventResult::consumed())
            }
            MouseEvent::WheelDown => {
                let list_h = self.list_h();
                let len = self.with_session(|s| self.list_len(s, screen));
                let max_off = len.saturating_sub(list_h);
                let off = &mut self.list_offset[screen];
                *off = (*off + WHEEL_STEP).min(max_off);
                Some(EventResult::consumed())
            }
            // `rect`'s own row 0 is the list's title row (see
            // `draw_row_list`) — not clickable as a list row — so row 1 is
            // the first actual list row.
            MouseEvent::Press(MouseButton::Left) => {
                let row = local.y - ry;
                if row == 0 || row > self.list_h() {
                    return Some(EventResult::consumed());
                }
                let idx = self.list_offset[screen] + (row - 1);
                let len = self.with_session(|s| self.list_len(s, screen));
                if idx < len {
                    return Some(self.click_row(screen, idx));
                }
                Some(EventResult::consumed())
            }
            _ => None,
        }
    }

    /// Mouse handling for one open pane's rect — `handle_mouse`'s
    /// counterpart for the dock instead of the main content. `None` means
    /// "not this pane" (outside `rect`) so `on_event` can try the next one.
    /// A hit focuses `pane` regardless of what was focused before, same as
    /// `handle_mouse` does for the main content.
    fn handle_pane_mouse(
        &mut self,
        pane: Pane,
        rect: Rect,
        offset: Vec2,
        position: Vec2,
        event: MouseEvent,
    ) -> Option<EventResult> {
        let local = position.checked_sub(offset)?;
        let (rx, ry) = (rect.top_left().x, rect.top_left().y);
        if local.x < rx || local.x >= rx + rect.width() || local.y < ry || local.y >= ry + rect.height() {
            return None;
        }
        // Log is free-form terminal output the user wants to select/copy
        // with the mouse (e.g. an error string to paste elsewhere) — a
        // click/drag there is left unhandled (not even a focus change)
        // instead of being consumed for pane focus, so it never looks like
        // the app ate a selection drag. The wheel still scrolls it, same as
        // any other pane.
        if pane == Pane::Log && matches!(event, MouseEvent::Press(_) | MouseEvent::Hold(_) | MouseEvent::Release(_))
        {
            return None;
        }
        self.focus = Focus::Pane(pane);

        let Some(screen) = list_screen_for_pane(pane) else {
            // Log/Settings/Vis: no per-row click target, but the wheel
            // still scrolls (Log/Settings) or is simply absorbed (Vis, a
            // live view with nothing to scroll — mirrors `scroll_pane`).
            match event {
                MouseEvent::WheelUp => self.scroll_pane(pane, true, WHEEL_STEP),
                MouseEvent::WheelDown => self.scroll_pane(pane, false, WHEEL_STEP),
                _ => {}
            }
            return Some(EventResult::consumed());
        };

        let pane_h = rect.height().saturating_sub(1);
        let mut result = EventResult::consumed();
        match event {
            MouseEvent::WheelUp => {
                let off = &mut self.list_offset[screen];
                *off = off.saturating_sub(WHEEL_STEP);
            }
            MouseEvent::WheelDown => {
                let len = self.with_session(|s| self.list_len(s, screen));
                let max_off = len.saturating_sub(pane_h);
                let off = &mut self.list_offset[screen];
                *off = (*off + WHEEL_STEP).min(max_off);
            }
            // Row 0 of `rect` is the title; the list body is rows 1..=pane_h.
            MouseEvent::Press(MouseButton::Left) => {
                let row = local.y - ry;
                if row != 0 && row <= pane_h {
                    let idx = self.list_offset[screen] + (row - 1);
                    let len = self.with_session(|s| self.list_len(s, screen));
                    if idx < len {
                        result = self.click_row(screen, idx);
                    }
                }
            }
            _ => {}
        }
        Some(result)
    }

    fn commit_edit(&mut self) -> EventResult {
        let kind = std::mem::replace(&mut self.editing, Editing::None);
        let text = std::mem::take(&mut self.buffer);
        match kind {
            Editing::Search => {
                self.last_query = if text.trim().is_empty() { None } else { Some(text.clone()) };
                self.run(Command::Search(text))
            }
            Editing::CommandLine => {
                let parsed = match command::parse(&text) {
                    Ok(p) => p,
                    Err(e) => return popup(e),
                };
                let open = self.open_playlist;
                if parsed == command::Parsed::Help {
                    self.open_help();
                    return EventResult::consumed();
                }
                // `Screen` mode: fullscreen, one at a time — drawn by
                // `MedleyView` itself (see `draw`/`on_event`), not a
                // separate cursive layer, so it's the same live pane (Log
                // tail, Vis's worker frame, ...) the embedded case uses,
                // just sized to the whole terminal.
                if let command::Parsed::TogglePane(pane) = parsed {
                    self.toggle_pane(pane);
                    return self.vis_fps_cb();
                }
                if command::Parsed::Vis == parsed {
                    self.toggle_pane(Pane::Vis);
                    return self.vis_fps_cb();
                }
                if command::Parsed::History == parsed {
                    self.screen = HIST;
                    self.leave_playlists();
                    self.filter_query = None;
                    self.clamp_scroll();
                    return EventResult::consumed();
                }
                if command::Parsed::Keys == parsed {
                    self.open_hotkey_menu();
                    return EventResult::consumed();
                }
                if let command::Parsed::SetPaneLayout(patch) = parsed {
                    // `side`/`stack` stay shared layout geometry regardless
                    // of `patch.pane` — only `mode` (screen vs. embedded) is
                    // ever per-pane (see `pane_mode_overrides`'s doc).
                    if let Some(side) = patch.side {
                        self.pane_cfg.side = side;
                    }
                    if let Some(stack) = patch.stack {
                        self.pane_cfg.stack = stack;
                    }
                    if let Some(mode) = patch.mode {
                        match patch.pane {
                            Some(pane) => {
                                self.pane_mode_overrides.insert(pane, mode);
                            }
                            None => self.pane_cfg.mode = mode,
                        }
                    }
                    self.clamp_focus();
                    return EventResult::consumed();
                }
                if parsed == command::Parsed::OpenBrowse {
                    let session = self.session.clone();
                    let start =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    return EventResult::with_cb(move |siv| {
                        crate::filebrowser::open(siv, session.clone(), open, start.clone());
                    });
                }
                if let command::Parsed::Open(arg) = parsed {
                    return self.open_arg(arg, open);
                }
                if let command::Parsed::PluginCommand { word, arg } = parsed {
                    let plugin = self.with_session(|s| s.plugin_for_command(&word));
                    return match plugin {
                        Some(plugin) => self.run_plugin_command(plugin, word, arg),
                        None => popup(format!("unknown command: {word}")),
                    };
                }
                let cmd = self.with_session(|s| {
                    let sel = self.selected_track(s, self.active_screen());
                    command::resolve(parsed, s, sel)
                });
                match cmd {
                    Ok(c) => self.run(c),
                    Err(e) => popup(e),
                }
            }
            Editing::PluginSetup(id) => {
                self.run_plugin_setup(id, Some(text));
                EventResult::consumed()
            }
            Editing::Filter => {
                self.filter_query = if text.trim().is_empty() { None } else { Some(text) };
                EventResult::consumed()
            }
            Editing::None => EventResult::Ignored,
        }
    }

    fn run(&mut self, cmd: Command) -> EventResult {
        // `Previous` only grows the queue when it actually wedges the
        // just-played track back onto the front (see `Session::dispatch`) —
        // a before/after length diff tells us that without core needing to
        // report it explicitly.
        let tracks_queue_len = matches!(cmd, Command::Enqueue(_) | Command::Wedge(_) | Command::Previous)
            .then(|| self.with_session(|s| s.queue_len()));
        let feedback_kind = match &cmd {
            Command::Enqueue(_) => Some("Queued"),
            Command::Wedge(_) => Some("Wedged"),
            Command::Previous => Some("Wedged"),
            _ => None,
        };
        let is_toggle_shuffle = matches!(cmd, Command::ToggleShuffle);
        let is_toggle_scan = matches!(cmd, Command::ToggleScan);
        let res = self.with_session_mut(|s| s.dispatch(cmd));
        if matches!(res, Ok(Dispatch::Ok)) {
            if let (Some(before), Some(kind)) = (tracks_queue_len, feedback_kind) {
                let after = self.with_session(|s| s.queue_len());
                if kind == "Queued" || after > before {
                    self.queue_feedback = Some(format!("  {kind}: {after} tracks"));
                }
            }
            // Flash feedback for the two now-clickable status-line tags.
            if is_toggle_shuffle {
                let on = self.with_session(|s| s.shuffle());
                self.queue_feedback = Some(format!("  Shuffle: {}", if on { "on" } else { "off" }));
            } else if is_toggle_scan {
                let label = self.with_session(|s| s.scan.as_ref().map(|scan| scan.mode()));
                let label = match label {
                    Some(core::ScanMode::Active) => "active",
                    Some(core::ScanMode::CacheOnly) => "cache-only",
                    Some(core::ScanMode::Disabled) | None => "off",
                };
                self.queue_feedback = Some(format!("  Scan: {label}"));
            }
        }
        match res {
            Ok(Dispatch::Ok) => EventResult::consumed(),
            Ok(Dispatch::Quit) => EventResult::with_cb(|c: &mut Cursive| c.quit()),
            Ok(Dispatch::Modal(m)) => popup(m),
            Err(e) => popup(e.to_string()),
        }
    }

    /// `F` (`Action::ConfirmUnlike`): a Yes/No cursive dialog — same
    /// `Dialog` styling as `popup`'s `Dialog::info`, just with two buttons —
    /// so a stray capital-F press can't silently remove a track from Liked
    /// Songs. Only "Remove" actually dispatches `Command::Unlike`; "Cancel"
    /// (`dismiss_button`) just pops the layer.
    fn confirm_unlike(&mut self, id: TrackId) -> EventResult {
        let name = self
            .with_session(|s| s.store.get_track(id).ok().flatten())
            .map(|t| format!("{} - {}", t.display_artist(), t.title))
            .unwrap_or_else(|| "this track".to_string());
        let session = self.session.clone();
        EventResult::with_cb(move |c: &mut Cursive| {
            let session = session.clone();
            let dialog = Dialog::text(format!("Remove {name:?} from Liked Songs?"))
                .title("Unlike")
                .button("Remove", move |c| {
                    let _ = session.lock().unwrap().dispatch(Command::Unlike(id));
                    c.pop_layer();
                })
                .dismiss_button("Cancel");
            c.add_layer(dialog);
        })
    }

    /// `:open <url-or-path>`'s remote-link case — open a pasted playlist
    /// link on the Playlists screen, if some registered source recognizes
    /// it and can browse it (e.g. a public Spotify playlist — no extra auth
    /// needed, the same user token already covers any public playlist by
    /// id).
    fn open_playlist_uri(&mut self, uri: String) -> EventResult {
        let found = self.with_session(|s| {
            let source = s.source_for_uri(&uri)?;
            let node = source.browse_uri(&uri)?;
            Some((source.id(), node))
        });
        match found {
            Some((sid, node)) => {
                let name = match &node {
                    core::BrowseNode::Path(id) => id.clone(),
                    core::BrowseNode::Root => String::new(),
                };
                self.screen = PLAYLISTS;
                self.open_playlist = None;
                self.open_remote = Some((sid, name, node));
                self.cursor[PLAYLISTS] = 0;
                self.filter_query = None; // a different list now — stale filter would be confusing
                self.clamp_scroll(); // new list under an old (now meaningless) cursor
                EventResult::consumed()
            }
            None => popup(format!("no source can open this as a playlist: {uri:?}")),
        }
    }

    /// `:open <url-or-path>`'s argument case: a source-recognized URI opens
    /// a playlist link, else `arg` is a local path — `.m3u`/`.m3u8` by
    /// extension imports, anything else adds to the open playlist.
    fn open_arg(&mut self, arg: String, open: Option<PlaylistId>) -> EventResult {
        if self.with_session(|s| s.source_for_uri(&arg).is_some()) {
            return self.open_playlist_uri(arg);
        }
        let lower = arg.to_ascii_lowercase();
        if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
            return self.run(Command::ImportM3u(std::path::PathBuf::from(arg.trim())));
        }
        let paths = command::split_paths(&arg);
        self.run(Command::AddFilesToPlaylist { playlist: open, paths })
    }

    fn activate(&mut self) -> EventResult {
        if norm_screen(self.screen) == PLAYLISTS && self.open_playlist.is_none() && self.open_remote.is_none() {
            let row = self.with_session(|s| {
                self.top_rows(s).into_iter().nth(self.cursor[PLAYLISTS])
            });
            match row {
                Some(TopRow::Local(id)) => {
                    self.open_playlist = Some(id);
                    self.cursor[PLAYLISTS] = 0;
                    self.filter_query = None;
                    self.clamp_scroll(); // opened a new list — old window is meaningless
                    return EventResult::consumed();
                }
                Some(TopRow::Remote(sid, name, node)) => {
                    self.open_remote = Some((sid, name, node));
                    self.cursor[PLAYLISTS] = 0;
                    self.filter_query = None;
                    self.clamp_scroll();
                    return EventResult::consumed();
                }
                None => {}
            }
        }
        EventResult::Ignored
    }

    fn handle_action(&mut self, action: Action) -> EventResult {
        match action {
            Action::Command(c) => self.run(c),
            // On the Search screen itself, same as switching to the Search
            // tab (focuses the input too). Everywhere else, `/` starts a
            // screen-local fuzzy filter over the list already on screen
            // instead of jumping away to run a remote search.
            Action::FocusSearch => {
                if norm_screen(self.screen) == SEARCH {
                    self.handle_action(Action::Screen(SEARCH))
                } else {
                    self.editing = Editing::Filter;
                    self.buffer.clear();
                    EventResult::consumed()
                }
            }
            Action::CommandLine => {
                self.editing = Editing::CommandLine;
                self.buffer.clear();
                EventResult::consumed()
            }
            Action::Screen(n) => {
                let was_playlists = self.screen == PLAYLISTS;
                self.screen = n;
                // Leaving the Playlists screen for anything else (Now
                // Playing included — it no longer aliases Playlists, see
                // `NOW_PLAYING`'s doc comment) drops whatever playlist/
                // remote folder was open, same as it always has for
                // Queue/History/Search — but remembers it (local or remote)
                // so switching back to Playlists (below) can restore it.
                if n != PLAYLISTS {
                    self.leave_playlists();
                } else if !was_playlists && self.open_playlist.is_none() && self.open_remote.is_none() {
                    // Switching back into Playlists fresh (not already on it,
                    // nothing already opened by some other path): restore
                    // whatever was open before, unless it was a local
                    // playlist that's been deleted/renamed away since — fall
                    // back to the top-level list rather than resurrect a
                    // dead id.
                    let resolved = self
                        .with_session(|s| resolve_remembered_playlist(self.remembered_playlist.clone(), &s.playlists()));
                    match resolved {
                        Some(RememberedPlaylist::Local(id)) => self.open_playlist = Some(id),
                        Some(RememberedPlaylist::Remote(sid, name, node)) => {
                            self.open_remote = Some((sid, name, node))
                        }
                        None => {}
                    }
                }
                // A different screen's list — any filter over the old one
                // is meaningless now.
                self.filter_query = None;
                // Switching to Search should focus the input immediately,
                // same as `/` (`Action::FocusSearch`), so typing works right away.
                if n == SEARCH {
                    self.editing = Editing::Search;
                    self.buffer.clear();
                }
                self.clamp_scroll();
                EventResult::consumed()
            }
            Action::Activate => self.activate(),
            // The `Event::Key(Key::Enter)` handler already special-cases
            // this (it has the visible list on hand for free there, from
            // deriving `selected` in the first place) and never forwards it
            // here — this is just a sane fallback for the variant existing.
            Action::PlayFromContext(id) => self.run(Command::Play(id)),
            Action::OpenHotkeyMenu => {
                self.open_hotkey_menu();
                EventResult::consumed()
            }
            Action::OpenHelp => {
                self.open_help();
                EventResult::consumed()
            }
            Action::AddToPlaylistPrompt(id) => {
                self.open_playlist_picker(id);
                EventResult::consumed()
            }
            Action::NewPlaylistPrompt => {
                self.editing = Editing::CommandLine;
                self.buffer = "newplaylist ".to_string();
                EventResult::consumed()
            }
            Action::ConfirmUnlike(id) => self.confirm_unlike(id),
            Action::CyclePaneLayout => {
                let cur = (self.pane_cfg.side, self.pane_cfg.stack);
                let next = PANE_LAYOUT_CYCLE.iter().position(|&c| c == cur).map_or(0, |i| (i + 1) % PANE_LAYOUT_CYCLE.len());
                (self.pane_cfg.side, self.pane_cfg.stack) = PANE_LAYOUT_CYCLE[next];
                EventResult::consumed()
            }
            Action::None => EventResult::Ignored,
        }
    }
}

/// Content for the help/shortcuts screen: `command::HELP` (the `:` command
/// table), then `keybindings::RAW_KEYS` (the raw keybinding table), then —
/// if any exist — each bound playlist hotkey. `playlist_hotkeys` comes
/// pre-resolved to `(char, playlist name)` pairs so this stays pure (no
/// `Session` access), and thus unit-testable.
fn build_help_lines(
    playlist_hotkeys: &[(char, String)],
    builtin_remaps: &[(char, String)],
    plugin_commands: &[(String, String)],
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("Commands".to_string());
    lines.push(String::new());
    for (name, desc) in command::HELP {
        lines.push(format!("  :{name}"));
        lines.push(format!("      {desc}"));
    }
    for (word, desc) in plugin_commands {
        lines.push(format!("  :{word}"));
        lines.push(format!("      {desc}"));
    }
    lines.push(String::new());
    lines.push("Keyboard shortcuts (defaults — see :keys for the live, remappable list)".to_string());
    lines.push(String::new());
    for (key, desc) in keybindings::RAW_KEYS {
        lines.push(format!("  {key:<10} {desc}"));
    }
    if !builtin_remaps.is_empty() {
        lines.push(String::new());
        lines.push("Remapped built-in keys (:keys)".to_string());
        lines.push(String::new());
        for (ch, name) in builtin_remaps {
            lines.push(format!("  {ch:<10} {name}"));
        }
    }
    if !playlist_hotkeys.is_empty() {
        lines.push(String::new());
        lines.push("Playlist hotkeys".to_string());
        lines.push(String::new());
        for (ch, name) in playlist_hotkeys {
            lines.push(format!("  {ch:<10} {name}"));
        }
    }
    lines
}

/// Which `Row` column a `render_cell` call is producing.
enum Column {
    Tags,
    Main,
    Hotkeys,
    Source,
    Duration,
}

/// The single per-column cell renderer: given a track and whatever context a
/// column might need, produces that column's `Cell` (text + optional color
/// override). `Tags` (bpm tempo-coloring) and `Hotkeys` (hotkey-playlist
/// membership) are the two columns that actually use `visible`/`hotkeys`
/// today; `Main`/`Source`/`Duration` just pass the track's own field through
/// unstyled — but every column goes through this one function, so a future
/// context-driven cell (for any column) is just another match arm here,
/// without `draw_list_body`/`five_col`/`Row` changing again.
fn render_cell(
    col: Column,
    t: &core::Track,
    cached: bool,
    visible: &[String],
    hotkeys: &[(char, HashSet<TrackId>)],
) -> Cell {
    match col {
        Column::Tags => {
            let color = visible.iter().find_map(|attr| t.attrs.get(attr).and_then(|v| tag_color(attr, v)));
            Cell { text: t.tags(visible), color }
        }
        Column::Main => Cell::plain(t.main()),
        Column::Source => Cell::plain(t.source(cached)),
        Column::Duration => Cell::plain(t.duration()),
        Column::Hotkeys => {
            let mut chars: Vec<char> = hotkeys.iter().filter(|(_, ids)| ids.contains(&t.id)).map(|(ch, _)| *ch).collect();
            chars.sort_unstable();
            Cell::plain(chars.into_iter().collect::<String>())
        }
    }
}

fn tracks_to_rows(s: &Session, tracks: Vec<core::Track>) -> Vec<Row> {
    // Resolved once per call rather than once per row: `is_current` used to
    // take `&Session` and re-fetch the whole now-playing `Track` (a store
    // round trip) on every row, every redraw — for a list running into the
    // thousands (Spotify Liked Songs) that alone was enough to lock up the
    // UI while scrolling. `hotkey_playlist_membership` is the same idea for
    // the hotkeys column: build every hotkey-bound playlist's membership set
    // once per call, not once per row.
    let now_playing = s.now_playing_id();
    let visible = &s.cfg.visible_track_attrs;
    let hotkeys = hotkey_playlist_membership(s);
    tracks
        .into_iter()
        .map(|t| {
            let cached = s.is_track_cached(&t);
            Row {
                tags: render_cell(Column::Tags, &t, cached, visible, &hotkeys),
                main: render_cell(Column::Main, &t, cached, visible, &hotkeys),
                hotkeys: render_cell(Column::Hotkeys, &t, cached, visible, &hotkeys),
                source: render_cell(Column::Source, &t, cached, visible, &hotkeys),
                duration: render_cell(Column::Duration, &t, cached, visible, &hotkeys),
                current: t.is_current(now_playing),
            }
        })
        .collect()
}

/// `(hotkey char, member track ids)` for every currently hotkey-bound
/// playlist, excluding synthetic ones (e.g. Spotify's "Liked Songs") — the
/// hotkeys column's per-track lookup table, built once per `tracks_to_rows`
/// call. A local playlist's membership is always fully known; a remote
/// (e.g. Spotify) playlist's set only has however much of it has loaded into
/// `ViewCache` so far — reading `Session::remote_playlist_track_ids` (its
/// `want=0` form, the same one cursor-bounds checks use on every keypress)
/// so a hotkey-bound remote playlist actually starts loading in the
/// background instead of staying permanently blank until the user happens
/// to browse to it; `ensure_remote_playlist_tracks` only kicks a fetch that
/// isn't already in flight and never blocks the caller.
fn hotkey_playlist_membership(s: &Session) -> Vec<(char, HashSet<TrackId>)> {
    s.hotkeys()
        .into_iter()
        .filter_map(|(ch, target)| {
            let ids = match &target {
                HotkeyTarget::Local(id) => Some(s.playlist_track_ids(*id)),
                HotkeyTarget::Remote(sid, node) => {
                    if s.is_synthetic_playlist(sid, node) {
                        None
                    } else {
                        Some(s.remote_playlist_track_ids(sid, node))
                    }
                }
                // Not a playlist — nothing to show in the hotkeys column.
                HotkeyTarget::Builtin(_) => None,
            }?;
            Some((ch, ids.into_iter().collect()))
        })
        .collect()
}

/// Per-tag-attr cell renderer, keyed by attr name (a `Config::visible_track_attrs`
/// entry): given the track's raw value for that attr, returns a color
/// override for its cell in the tags column, or `None` to leave it in the
/// row's normal color. `bpm` is the only registered case today; a future
/// attr-driven cell (e.g. an "energy" bar/color) is just another match arm
/// here, called from `render_cell`'s `Column::Tags` arm.
fn tag_color(attr: &str, value: &str) -> Option<Color> {
    match attr {
        "bpm" => bpm_color(value),
        _ => None,
    }
}

/// Colors bpm by tempo on a continuous blue→green→red gradient (cool/slow to
/// hot/fast), clamped to a 60-180 bpm range roughly spanning ballads/hip-hop
/// through DnB/hardstyle, with green sitting at the house/pop midpoint (120).
fn bpm_color(bpm: &str) -> Option<Color> {
    const LOW: f64 = 60.0;
    const MID: f64 = 120.0;
    const HIGH: f64 = 180.0;
    const COOL: (u8, u8, u8) = (60, 110, 220);
    const NEUTRAL: (u8, u8, u8) = (90, 200, 90);
    const HOT: (u8, u8, u8) = (220, 60, 60);

    let bpm: f64 = bpm.parse().ok()?;
    let bpm = bpm.clamp(LOW, HIGH);
    let (a, b, t) = if bpm < MID {
        (COOL, NEUTRAL, (bpm - LOW) / (MID - LOW))
    } else {
        (NEUTRAL, HOT, (bpm - MID) / (HIGH - MID))
    };
    let lerp = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    Some(Color::Rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2)))
}

fn popup(msg: impl Into<String>) -> EventResult {
    let msg = msg.into();
    EventResult::with_cb(move |c: &mut Cursive| {
        c.add_layer(Dialog::info(msg.clone()));
    })
}

/// Partitions the full screen into the main content rect and one rect per
/// currently-open **embedded** pane (in `open_panes` order). `open_panes`
/// only ever holds panes whose own placement (`MedleyView::pane_mode`) is
/// currently `Embedded` — `toggle_pane` is what enforces that — so unlike
/// before per-pane placement existed, this no longer needs to re-check a
/// single global mode: a `Screen`-mode pane (Log/Settings/Vis's
/// `screen_pane` overlay, or a list-pane's `self.screen` switch) never
/// ends up in `open_panes` in the first place.
///
/// `TAB_BAR_ROWS`/`BOTTOM_BAR_ROWS` are carved off `total` *before* any of
/// the per-`Side` math below runs, so every returned rect (main and every
/// pane, `Side::Left`/`Right` included) is confined to the band between
/// them — a pane can never claim the title/tab-bar row or the bottom two
/// (cmdline/hint + player-status) rows, regardless of placement.
fn split(total: Vec2, open_panes: &[Pane], cfg: PaneLayoutConfig) -> (Rect, Vec<(Pane, Rect)>) {
    let band = Vec2::new(total.x, total.y.saturating_sub(TAB_BAR_ROWS + BOTTOM_BAR_ROWS));
    if open_panes.is_empty() {
        return (Rect::from_size((0, TAB_BAR_ROWS), band), Vec::new());
    }
    // One cell is reserved between main and the pane block for the "│"/"─"
    // `draw()` prints there — otherwise it lands on the pane's own leading
    // row/column and chops its first character.
    const GUTTER: usize = 1;
    let n = open_panes.len();

    let (main, panes) = match cfg.side {
        // Left/Right: pane block is a narrow column alongside main, full
        // band height. Multiple panes divide that column per `cfg.stack`.
        Side::Left | Side::Right => {
            let avail = band.x.saturating_sub(GUTTER);
            // Fixed fraction, floored so it never eats the whole screen; MVP —
            // no per-pane resizing yet. Degenerate (avail < 2) just squeezes
            // to nothing rather than underflowing.
            let extent = if avail < 2 { avail } else { (avail / 3).clamp(1, avail - 1) };
            let main_w = avail - extent;
            let (main_x, side_x) = if cfg.side == Side::Left {
                (extent + GUTTER, 0)
            } else {
                (0, main_w + GUTTER)
            };
            let main = Rect::from_size((main_x, 0), (main_w, band.y));

            let panes: Vec<(Pane, Rect)> = open_panes
                .iter()
                .enumerate()
                .map(|(i, &pane)| {
                    let rect = match cfg.stack {
                        Axis::Vertical => {
                            let h = band.y / n;
                            let y = i * h;
                            let h = if i + 1 == n { band.y - y } else { h }; // last slot eats rounding
                            Rect::from_size((side_x, y), (extent, h))
                        }
                        Axis::Horizontal => {
                            let w = extent / n;
                            let x = side_x + i * w;
                            let w = if i + 1 == n { extent - i * w } else { w };
                            Rect::from_size((x, 0), (w, band.y))
                        }
                    };
                    (pane, rect)
                })
                .collect();
            (main, panes)
        }
        // Top/Bottom: pane block is a full-width bar above/below main.
        // Multiple panes divide that bar per `cfg.stack`.
        Side::Top | Side::Bottom => {
            let avail = band.y.saturating_sub(GUTTER);
            let extent = if avail < 2 { avail } else { (avail / 3).clamp(1, avail - 1) };
            let main_h = avail - extent;
            let (main_y, side_y) = if cfg.side == Side::Top {
                (extent + GUTTER, 0)
            } else {
                (0, main_h + GUTTER)
            };
            let main = Rect::from_size((0, main_y), (band.x, main_h));

            let panes: Vec<(Pane, Rect)> = open_panes
                .iter()
                .enumerate()
                .map(|(i, &pane)| {
                    let rect = match cfg.stack {
                        // Side by side across the full width — "split down
                        // the middle" for two panes.
                        Axis::Horizontal => {
                            let w = band.x / n;
                            let x = i * w;
                            let w = if i + 1 == n { band.x - x } else { w };
                            Rect::from_size((x, side_y), (w, extent))
                        }
                        Axis::Vertical => {
                            let h = extent / n;
                            let y = side_y + i * h;
                            let h = if i + 1 == n { extent - i * h } else { h };
                            Rect::from_size((0, y), (band.x, h))
                        }
                    };
                    (pane, rect)
                })
                .collect();
            (main, panes)
        }
    };

    let shift = |r: Rect| Rect::from_size((r.top_left().x, r.top_left().y + TAB_BAR_ROWS), r.size());
    (shift(main), panes.into_iter().map(|(p, r)| (p, shift(r))).collect())
}

fn pane_title(pane: Pane) -> &'static str {
    match pane {
        Pane::Log => "Log",
        Pane::Settings => "Settings",
        Pane::Vis => "Vis",
        Pane::Queue => "Queue",
        Pane::History => "History",
    }
}

/// One row of the Settings pane: plain info text, or a togglable bool.
#[derive(Clone)]
enum SettingsEntry {
    Info(String),
    /// One of `TOGGLABLE_SOURCES` — config-only, takes effect next restart.
    Source { name: &'static str, enabled: bool },
    /// Background scan on/off — live via `ScanDriver::set_mode`, unlike `Source`.
    Scan { enabled: bool, available: bool },
}

fn settings_entry_line(e: &SettingsEntry) -> String {
    match e {
        SettingsEntry::Info(s) => s.clone(),
        SettingsEntry::Source { name, enabled } => format!("[{}] {name}", if *enabled { "x" } else { " " }),
        SettingsEntry::Scan { enabled, available: true } => {
            format!("[{}] bpm scan", if *enabled { "x" } else { " " })
        }
        SettingsEntry::Scan { available: false, .. } => "[ ] bpm scan (unavailable)".to_string(),
    }
}

/// Effective config as togglable/info rows, for both the embedded pane and
/// the screen-mode modal. `pane_cfg` is the view's live layout state, not
/// `s.cfg.panes` — `P`/`:panes` update it in the view only (MVP, not
/// persisted to `cfg`), so reading `cfg.panes` here would show a stale value
/// until the next full config reload.
fn settings_entries(s: &Session, pane_cfg: PaneLayoutConfig) -> Vec<SettingsEntry> {
    let cfg = &s.cfg;
    let mut v = vec![
        SettingsEntry::Info(format!("theme:            {}", cfg.theme)),
        SettingsEntry::Info(format!("initial_screen:   {}", cfg.initial_screen)),
        SettingsEntry::Info(format!("volume:           {:.0}%", s.player_status().volume * 100.0)),
        SettingsEntry::Info(format!("http.roots:       {}", cfg.http.roots.len())),
        SettingsEntry::Info(format!("http.recurse:     {}", cfg.http.recurse_depth)),
    ];
    for name in TOGGLABLE_SOURCES {
        v.push(SettingsEntry::Source { name, enabled: cfg.source_enabled(name).unwrap_or(false) });
    }
    v.push(SettingsEntry::Scan {
        enabled: s.scan.as_ref().is_some_and(|d| d.mode() != ScanMode::Disabled),
        available: s.scan.is_some(),
    });
    v.push(SettingsEntry::Info(String::new()));
    v.push(SettingsEntry::Info(format!("panes.mode:       {:?}", pane_cfg.mode)));
    v.push(SettingsEntry::Info(format!("panes.side:       {:?}", pane_cfg.side)));
    v.push(SettingsEntry::Info(format!("panes.stack:      {:?}", pane_cfg.stack)));
    v
}

/// Settings pane's title + rows, with a highlight on `cursor` — the
/// `SettingsEntry` analogue of `draw_pane` (Log's wrap-based line scroll
/// doesn't apply here: one entry is always exactly one row).
fn draw_settings_pane(printer: &Printer, entries: &[SettingsEntry], offset: usize, cursor: usize, focused: bool) {
    let mut title = pane_title(Pane::Settings).to_string();
    if focused {
        title = format!("[{title}]");
    }
    printer.with_color(ColorStyle::title_secondary(), |p| {
        p.print((0, 0), &pad(&title, p.size.x));
    });
    let width = printer.size.x;
    let h = printer.size.y.saturating_sub(1);
    for (i, entry) in entries.iter().enumerate().skip(offset).take(h) {
        let y = 1 + (i - offset);
        let line = pad(&settings_entry_line(entry), width);
        if i == cursor {
            printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
        } else {
            printer.print((0, y), &line);
        }
    }
}

/// A title bar plus a window of track rows and a scrollbar — the main
/// content area's rendering, factored out so a docked Queue/History pane
/// draws with exactly the same look, just into its own (already-windowed,
/// already-sized-to-its-rect) `printer`. `rows` is already the resolved
/// `offset..offset+list_h` window (see `MedleyView::rows`); `sel` is the
/// absolute index of the highlighted row, `total` the full list length for
/// the scrollbar thumb.
fn draw_row_list(printer: &Printer, title: &str, rows: &[Row], offset: usize, sel: usize, total: usize) {
    // Reserve the rightmost column of the list body as a scrollbar gutter —
    // always present so there's somewhere to show "how far into a many-
    // thousand-row list (Liked Songs) am I", which `sel`/`offset` alone
    // don't convey.
    let content_w = printer.size.x.saturating_sub(1);
    printer.with_color(ColorStyle::title_primary(), |p| {
        p.print((0, 0), &pad(title, content_w));
    });
    let body_h = printer.size.y.saturating_sub(1);
    let body = printer.windowed(Rect::from_size((0, 1), (printer.size.x, body_h)));
    draw_list_body(&body, rows, offset, sel, total);
}

/// Fills every row of `printer` with `rows` (the caller's already-resolved
/// visible window) plus a scrollbar gutter — no title row of its own.
/// `draw_row_list` prints one separately and passes a printer windowed past
/// it; the main content's own row 0 is the global tab bar (`draw_tab_bar`),
/// drawn on the whole-screen printer rather than `main_rect`, so `draw`
/// passes `main_rect` here directly.
fn draw_list_body(printer: &Printer, rows: &[Row], offset: usize, sel: usize, total: usize) {
    let content_w = printer.size.x.saturating_sub(1);
    let list_h = printer.size.y;
    for (i, row) in rows.iter().enumerate() {
        let y = i;
        let idx = i + offset;
        let mark = if row.current { "> " } else { "  " };
        let line = format!(
            "{mark}{}",
            five_col(
                &row.tags.text,
                &row.main.text,
                &row.hotkeys.text,
                &row.source.text,
                &row.duration.text,
                content_w.saturating_sub(2),
            )
        );
        let line = pad(&line, content_w);
        if idx == sel {
            printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
        } else if row.current {
            printer.with_color(ColorStyle::secondary(), |p| p.print((0, y), &line));
        } else {
            printer.print((0, y), &line);
            // Selection/now-playing highlight above takes the whole line —
            // a per-cell color only shows through on an otherwise-plain row.
            for (start, width, right_aligned, cell) in column_layout(content_w.saturating_sub(2), row) {
                if let Some(color) = cell.color {
                    let text = if right_aligned {
                        pad_right_aligned(&cell.text, width)
                    } else {
                        pad(&cell.text, width)
                    };
                    printer.with_color(ColorStyle::front(color), |p| {
                        p.print((mark.width() + start, y), &text)
                    });
                }
            }
        }
    }
    draw_scrollbar(printer, content_w, list_h, offset, total);
}

/// Each column's `(start column, width, right-aligned)` within a `width`-wide
/// `five_col` line, plus the row's own `Cell` for it — the color-overlay
/// counterpart to `five_col`'s text layout, so the two can never drift
/// apart. Empty when `width` is too narrow for `five_col` to lay out columns
/// at all (its "just the main column" fallback).
fn column_layout(width: usize, row: &Row) -> Vec<(usize, usize, bool, &Cell)> {
    let fixed = TAGS_COL_W + SOURCE_COL_W + DURATION_COL_W + HOTKEYS_COL_W + 4;
    if width <= fixed {
        return Vec::new();
    }
    let main_w = width - fixed;
    let tags_start = 0;
    let main_start = TAGS_COL_W + 1;
    let hotkeys_start = main_start + main_w + 1;
    let source_start = hotkeys_start + HOTKEYS_COL_W + 1;
    let duration_start = source_start + SOURCE_COL_W + 1;
    vec![
        (tags_start, TAGS_COL_W, true, &row.tags),
        (main_start, main_w, false, &row.main),
        (hotkeys_start, HOTKEYS_COL_W, false, &row.hotkeys),
        (source_start, SOURCE_COL_W, false, &row.source),
        (duration_start, DURATION_COL_W, false, &row.duration),
    ]
}

/// The main content's row-0 tabs, `(screen, bare name)`, in both display
/// and hotkey order: 1 Now Playing, 2 Playlists, 3 Search, 4 History,
/// 5 Queue. Shared between rendering and click hit-testing (`tab_layout`)
/// so they can't drift apart.
const TABS: [(usize, &str); 5] = [
    (NOW_PLAYING, "Now Playing"),
    (PLAYLISTS, "Playlists"),
    (SEARCH, "Search"),
    (HIST, "History"),
    (QUEUE, "Queue"),
];

/// Background for the active tab only — every other tab uses the
/// terminal's default colors, unstyled.
const ACTIVE_TAB_BG: Color = Color::Dark(BaseColor::Red);

/// `screen`'s bare tab name — the one place that maps a screen to its
/// display name, shared by the tab strip and the docked Queue/History pane
/// title so the name is never spelled out twice.
fn screen_name(screen: usize) -> &'static str {
    TABS.iter().find(|&&(s, _)| s == screen).map_or("", |&(_, name)| name)
}

/// A tab's rendered button text: its 1-based hotkey number plus name,
/// padded with a leading/trailing space so the active tab's background
/// highlight doesn't hug the text. `collapsed` (too narrow for full labels —
/// see `tab_layout`) shows just the name's first letter instead.
fn tab_label(index: usize, name: &str, collapsed: bool) -> String {
    if collapsed {
        let letter = name.chars().next().unwrap_or('?');
        format!(" {letter} ")
    } else {
        format!(" [{}] {name} ", index + 1)
    }
}

/// Total width of every tab label plus the gaps between them, for the given
/// `collapsed` mode — the threshold `draw_tab_bar`/`tab_at_x` collapse at.
fn tabs_width(collapsed: bool) -> usize {
    let gap = 1;
    TABS.iter().enumerate().map(|(i, &(_, label))| tab_label(i, label, collapsed).chars().count()).sum::<usize>()
        + gap * TABS.len().saturating_sub(1)
}

/// Each tab's screen, start column and width, left-aligned starting at
/// column 0 with a 1-column gap between tabs — pure (independent of the
/// screen width; overflow past it is the caller's problem, see
/// `draw_tab_bar`/`tab_at_x`) so both agree on where each tab sits without
/// duplicating the layout. `collapsed`: single-letter labels instead of the
/// full `"[N] Name"` form — see `tab_label`.
fn tab_layout(collapsed: bool) -> Vec<(usize, usize, usize)> {
    let widths: Vec<usize> =
        TABS.iter().enumerate().map(|(i, &(_, label))| tab_label(i, label, collapsed).chars().count()).collect();
    let gap = 1;
    let mut x = 0;
    TABS
        .iter()
        .zip(widths.iter())
        .enumerate()
        .map(|(i, (&(screen, _), &w))| {
            let start = x;
            x += w;
            if i + 1 < TABS.len() {
                x += gap;
            }
            (screen, start, w)
        })
        .collect()
}

/// Which tab (if any) occupies column `x` of a tab bar `width` columns
/// wide — `None` over the gap/detail area. Collapse state is a pure function
/// of `width` (see `draw_tab_bar`), so a click always agrees with what was
/// last drawn there.
fn tab_at_x(x: usize, width: usize, state: &PlayerState) -> Option<usize> {
    let collapsed = tab_bar_collapsed(width, state);
    tab_layout(collapsed)
        .into_iter()
        .find(|&(_, start, w)| x >= start && x < start + w && start < width)
        .map(|(screen, ..)| screen)
}

/// Whether the tab bar's full `"[N] Name"` labels have to collapse to
/// single letters (see `tab_label`) to leave room for the transport-button
/// strip (`transport_layout`) right after them within `content_w` — the one
/// place this trade-off is decided, shared by `draw_tab_bar`/`tab_at_x`/
/// `transport_at_x` so all three always agree on the current layout.
fn tab_bar_collapsed(content_w: usize, state: &PlayerState) -> bool {
    let gap = TRANSPORT_GAP;
    let transport_w = transport_layout(0, state).last().map_or(0, |&(_, s, w)| s + w);
    tabs_width(false) + gap + transport_w > content_w
}

/// One of the top-bar's transport buttons, drawn right after the tabs (see
/// `transport_layout`/`draw_tab_bar`) — click targets for the same commands
/// as the `<`/`Space`/`>` keys.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Prev,
    PlayPause,
    Next,
}

impl Transport {
    fn command(self) -> Command {
        match self {
            Transport::Prev => Command::Previous,
            Transport::PlayPause => Command::PlayPause,
            Transport::Next => Command::Next,
        }
    }
}

/// Prev/next glyphs — shared by the top-bar transport strip
/// (`transport_labels`) and the bottom status line's own prev/next buttons
/// (`draw`) so both always show the same icons.
const PREV_ICON: &str = "⏮";
const NEXT_ICON: &str = "⏭";

/// Gap on either side of the top-bar transport cluster — 2 columns, matching
/// the padding already inside each button, so the strip reads as evenly
/// spaced. Shared by `tab_bar_collapsed`/`draw_tab_bar`/`transport_at_x`.
const TRANSPORT_GAP: usize = 2;

/// The three transport buttons' text, space-padded like `tab_label` — the
/// middle one is `player_action_glyph`, the action a press would take.
fn transport_labels(state: &PlayerState) -> [(Transport, String); 3] {
    [
        (Transport::Prev, format!(" {PREV_ICON} ")),
        (Transport::PlayPause, format!(" {} ", player_action_glyph(state))),
        (Transport::Next, format!(" {NEXT_ICON} ")),
    ]
}

/// Transport buttons' start column and width, packed left-to-right from
/// `start` with no gap between them (they already carry their own padding —
/// see `transport_labels`) — shared by `draw_tab_bar` and `transport_at_x`
/// so a click always agrees with what was last drawn.
fn transport_layout(start: usize, state: &PlayerState) -> Vec<(Transport, usize, usize)> {
    let mut x = start;
    transport_labels(state)
        .into_iter()
        .map(|(button, label)| {
            let w = label.width();
            let s = x;
            x += w;
            (button, s, w)
        })
        .collect()
}

/// Which transport button (if any) occupies column `x`, given the current
/// tab-bar `width` and `active`/`detail`-independent tab collapse state —
/// `None` when `x` isn't over a button, or the buttons weren't drawn at all
/// because they didn't fit (mirrors `draw_tab_bar`'s own fit check).
fn transport_at_x(x: usize, width: usize, state: &PlayerState) -> Option<Transport> {
    let collapsed = tab_bar_collapsed(width, state);
    let tabs_end = tab_layout(collapsed).last().map_or(0, |&(_, start, w)| start + w);
    let start = tabs_end + TRANSPORT_GAP;
    let layout = transport_layout(start, state);
    let end = layout.last().map_or(start, |&(_, s, w)| s + w);
    if end > width {
        return None;
    }
    layout.into_iter().find(|&(_, s, w)| x >= s && x < s + w).map(|(b, ..)| b)
}

/// Row 0 of the whole screen — fixed, full width, drawn on the raw
/// (unwindowed) printer regardless of any open pane (see `split`): the
/// screen tabs left-aligned from column 0 — default colors, except the
/// active tab gets a red background with white text — and `marquee` (the
/// now-playing text, scrolled by `marquee_offset` real-time columns when it
/// doesn't fit) right-aligned past them. The active screen's own title lives
/// in the list view's first row instead (see `draw_row_list`), not here.
///
/// Too narrow for the tabs' full `"[N] Name"` labels: they collapse to a
/// single letter each (`tab_label`) instead of being truncated mid-label.
///
/// Between the tabs and `marquee` sits a fixed transport-button strip
/// (`⏮`/play-pause/`⏭` — see `transport_layout`), drawn whenever it fits
/// past the tabs; dropped silently otherwise (`marquee` takes priority over
/// the buttons on a very narrow screen).
fn draw_tab_bar(
    printer: &Printer,
    active: usize,
    marquee: &str,
    marquee_offset: usize,
    player_state: &PlayerState,
) {
    let content_w = printer.size.x.saturating_sub(1);
    let collapsed = tab_bar_collapsed(content_w, player_state);
    let layout = tab_layout(collapsed);

    for (i, &(screen, start, w)) in layout.iter().enumerate() {
        if start >= content_w {
            continue;
        }
        let label = TABS[i].1;
        let text: String = tab_label(i, label, collapsed).chars().take(w.min(content_w - start)).collect();
        if screen == active {
            let style = ColorStyle::new(Color::Dark(BaseColor::White), ACTIVE_TAB_BG);
            printer.with_color(style, |p| p.print((start, 0), &text));
        } else {
            printer.print((start, 0), &text);
        }
    }

    let tabs_end = layout.last().map_or(0, |&(_, start, w)| start + w);
    let gap = TRANSPORT_GAP;
    let transport_start = tabs_end + gap;
    let transport_labels = transport_labels(player_state);
    let transport = transport_layout(transport_start, player_state);
    let transport_end = transport.last().map_or(transport_start, |&(_, s, w)| s + w);
    let detail_start = if transport_end <= content_w {
        for ((_, label), &(_, start, _)) in transport_labels.iter().zip(transport.iter()) {
            printer.print((start, 0), label);
        }
        transport_end + gap
    } else {
        transport_start
    };
    if detail_start >= content_w {
        return;
    }
    let avail = content_w - detail_start;
    let text = scroll_title(marquee, avail, marquee_offset);
    if text.is_empty() {
        return;
    }
    let start = content_w - text.width();
    printer.with_color(ColorStyle::title_primary(), |p| p.print((start, 0), &text));
}

/// Updates the Log pane's pin point (`log_pin`) after `scroll` changes.
///
/// The Log pane always shows the live tail while `scroll == 0`; that's the
/// `None` case here, and it's what a fresh line should do — appear right
/// away. The moment the user scrolls away from the tail (`scroll` becomes
/// non-zero with no existing pin), this pins the view to `live_len` — the
/// snapshot length *right now* — so lines arriving afterwards don't shift
/// what's on screen; they simply queue up out of view until the user
/// scrolls back down to 0, which releases the pin again. An existing pin
/// is left untouched by further scrolling within the pinned view (only
/// `scroll` moves), and is recreated at the then-current length if the
/// user releases it and immediately re-pins.
fn log_pin_after_scroll(scroll: usize, pin: Option<usize>, live_len: usize) -> Option<usize> {
    if scroll == 0 { None } else { Some(pin.unwrap_or(live_len)) }
}

/// Length of the Log snapshot to actually render this frame: the live
/// length while following the tail (`scroll == 0`), or the pinned length
/// captured by `log_pin_after_scroll` once the user has scrolled away from
/// it — clamped to `live_len` in case the underlying `LogBuf` ring has since
/// evicted lines out from under a large pin.
fn log_visible_len(scroll: usize, pin: Option<usize>, live_len: usize) -> usize {
    if scroll == 0 { live_len } else { pin.unwrap_or(live_len).min(live_len) }
}

/// Draw a pane's title + content into its own (already-windowed) printer.
/// `scroll`: rows scrolled from the default view (Log's live tail, or
/// Settings' top).
fn draw_pane(pane: Pane, printer: &Printer, lines: &[String], scroll: usize, focused: bool) {
    let mut title = match (pane, scroll > 0) {
        (Pane::Log, true) => format!("{} (scrolled, PgDn to catch up)", pane_title(pane)),
        (Pane::Settings, true) => format!("{} (scrolled)", pane_title(pane)),
        _ => pane_title(pane).to_string(),
    };
    if focused {
        title = format!("[{title}]");
    }
    printer.with_color(ColorStyle::title_secondary(), |p| {
        p.print((0, 0), &pad(&title, p.size.x));
    });
    let width = printer.size.x;
    let wrapped: Vec<String> = lines.iter().flat_map(|l| wrap(l, width)).collect();
    let h = printer.size.y.saturating_sub(1);
    // Each *wrapped* line counts as a row, so a long line takes the space it
    // needs.
    let visible: Vec<&String> = if pane == Pane::Log {
        let total = wrapped.len();
        // Clamp to the top-most full window, so scrolling past the oldest
        // line freezes there instead of shrinking the window toward empty.
        let max_scroll = total.saturating_sub(h);
        let end = total.saturating_sub(scroll.min(max_scroll));
        let start = end.saturating_sub(h);
        wrapped[start..end].iter().collect()
    } else {
        let max_scroll = wrapped.len().saturating_sub(h);
        wrapped.iter().skip(scroll.min(max_scroll)).take(h).collect()
    };
    for (i, line) in visible.into_iter().enumerate() {
        printer.print((0, i + 1), line);
    }
}

/// Greedy word-wrap: breaks `s` into `<= width`-column segments on whitespace,
/// hard-breaking a single word longer than `width`. An empty `s` yields one
/// empty segment (so blank separator lines survive); `width == 0` yields none.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let mut chars: Vec<char> = word.chars().collect();
        loop {
            let sep = usize::from(!cur.is_empty());
            if cur.chars().count() + sep + chars.len() <= width {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.extend(chars.iter());
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                continue; // retry the same word against a fresh line
            }
            // the word alone is longer than `width` — hard-break it.
            let take = width.min(chars.len());
            let rest = chars.split_off(take);
            lines.push(chars.into_iter().collect());
            chars = rest;
            if chars.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

fn key_name(event: &Event) -> Option<String> {
    match event {
        Event::Char(' ') => Some("Space".to_string()),
        Event::Char(c) => Some(c.to_string()),
        Event::Key(Key::Enter) => Some("Enter".to_string()),
        _ => None,
    }
}

impl View for MedleyView {
    fn draw(&self, printer: &Printer) {
        if let Some(pane) = self.screen_pane {
            self.draw_screen_pane(pane, printer);
            return;
        }
        if self.warnings_open {
            self.draw_warnings(printer);
            return;
        }
        if self.hotkey_menu_open {
            self.draw_hotkey_menu(printer);
            return;
        }
        if self.hotkey_capture.is_some() {
            self.draw_playlist_hotkey_modal(printer);
            return;
        }
        if self.playlist_picker_open {
            self.draw_playlist_picker(printer);
            return;
        }
        if self.help_open {
            self.draw_help(printer);
            return;
        }

        let (main_rect, panes) = split(printer.size, &self.open_panes, self.pane_cfg);

        // Computed before the list is resolved (not after, as it used to
        // be) so `rows` only ever gets asked for the visible window — a
        // list running into the thousands (Spotify Liked Songs) must not
        // pay to resolve/format every row on every redraw, only the ones
        // actually on screen. `main_rect`'s height matches what `printer`
        // will report once windowed to it below, minus one row for the
        // list's own title row (see `draw_row_list`).
        let list_h = main_rect.height().saturating_sub(1);
        let sel = self.cursor[norm_screen(self.screen)];
        // Persisted, not recomputed from `sel` — see `list_offset`'s doc.
        // `required_size` (which runs before every `draw`, with the same
        // resolved size) keeps it clamped, so it's already a valid window
        // here.
        let offset = self.list_offset[norm_screen(self.screen)];

        // A docked Queue/History pane needs the exact same (title, Vec<Row>,
        // total) triple the main content does, just for its own rect/
        // screen/cursor — gathered alongside everything else below so it's
        // all one session lock per frame, not one per pane.
        let list_panes: Vec<(Pane, Rect, usize, usize, usize)> = panes
            .iter()
            .filter_map(|&(pane, rect)| {
                let screen = list_screen_for_pane(pane)?;
                let pane_h = rect.height().saturating_sub(1);
                Some((pane, rect, screen, self.list_offset[screen], pane_h))
            })
            .collect();

        // One lock for the whole frame: pull every session-derived value out
        // here, then render without the guard. (Nothing below re-locks or calls
        // back into cursive.) Settings pane content needs `Session::cfg`, so
        // it's gathered in the same pass rather than locking again per pane.
        let want_settings = panes.iter().any(|(p, _)| *p == Pane::Settings);
        let (
            detail,
            rows,
            total,
            st,
            np,
            bpm_tag,
            shuffle,
            settings,
            warn_count,
            pane_rows,
            membership_feedback,
            main_context_name,
        ) = self.with_session(|s| {
                let (detail, rows) = self.rows(s, self.screen, offset, list_h);
                let total = self.list_len(s, self.screen);
                let main_context_name = self.context_name(s, self.screen).filter(|_| self.screen == NOW_PLAYING);
                let now_playing = s.now_playing();
                let np = now_playing
                    .as_ref()
                    .map(|t| format!("{} - {}", t.display_artist(), t.title))
                    .unwrap_or_else(|| "nothing playing".to_string());
                let bpm_tag = bpm_status_tag(s, now_playing.as_ref());
                let shuffle = s.shuffle();
                let settings = if want_settings { settings_entries(s, self.pane_cfg) } else { Vec::new() };
                let warn_count = s.plugin_statuses().iter().filter(|(_, h)| !h.is_ok()).count();
                // Feed the scan walk "what the user is looking at" every
                // redraw (cheap, and this already runs at BASELINE_FPS) so
                // it prioritizes the visible list over arbitrary store order.
                if let Some(scan) = &s.scan {
                    let view_screen = norm_screen(self.active_screen());
                    let ids = self.visible_track_ids(s, view_screen);
                    let highlighted = self.cursor[view_screen];
                    scan.follow_view(ids, highlighted);
                }
                let pane_rows: Vec<(Pane, String, Vec<Row>, usize)> = list_panes
                    .iter()
                    .map(|&(pane, _, screen, offset, pane_h)| {
                        let (detail, rows) = self.rows(s, screen, offset, pane_h);
                        // "Now Playing" is already the tab label above it, so its own name stands in for the screen name instead of appending to it.
                        let title = match self.context_name(s, screen).filter(|_| screen == NOW_PLAYING) {
                            Some(name) => name,
                            None => format!("{}{detail}", screen_name(screen)),
                        };
                        (pane, title, rows, self.list_len(s, screen))
                    })
                    .collect();
                (
                    detail,
                    rows,
                    total,
                    s.player_status(),
                    np,
                    bpm_tag,
                    shuffle,
                    settings,
                    warn_count,
                    pane_rows,
                    s.membership_feedback(),
                    main_context_name,
                )
            });

        for &(pane, rect) in &panes {
            let focused = self.focus == Focus::Pane(pane);
            if pane == Pane::Vis {
                self.vis.draw(&printer.windowed(rect), focused);
                continue;
            }
            if let Some(screen) = list_screen_for_pane(pane) {
                let (_, title, rows, total) =
                    pane_rows.iter().find(|(p, ..)| *p == pane).expect("resolved above");
                let title = if focused { format!("[{title}]") } else { title.clone() };
                draw_row_list(
                    &printer.windowed(rect),
                    &title,
                    rows,
                    self.list_offset[screen],
                    self.cursor[screen],
                    *total,
                );
                continue;
            }
            if pane == Pane::Settings {
                draw_settings_pane(&printer.windowed(rect), &settings, self.settings_offset, self.settings_cursor, focused);
                continue;
            }
            let (lines, scroll) = match pane {
                Pane::Log => self.log_render_lines(),
                Pane::Settings | Pane::Vis | Pane::Queue | Pane::History => unreachable!("handled above"),
            };
            draw_pane(pane, &printer.windowed(rect), &lines, scroll, focused);
        }
        if !panes.is_empty() {
            // One-cell separator between main content and the pane block —
            // confined to `main_rect`'s own row/column span (the reserved
            // band, never the fixed top/bottom bars) in every `Side`.
            match self.pane_cfg.side {
                Side::Left | Side::Right => {
                    let x = if self.pane_cfg.side == Side::Left {
                        main_rect.top_left().x - 1
                    } else {
                        main_rect.top_left().x + main_rect.width()
                    };
                    let (y0, y1) = (main_rect.top_left().y, main_rect.top_left().y + main_rect.height());
                    for y in y0..y1 {
                        printer.print((x, y), "│");
                    }
                }
                Side::Top | Side::Bottom => {
                    let y = if self.pane_cfg.side == Side::Top {
                        main_rect.top_left().y - 1
                    } else {
                        main_rect.top_left().y + main_rect.height()
                    };
                    for x in 0..printer.size.x {
                        printer.print((x, y), "─");
                    }
                }
            }
        }

        // Row 0 of the whole screen — fixed, full width, never `main_rect`
        // (which panes may have narrowed) — see `split`/`draw_tab_bar`.
        let marquee_offset = {
            let mut m = self.tab_marquee.lock().unwrap();
            if m.0 != np {
                m.0 = np.clone();
                m.1 = Instant::now();
            }
            m.1.elapsed().as_secs() as usize
        };
        draw_tab_bar(printer, self.screen, &np, marquee_offset, &st.state);
        let main_title = {
            let title = match main_context_name {
                Some(name) => name,
                None => format!("{}{detail}", screen_name(self.screen)),
            };
            if self.focus == Focus::Main { format!("[{title}]") } else { title }
        };
        draw_row_list(&printer.windowed(main_rect), &main_title, &rows, offset, sel, total);

        // command / hint line (row above the status line) — also the whole
        // screen's fixed bottom band, not `main_rect`.
        let bottom = printer.size.y.saturating_sub(2);
        let line = match &self.editing {
            Editing::Search => format!("/{}", self.buffer),
            Editing::CommandLine => format!(":{}", self.buffer),
            Editing::PluginSetup(_) => format!("> {}", self.buffer),
            Editing::Filter => format!("/{}", self.buffer),
            // `queue_feedback` (this keypress only) wins over
            // `membership_feedback` (like/unlike, playlist-hotkey toggle —
            // arrives async off a background thread, so it lingers until
            // the next keypress instead of being tied to one) — previously
            // `membership_feedback` was only ever drawn inside the hotkey
            // menu, so a bare `f`/`F` outside it produced no visible
            // feedback at all. `hotkey_feedback` (bind/unbind result from the
            // Playlists screen's standalone set-hotkey modal — see
            // `open_playlist_hotkey_modal`) gets the same treatment, since
            // that modal closes itself before there's anywhere else to show it.
            Editing::None => self
                .queue_feedback
                .clone()
                .or(membership_feedback.map(|m| format!("  {m}")))
                .or(self.hotkey_feedback.clone().map(|m| format!("  {m}")))
                .unwrap_or_else(|| {
                    // The Playlists screen's own hint replaces the generic
                    // help hint when a row/open playlist can take a hotkey —
                    // `[`]`'s local override there (see `on_event`).
                    if norm_screen(self.screen) == PLAYLISTS
                        && self.with_session(|s| self.selected_hotkey_target(s)).is_some()
                    {
                        return "  [`] set hotkey".to_string();
                    }
                    let help_key = self
                        .with_session(|s| s.effective_hotkey(&HotkeyTarget::Builtin(core::BuiltinAction::OpenHelp)))
                        .map(String::from)
                        .unwrap_or_default();
                    format!("  [{help_key}] help")
                }),
        };
        printer.print((0, bottom), &pad(&line, printer.size.x));

        // status line, pinned to the very last row — see `status_line_layout`
        // for the column math and `on_event`'s mirror of it for click
        // targets. `bpm_tag`/`shuffle_tag` are always shown now, since both
        // are clickable toggles rather than passive indicators.
        let icon = player_action_glyph(&st.state);
        let curtime = ms(st.position_ms);
        let totaltime = ms(st.duration_ms);
        let bar = progress_bar(st.position_ms, st.duration_ms, STATUS_BAR_WIDTH);
        let shuffle_tag = if shuffle { "[S]" } else { "[s]" };
        let (name_w, _) = status_line_layout(
            printer.size.x,
            &StatusLineWidths {
                prev: PREV_ICON.width(),
                playpause: icon.width(),
                next: NEXT_ICON.width(),
                curtime: curtime.width(),
                bar: STATUS_BAR_WIDTH,
                totaltime: totaltime.width(),
                bpm: bpm_tag.width(),
                shuffle: shuffle_tag.width(),
            },
        );
        let title_field = pad(&scroll_title(&np, name_w, marquee_offset), name_w);
        let status = format!(
            "{PREV_ICON} {icon} {NEXT_ICON}  {title_field}  {curtime} {bar} {totaltime}  {bpm_tag} {shuffle_tag}"
        );
        let y = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, y), &pad(&status, p.size.x));
        });

        // Warnings button — right-aligned on the hint line, drawn last so it
        // overwrites that tail rather than being covered by it. Distinct
        // red background regardless of focus; focus just swaps which side
        // the color sits on (an underline-ish "this is what Tab lands on"
        // cue, same idea as `highlight`/`highlight_inactive` elsewhere).
        // Omitted entirely when there are no warnings, leaving the hint
        // line's own text in place instead of an empty/faded button.
        if warn_count > 0 {
            let label = warnings_label(warn_count);
            let label_w = label.chars().count().min(printer.size.x);
            let bx = printer.size.x - label_w;
            let (fg, bg) = (Color::Dark(BaseColor::White), Color::Dark(BaseColor::Red));
            let style = if self.focus == Focus::Warnings {
                ColorStyle::new(bg, fg)
            } else {
                ColorStyle::new(fg, bg)
            };
            printer.with_color(style, |p| p.print((bx, bottom), &label));
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        // The one layout hook that gets `&mut self` with the resolved
        // screen size — recompute `last_main_rect` here (matching `draw`'s
        // `main_rect`) so hit-testing and `list_h()` stay accurate.
        //
        // Cursive calls this on every layout pass, not just an actual
        // terminal resize or pane toggle (e.g. the periodic playback-
        // position tick redraw goes through it too), so a bare
        // `clamp_scroll()` (cursor-following) here would snap `list_offset`
        // back to the cursor's window on the very next frame after any
        // wheel scroll, undoing it almost immediately. But when a list's
        // height genuinely changed since the last pass (a resize, or a
        // pane opening/closing/resizing above/below it), the old offset can
        // leave the cursor outside the new window — and unlike a wheel
        // scroll, that's not intentional, so it must be re-clamped to the
        // cursor immediately rather than left stale until the next nav key.
        // So: cursor-following clamp only on an actual height change,
        // bounds-only (cursor-agnostic) clamp otherwise.
        // Also re-checked here (not just on pane toggle) because the
        // warnings button can appear/disappear on its own as plugin health
        // changes, independent of any pane action.
        self.clamp_focus();
        let screen_size_changed = constraint != self.last_screen_size;
        self.last_screen_size = constraint;
        if self.warnings_open {
            let h = self.warnings_list_h(constraint.y);
            if screen_size_changed {
                self.follow_warnings_offset();
            } else {
                let n = self.with_session(|s| s.plugin_statuses().len());
                self.warnings_offset = bound_offset(self.warnings_offset, n, h);
            }
        }
        if self.hotkey_menu_open {
            let h = modal_list_h(constraint.y, HOTKEY_LIST_TOP);
            if screen_size_changed {
                self.follow_hotkey_menu_offset();
            } else {
                let n = self.hotkey_rows().len();
                self.hotkey_menu_offset = bound_offset(self.hotkey_menu_offset, n, h);
            }
        }
        if self.playlist_picker_open {
            let h = modal_list_h(constraint.y, PLAYLIST_PICKER_LIST_TOP);
            if screen_size_changed {
                self.follow_playlist_picker_offset();
            } else {
                let n = self.with_session(|s| s.playlists().len());
                self.playlist_picker_offset = bound_offset(self.playlist_picker_offset, n, h);
            }
        }
        let (main_rect, panes) = split(constraint, &self.open_panes, self.pane_cfg);
        let old_pane_heights: Vec<(Pane, usize)> = self
            .last_pane_rects
            .iter()
            .map(|&(p, r)| (p, r.height().saturating_sub(1)))
            .collect();
        let main_h_changed = main_rect.height() != self.last_main_rect.height();
        self.last_main_rect = main_rect;
        self.last_pane_rects = panes;
        let list_h = self.list_h();
        if main_h_changed {
            self.clamp_scroll_for(self.screen, list_h);
        } else {
            self.clamp_offset_bounds(self.screen, list_h);
        }
        for i in 0..self.last_pane_rects.len() {
            let (pane, rect) = self.last_pane_rects[i];
            if let Some(screen) = list_screen_for_pane(pane) {
                let h = rect.height().saturating_sub(1);
                let changed = old_pane_heights.iter().find(|(p, _)| *p == pane).map(|&(_, oh)| oh) != Some(h);
                if changed {
                    self.clamp_scroll_for(screen, h);
                } else {
                    self.clamp_offset_bounds(screen, h);
                }
            }
        }
        constraint
    }

    fn take_focus(&mut self, _: Direction) -> Result<EventResult, CannotFocus> {
        Ok(EventResult::consumed())
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        // Synthetic periodic wakeup (fires on a timer whenever an fps is
        // set, which is now always — see `ui::BASELINE_FPS`), not real user
        // input — must bail out before the trailing `clamp_scroll()` below,
        // or it snaps a wheel-scrolled list back to the cursor every tick.
        if event == Event::Refresh {
            return EventResult::Ignored;
        }
        // Transient queue/wedge feedback shows for one keypress, same as
        // `hotkey_feedback`'s "until the next thing happens" convention.
        // `membership_feedback` (like/unlike, playlist-hotkey toggle) gets
        // the same treatment on the main screen — see the hint-line draw.
        // Skipped for a mouse release/hold: those always follow the press
        // that actually triggered a command, one input gesture later, and
        // would otherwise wipe that command's flash message before it's
        // ever drawn.
        let is_mouse_followup =
            matches!(event, Event::Mouse { event: MouseEvent::Release(_) | MouseEvent::Hold(_), .. });
        if !is_mouse_followup {
            self.queue_feedback = None;
            self.hotkey_feedback = None;
            self.with_session(|s| s.clear_membership_feedback());
        }
        // Active text field: capture everything, except a click elsewhere
        // (releases focus, same as Esc, instead of being swallowed) or a
        // digit as Search's very first keystroke (reinterpreted as the
        // `1`-`5` screen hotkey) — both clear `editing` and re-dispatch the
        // same event as if no field were active.
        if self.editing != Editing::None {
            let is_filter = self.editing == Editing::Filter;
            let outside_click = matches!(event, Event::Mouse { event: MouseEvent::Press(_), .. });
            let leading_digit = self.editing == Editing::Search
                && self.buffer.is_empty()
                && matches!(event, Event::Char(c) if c.is_ascii_digit());
            if outside_click || leading_digit {
                if is_filter {
                    self.filter_query = None; // clear it — show the full list again
                }
                self.editing = Editing::None;
                self.buffer.clear();
                return self.on_event(event);
            }
            return match event {
                Event::Char(c) => {
                    self.buffer.push(c);
                    // The filter narrows live as you type — reset to the
                    // top rather than leave a now-possibly-out-of-range
                    // cursor/scroll position from the unfiltered list.
                    if is_filter {
                        self.reset_filter_selection();
                    }
                    EventResult::consumed()
                }
                Event::Key(Key::Backspace) => {
                    self.buffer.pop();
                    if is_filter {
                        self.reset_filter_selection();
                    }
                    EventResult::consumed()
                }
                Event::Key(Key::Esc) => {
                    if is_filter {
                        self.filter_query = None; // clear it — show the full list again
                    }
                    self.editing = Editing::None;
                    self.buffer.clear();
                    EventResult::consumed()
                }
                Event::Key(Key::Enter) => self.commit_edit(),
                _ => EventResult::consumed(),
            };
        }

        // Fullscreen Screen-mode pane: Esc closes it, nav keys scroll it,
        // everything else is swallowed — same as the old one-shot modal.
        if let Some(pane) = self.screen_pane {
            return match event {
                Event::Key(Key::Esc) => {
                    self.screen_pane = None;
                    if pane == Pane::Vis {
                        self.vis.set_enabled(false);
                        return EventResult::with_cb(|siv| siv.set_fps(crate::BASELINE_FPS));
                    }
                    EventResult::consumed()
                }
                Event::Key(Key::Up) | Event::Char('k') => {
                    self.scroll_pane(pane, true, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::Down) | Event::Char('j') => {
                    self.scroll_pane(pane, false, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::PageUp) | Event::Char('K') => {
                    self.scroll_pane(pane, true, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                Event::Key(Key::PageDown) | Event::Char('J') => {
                    self.scroll_pane(pane, false, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                _ => EventResult::consumed(),
            };
        }

        // The warnings modal: fullscreen, own nav (mirrors `screen_pane`
        // above). Text entry for a `SetupKind::TextInput` plugin is handled
        // entirely by the generic "active text field" block at the very top
        // of this function (`Editing::PluginSetup`) before we ever get
        // here, so this only needs the list-navigation/selection case.
        if self.warnings_open {
            return match event {
                Event::Key(Key::Esc) => {
                    self.warnings_open = false;
                    self.focus = self.fallback_focus();
                    EventResult::consumed()
                }
                Event::Key(Key::Up) | Event::Char('k') => {
                    self.jump_warnings(true, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::Down) | Event::Char('j') => {
                    self.jump_warnings(false, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::PageUp) | Event::Char('K') => {
                    self.jump_warnings(true, LIST_JUMP_STEP);
                    EventResult::consumed()
                }
                Event::Key(Key::PageDown) | Event::Char('J') => {
                    self.jump_warnings(false, LIST_JUMP_STEP);
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelUp, .. } => {
                    self.jump_warnings(true, WHEEL_STEP);
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelDown, .. } => {
                    self.jump_warnings(false, WHEEL_STEP);
                    EventResult::consumed()
                }
                Event::Key(Key::Enter) => {
                    self.activate_selected_warning();
                    EventResult::consumed()
                }
                Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } => {
                    let list_h = self.warnings_list_h(self.last_screen_size.y);
                    if let Some(local) = position.checked_sub(offset)
                        && local.y >= WARNINGS_LIST_TOP
                        && local.y < WARNINGS_LIST_TOP + list_h
                    {
                        let idx = self.warnings_offset + (local.y - WARNINGS_LIST_TOP);
                        let n = self.with_session(|s| s.plugin_statuses().len());
                        if idx < n {
                            self.warnings_cursor = idx;
                            self.activate_selected_warning();
                        }
                    }
                    EventResult::consumed()
                }
                _ => EventResult::consumed(),
            };
        }

        // The "Add to Playlist" picker (`+` on a selected track): fullscreen,
        // own nav (mirrors `warnings_open`/`hotkey_menu_open`).
        if self.playlist_picker_open {
            return match event {
                Event::Key(Key::Esc) => {
                    self.playlist_picker_open = false;
                    self.playlist_picker_track = None;
                    self.focus = self.fallback_focus();
                    EventResult::consumed()
                }
                Event::Key(Key::Up) | Event::Char('k') => {
                    self.jump_playlist_picker(true, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::Down) | Event::Char('j') => {
                    self.jump_playlist_picker(false, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::PageUp) | Event::Char('K') => {
                    self.jump_playlist_picker(true, LIST_JUMP_STEP);
                    EventResult::consumed()
                }
                Event::Key(Key::PageDown) | Event::Char('J') => {
                    self.jump_playlist_picker(false, LIST_JUMP_STEP);
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelUp, .. } => {
                    self.jump_playlist_picker(true, WHEEL_STEP);
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelDown, .. } => {
                    self.jump_playlist_picker(false, WHEEL_STEP);
                    EventResult::consumed()
                }
                Event::Key(Key::Enter) => self.commit_playlist_picker(),
                Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } => {
                    if let Some(local) = position.checked_sub(offset)
                        && local.y >= PLAYLIST_PICKER_LIST_TOP
                    {
                        let idx = self.playlist_picker_offset + (local.y - PLAYLIST_PICKER_LIST_TOP);
                        let n = self.with_session(|s| s.playlists().len());
                        if idx < n {
                            self.playlist_picker_cursor = idx;
                            return self.commit_playlist_picker();
                        }
                    }
                    EventResult::consumed()
                }
                _ => EventResult::consumed(),
            };
        }

        // The "press a key to bind" sub-popup: steals the very next raw
        // keypress as the new binding instead of routing it through
        // `keybindings::map`/`Action` at all (mirrors the "active text
        // field" block at the top of this function, but for one keystroke
        // rather than a buffer).
        if self.hotkey_capture.is_some() {
            return match event {
                Event::Key(Key::Esc) => {
                    self.hotkey_capture = None;
                    EventResult::consumed()
                }
                // Same clear gesture as the hotkey menu's row list
                // (`clear_selected_hotkey`) — carried into the capture step
                // itself since the Playlists screen's standalone modal has
                // no row-list step of its own to press it on first.
                Event::Key(Key::Backspace) => self.clear_captured_hotkey(),
                ev => match key_name(&ev) {
                    Some(k) if k.chars().count() == 1 => self.bind_captured_key(k.chars().next().unwrap()),
                    _ => EventResult::consumed(),
                },
            };
        }

        // The hotkey-menu modal: fullscreen, own nav (mirrors
        // `warnings_open` above).
        if self.hotkey_menu_open {
            return match event {
                Event::Key(Key::Esc) => {
                    self.hotkey_menu_open = false;
                    self.focus = self.fallback_focus();
                    EventResult::consumed()
                }
                Event::Key(Key::Up) | Event::Char('k') => {
                    self.jump_hotkey_menu(true, 1);
                    self.hotkey_feedback = None;
                    EventResult::consumed()
                }
                Event::Key(Key::Down) | Event::Char('j') => {
                    self.jump_hotkey_menu(false, 1);
                    self.hotkey_feedback = None;
                    EventResult::consumed()
                }
                Event::Key(Key::PageUp) | Event::Char('K') => {
                    self.jump_hotkey_menu(true, LIST_JUMP_STEP);
                    self.hotkey_feedback = None;
                    EventResult::consumed()
                }
                Event::Key(Key::PageDown) | Event::Char('J') => {
                    self.jump_hotkey_menu(false, LIST_JUMP_STEP);
                    self.hotkey_feedback = None;
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelUp, .. } => {
                    self.jump_hotkey_menu(true, WHEEL_STEP);
                    self.hotkey_feedback = None;
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelDown, .. } => {
                    self.jump_hotkey_menu(false, WHEEL_STEP);
                    self.hotkey_feedback = None;
                    EventResult::consumed()
                }
                Event::Key(Key::Enter) => {
                    self.open_hotkey_capture();
                    EventResult::consumed()
                }
                Event::Key(Key::Backspace) => self.clear_selected_hotkey(),
                Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } => {
                    if let Some(local) = position.checked_sub(offset)
                        && local.y >= HOTKEY_LIST_TOP
                    {
                        let idx = self.hotkey_menu_offset + (local.y - HOTKEY_LIST_TOP);
                        if idx < self.hotkey_rows().len() {
                            self.hotkey_menu_cursor = idx;
                            self.hotkey_feedback = None;
                        }
                    }
                    EventResult::consumed()
                }
                _ => EventResult::consumed(),
            };
        }

        // The help/shortcuts modal: fullscreen, own nav (mirrors
        // `warnings_open`/`hotkey_menu_open` above).
        if self.help_open {
            return match event {
                Event::Key(Key::Esc) => {
                    self.help_open = false;
                    self.focus = self.fallback_focus();
                    EventResult::consumed()
                }
                Event::Key(Key::Up) | Event::Char('k') => {
                    self.jump_help(true, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::Down) | Event::Char('j') => {
                    self.jump_help(false, 1);
                    EventResult::consumed()
                }
                Event::Key(Key::PageUp) | Event::Char('K') => {
                    self.jump_help(true, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                Event::Key(Key::PageDown) | Event::Char('J') => {
                    self.jump_help(false, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelUp, .. } => {
                    self.jump_help(true, WHEEL_STEP);
                    EventResult::consumed()
                }
                Event::Mouse { event: MouseEvent::WheelDown, .. } => {
                    self.jump_help(false, WHEEL_STEP);
                    EventResult::consumed()
                }
                _ => EventResult::consumed(),
            };
        }

        // The tab bar lives on the fixed top row of the whole screen, never
        // `last_main_rect` (which a pane may have narrowed — see `split`) —
        // clicking a tab switches screen regardless of which pane currently
        // has focus, same as `handle_mouse` does for the list body below.
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.y == 0
            && local.x < self.last_screen_size.x
        {
            let width = self.last_screen_size.x.saturating_sub(1);
            let player_state = self.with_session(|s| s.player_status().state);
            if let Some(button) = transport_at_x(local.x, width, &player_state) {
                return self.run(button.command());
            }
            self.focus = Focus::Main;
            return match tab_at_x(local.x, width, &player_state) {
                Some(target) => self.handle_action(Action::Screen(target)),
                None => EventResult::consumed(),
            };
        }

        // The warnings button lives on the fixed bottom-2 (hint/cmdline)
        // row of the whole screen, never `last_main_rect` — clickable
        // regardless of which pane currently has focus, checked before the
        // `Focus::Main`-gated list-mouse handling below, which only cares
        // about clicks/wheel on the list body itself. Only live when
        // there's actually a button drawn there.
        if self.warn_count() > 0
            && let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.x < self.last_screen_size.x
            && local.y == self.last_screen_size.y.saturating_sub(2)
        {
            self.open_warnings();
            return EventResult::consumed();
        }

        // The bottom status line's transport cluster, scrubber, and
        // bpm/shuffle tags — mirrors `status_line_layout` exactly.
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.x < self.last_screen_size.x
            && local.y == self.last_screen_size.y.saturating_sub(1)
        {
            let (icon, curtime_w, totaltime_w, bpm_w, shuffle_w, duration_ms, position_ms) =
                self.with_session(|s| {
                    let np = s.now_playing();
                    let st = s.player_status();
                    let bpm_tag = bpm_status_tag(s, np.as_ref());
                    let shuffle_tag = if s.shuffle() { "[S]" } else { "[s]" };
                    (
                        player_action_glyph(&st.state),
                        ms(st.position_ms).width(),
                        ms(st.duration_ms).width(),
                        bpm_tag.width(),
                        shuffle_tag.width(),
                        st.duration_ms,
                        st.position_ms,
                    )
                });
            let (_, layout) = status_line_layout(
                self.last_screen_size.x,
                &StatusLineWidths {
                    prev: PREV_ICON.width(),
                    playpause: icon.width(),
                    next: NEXT_ICON.width(),
                    curtime: curtime_w,
                    bar: STATUS_BAR_WIDTH,
                    totaltime: totaltime_w,
                    bpm: bpm_w,
                    shuffle: shuffle_w,
                },
            );
            if in_span(local.x, layout.playpause) {
                return self.run(Command::PlayPause);
            }
            if in_span(local.x, layout.prev) {
                return self.run(Command::Previous);
            }
            if in_span(local.x, layout.next) {
                return self.run(Command::Next);
            }
            if in_span(local.x, layout.scrubber) && duration_ms > 0 {
                let frac = (local.x - layout.scrubber.0) as f64 / layout.scrubber.1 as f64;
                let target_ms = (frac * duration_ms as f64).round() as u32;
                return self.run(Command::Seek(target_ms as i64 - position_ms as i64));
            }
            if in_span(local.x, layout.bpm) {
                return self.run(Command::ToggleScan);
            }
            if in_span(local.x, layout.shuffle) {
                return self.run(Command::ToggleShuffle);
            }
            return EventResult::consumed();
        }

        // Mouse: routed separately from the keyboard path below entirely,
        // and returned early — a wheel scroll deliberately skips the
        // trailing `clamp_scroll()` the keyboard arms get (see
        // `handle_mouse`'s doc), so it must never fall into that match.
        //
        // Tried regardless of current focus, main content first then each
        // open pane's own rect — a click/wheel *sets* focus to wherever it
        // landed rather than requiring it already be there (previously this
        // only ran at all while `Focus::Main`, silently swallowing clicks
        // anywhere else instead of focusing them).
        if let Event::Mouse { offset, position, event: mev } = event {
            if let Some(result) = self.handle_mouse(offset, position, mev) {
                return result;
            }
            for (pane, rect) in self.last_pane_rects.clone() {
                if let Some(result) = self.handle_pane_mouse(pane, rect, offset, position, mev) {
                    return result;
                }
            }
        }

        // Any key other than Enter defocuses the warnings button (a click on
        // the button itself already returned above) and falls through to
        // the same event as if `fallback_focus()` had already been focused
        // — so e.g. `j` both switches focus and moves the cursor in one
        // keystroke, instead of leaving a dead keypress on `Focus::Warnings`.
        if self.focus == Focus::Warnings && defocuses_warnings(&event) {
            self.focus = self.fallback_focus();
        }

        // One lock: computing the row count for the playlists screen also needs
        // `playlists().len()`, and taking the guard twice in one statement
        // deadlocks (non-reentrant mutex).
        let len = self.with_session(|s| {
            let tracks = self.visible_track_ids(s, self.screen).len();
            if norm_screen(self.screen) == PLAYLISTS
                && self.open_playlist.is_none()
                && self.open_remote.is_none()
            {
                tracks.max(self.top_rows(s).len())
            } else {
                tracks
            }
        });

        let result = match event {
            Event::Key(Key::Tab) => {
                self.cycle_focus();
                EventResult::consumed()
            }
            Event::Key(Key::Up) | Event::Char('k') if self.focus == Focus::Main => {
                let c = &mut self.cursor[norm_screen(self.screen)];
                *c = c.saturating_sub(1);
                EventResult::consumed()
            }
            Event::Key(Key::Down) | Event::Char('j') if self.focus == Focus::Main => {
                let s = norm_screen(self.screen);
                self.cursor[s] = self.cursor[s].saturating_add(1);
                self.clamp_cursor(len);
                EventResult::consumed()
            }
            Event::Key(Key::Right) => self.run(Command::Seek(5000)),
            Event::Key(Key::Left) => self.run(Command::Seek(-5000)),
            // Shift-J/Shift-K jump `LIST_JUMP_STEP` rows on the main
            // tracklist or a focused list-pane (`jump_list`), or scroll a
            // focused non-list pane (Log/Settings) by the same page step —
            // guarded so an unrelated 'J'/'K' hotkey (e.g. while
            // `Focus::Warnings`) still falls through to the lookup below.
            Event::Char('K') if self.focus != Focus::Warnings => match self.focus {
                Focus::Pane(pane) if list_screen_for_pane(pane).is_none() => {
                    self.scroll_pane(pane, true, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                _ => self.jump_list(true, LIST_JUMP_STEP),
            },
            Event::Char('J') if self.focus != Focus::Warnings => match self.focus {
                Focus::Pane(pane) if list_screen_for_pane(pane).is_none() => {
                    self.scroll_pane(pane, false, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                _ => self.jump_list(false, LIST_JUMP_STEP),
            },
            Event::Key(Key::Esc)
                if norm_screen(self.screen) == PLAYLISTS
                    && (self.open_playlist.is_some() || self.open_remote.is_some()) =>
            {
                self.open_playlist = None;
                self.open_remote = None;
                // Explicitly backing out to the list (unlike switching
                // screens away and back) means "forget this" — don't let a
                // later screen-switch resurrect it.
                self.remembered_playlist = None;
                self.cursor[PLAYLISTS] = 0;
                self.filter_query = None; // going back — the filtered list no longer applies
                EventResult::consumed()
            }
            // Nav keys past this point only apply while a pane is focused —
            // otherwise leave them Ignored (e.g. free for search-results
            // paging), matching `focus_order` (never `Pane` in `Screen` mode).
            // A focused list-pane (Queue/History) moves its own cursor, same
            // as `Focus::Main` above; any other pane (Log/Settings/Vis)
            // scrolls lines instead — see `list_screen_for_pane`.
            Event::Key(Key::Up) | Event::Char('k') => {
                if let Focus::Pane(pane) = self.focus {
                    match list_screen_for_pane(pane) {
                        Some(screen) => {
                            let c = &mut self.cursor[screen];
                            *c = c.saturating_sub(1);
                        }
                        None => self.scroll_pane(pane, true, 1),
                    }
                }
                EventResult::consumed()
            }
            Event::Key(Key::Down) | Event::Char('j') => {
                if let Focus::Pane(pane) = self.focus {
                    match list_screen_for_pane(pane) {
                        Some(screen) => self.bump_pane_cursor(screen, 1),
                        None => self.scroll_pane(pane, false, 1),
                    }
                }
                EventResult::consumed()
            }
            // PageUp/PageDown: same `jump_list`/`scroll_pane` split as
            // Shift-J/Shift-K above, now also reaching `Focus::Main` (used
            // to be `Ignored` there — the main tracklist had no page-jump
            // at all).
            Event::Key(Key::PageUp) => match self.focus {
                Focus::Pane(pane) if list_screen_for_pane(pane).is_none() => {
                    self.scroll_pane(pane, true, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                _ => self.jump_list(true, LIST_JUMP_STEP),
            },
            Event::Key(Key::PageDown) => match self.focus {
                Focus::Pane(pane) if list_screen_for_pane(pane).is_none() => {
                    self.scroll_pane(pane, false, PAGE_SCROLL_STEP);
                    EventResult::consumed()
                }
                _ => self.jump_list(false, LIST_JUMP_STEP),
            },
            Event::Key(Key::Enter) if self.focus == Focus::Warnings => {
                self.open_warnings();
                EventResult::consumed()
            }
            Event::Key(Key::Enter) | Event::Char(' ') if self.focus == Focus::Pane(Pane::Settings) => {
                self.toggle_selected_setting();
                EventResult::consumed()
            }
            Event::Char(':') => self.handle_action(Action::CommandLine),
            Event::Char('x') => {
                match self.with_session(|s| self.selected_playlist(s)) {
                    Some(id) => self.run(Command::ExportM3u(id)),
                    None => EventResult::Ignored,
                }
            }
            // With a playlist (local or remote) selected on the Playlists
            // screen, backtick locally overrides its global `OpenHotkeyMenu`
            // meaning: the built-ins-only hotkey menu has no use for a
            // playlist target, so this goes straight to the standalone
            // "press a key to bind" modal instead (see
            // `open_playlist_hotkey_modal`/`draw_playlist_hotkey_modal`).
            Event::Char('`') if self.with_session(|s| self.selected_hotkey_target(s)).is_some() => {
                if let Some(target) = self.with_session(|s| self.selected_hotkey_target(s)) {
                    self.open_playlist_hotkey_modal(target);
                }
                EventResult::consumed()
            }
            Event::Key(Key::Enter) => {
                let active = self.active_screen();
                let sel = self.with_session(|s| self.selected_track(s, active));
                match keybindings::map("Enter", sel, &self.hotkeys_map()) {
                    Action::PlayFromContext(_) => {
                        self.play_track_at(active, self.cursor[norm_screen(active)])
                    }
                    action => self.handle_action(action),
                }
            }
            ev => match key_name(&ev) {
                Some(k) => {
                    let active = self.active_screen();
                    let sel = self.with_session(|s| self.selected_track(s, active));
                    // Dynamic per-user playlist hotkeys (checked here, not in
                    // `keybindings::map`, which has no access to runtime
                    // state) win over a built-in command when a key names a
                    // playlist target; `map` consults the same table itself
                    // for a built-in one.
                    let hotkeys = self.hotkeys_map();
                    match keybindings::hotkey_toggle(&k, sel, &hotkeys) {
                        Some(cmd) => self.run(cmd),
                        None => self.handle_action(keybindings::map(&k, sel, &hotkeys)),
                    }
                }
                None => EventResult::Ignored,
            },
        };
        // Cheap (no session lock) and covers every arm above uniformly,
        // including ones reached via `handle_action`/keybindings — simpler
        // and less fragile than clamping inside each cursor-moving arm.
        self.clamp_scroll();
        result
    }
}

/// Only ever called for `count > 0` — the button isn't drawn at all when
/// there are no warnings.
fn warnings_label(count: usize) -> String {
    format!(" ⚠ warnings ({count}) ")
}

/// Left-aligns `s` in a `width`-column field, truncating/padding by terminal
/// display width (not char count) so wide codepoints (emoji, CJK) don't
/// shift whatever comes after.
pub(crate) fn pad(s: &str, width: usize) -> String {
    let mut s = truncate(s, width);
    let w = s.width();
    if w < width {
        s.extend(std::iter::repeat_n(' ', width - w));
    }
    s
}

/// Right-aligns `s` in a `width`-column field (display-width-aware, see `pad`).
fn pad_right_aligned(s: &str, width: usize) -> String {
    let s = truncate(s, width);
    let w = s.width();
    if w < width { " ".repeat(width - w) + &s } else { s }
}

/// Draw a scrollbar thumb in the column at `x = gutter_x` of `printer`,
/// covering rows `1..=list_h` (row 0 is the title). `total` is the full
/// (possibly still-growing, e.g. Spotify Liked Songs mid-fetch) list length;
/// `offset` is the first visible row's index into it. No-op when everything
/// fits on screen already (`total <= list_h`) — just the bare column shows.
fn draw_scrollbar(printer: &Printer, gutter_x: usize, list_h: usize, offset: usize, total: usize) {
    if list_h == 0 {
        return;
    }
    for y in 0..list_h {
        printer.print((gutter_x, y), "│");
    }
    if total <= list_h {
        return;
    }
    let thumb_len = (list_h * list_h / total).max(1).min(list_h);
    let track = list_h - thumb_len;
    let thumb_start = if total > list_h {
        (offset * track) / (total - list_h)
    } else {
        0
    }
    .min(track);
    printer.with_color(ColorStyle::highlight(), |p| {
        for y in thumb_start..thumb_start + thumb_len {
            p.print((gutter_x, y), "┃");
        }
    });
}

/// Fixed widths for the tags/hotkeys/source/duration columns of a track row
/// (see `ui::row::RowItem`); the main (artist/title) column takes whatever's
/// left. `TAGS_COL_W` is bpm's own width (the canonical MVP tag) — a rounded
/// number right-aligned in a 3-wide field, so 60-200 BPM all line up
/// (mirrors `sort-tab-features`'s `Track::bpm_display`); `SOURCE_COL_W` fits
/// two "+"-joined 2-char source badges plus the cached-track "*" prefix
/// (e.g. "*sp+sc") before truncating. `DURATION_COL_W` is "M:SS"/"MM:SS"
/// left-aligned in 6. `HOTKEYS_COL_W` fits a handful of sorted, concatenated
/// hotkey chars (e.g. "Cgm") before truncating.
const TAGS_COL_W: usize = 3;
const SOURCE_COL_W: usize = 6;
const DURATION_COL_W: usize = 6;
const HOTKEYS_COL_W: usize = 8;

fn five_col(tags: &str, main: &str, hotkeys: &str, source: &str, duration: &str, width: usize) -> String {
    let fixed = TAGS_COL_W + SOURCE_COL_W + DURATION_COL_W + HOTKEYS_COL_W + 4; // 4 single-space gaps
    if width <= fixed {
        return truncate(main, width);
    }
    let main_w = width - fixed;
    format!(
        "{} {} {} {} {}",
        pad_right_aligned(tags, TAGS_COL_W),
        pad(main, main_w),
        pad(hotkeys, HOTKEYS_COL_W),
        pad(source, SOURCE_COL_W),
        pad(duration, DURATION_COL_W),
    )
}

/// Strips U+FE0E/U+FE0F variation selectors — zero-width in terminals, but
/// counted inconsistently by some width tables, so drop them before any
/// display-width computation rather than trust every source to agree.
fn strip_variation_selectors(s: &str) -> String {
    s.chars().filter(|&c| c != '\u{FE0E}' && c != '\u{FE0F}').collect()
}

/// `truncate`, but marks a cut with a trailing `…` (reserving 1 display
/// column for it) instead of silently dropping the rest — the Now Playing
/// title bar's truncation convention for a long context name.
fn truncate_ellipsis(s: &str, width: usize) -> String {
    let s = strip_variation_selectors(s);
    if s.width() <= width {
        return s;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = truncate(&s, width - 1);
    out.push('…');
    out
}

/// Truncates `s` to at most `width` terminal display columns (not chars) —
/// a wide (2-column) codepoint that wouldn't fully fit is dropped entirely
/// rather than emitting half of it.
fn truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in strip_variation_selectors(s).chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// `clamp_scroll_for`'s cursor-follow arithmetic, extracted as a free
/// function so it's unit-testable without a `MedleyView`/`Session`: scrolls
/// `offset` up just enough to include `cursor` if it's above the window, or
/// down just enough if it's below — otherwise leaves `offset` untouched.
/// This is the ONLY place the view window is meant to chase the cursor; a
/// wheel scroll must never route through it.
fn follow_cursor_offset(cursor: usize, offset: usize, list_h: usize) -> usize {
    if cursor < offset {
        cursor
    } else if list_h > 0 && cursor >= offset + list_h {
        cursor + 1 - list_h
    } else {
        offset
    }
}

/// `clamp_offset_bounds`'s arithmetic, extracted the same way — caps
/// `offset` so the window can't run past the end of a `len`-row list.
/// Deliberately takes no `cursor` at all: this is the only reclamp
/// `required_size` still performs on every layout pass (including a plain
/// resize), and it must stay cursor-agnostic or the old snap-back bug comes
/// right back.
fn bound_offset(offset: usize, len: usize, list_h: usize) -> usize {
    offset.min(len.saturating_sub(list_h))
}

/// Whether a click on (`screen`, `idx`) at `now`, given the previous click
/// `last`, counts as a double-click — the same row, within
/// `DOUBLE_CLICK_WINDOW`. A free function (rather than inline in
/// `click_row`) so it's unit-testable without a `MedleyView`/`Session`.
fn is_double_click(last: Option<(Instant, usize, usize)>, now: Instant, screen: usize, idx: usize) -> bool {
    matches!(
        last,
        Some((t, s, i)) if s == screen && i == idx && now.duration_since(t) <= DOUBLE_CLICK_WINDOW
    )
}

/// Whether a keyboard event arriving while `Focus::Warnings` is focused
/// should knock focus off the button (see `on_event`, next to
/// `fallback_focus`) — everything except `Enter`, which opens the modal
/// instead. Mouse events never reach this check: `handle_mouse`/
/// `handle_pane_mouse` already reassign focus unconditionally before this
/// point. A free function so it's unit-testable without a `MedleyView`.
fn defocuses_warnings(event: &Event) -> bool {
    !matches!(event, Event::Key(Key::Enter))
}

fn ms(ms: u32) -> String {
    let total = ms / 1000;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Width left for the track-name field on the status line once the leading
/// icon (`prefix_w`) and the trailing scrubber+time block (`reserved_w`) are
/// accounted for. Saturates to 0 rather than underflowing on narrow terminals.
fn name_field_width(total_w: usize, prefix_w: usize, reserved_w: usize) -> usize {
    total_w.saturating_sub(prefix_w).saturating_sub(reserved_w)
}

/// The status line's scrubber width — the original 20 columns, 20% longer
/// (requested explicitly, rather than derived from anything else).
const STATUS_BAR_WIDTH: usize = 24;

/// Click targets on the bottom status line — `(start column, width)` each,
/// in screen columns. Built by [`status_line_layout`], the one place that
/// decides where every segment of that row sits, so `draw`'s rendering and
/// `on_event`'s hit-testing can never drift apart.
struct StatusLineLayout {
    prev: (usize, usize),
    playpause: (usize, usize),
    next: (usize, usize),
    scrubber: (usize, usize),
    bpm: (usize, usize),
    shuffle: (usize, usize),
}

/// Every segment's already-rendered display width, for [`status_line_layout`]
/// — a struct rather than a long parameter list since the caller needs the
/// actual rendered text for all of these anyway (to draw them, or, for a
/// click, to size a freshly recomputed layout the same way).
struct StatusLineWidths {
    prev: usize,
    playpause: usize,
    next: usize,
    curtime: usize,
    bar: usize,
    totaltime: usize,
    bpm: usize,
    shuffle: usize,
}

/// Column layout for the status line: `{prev} {playpause} {next}  {title}
/// {curtime} {scrubber} {totaltime}  {bpm} {shuffle}` — transport buttons
/// clustered at the far left (one column of breathing room between each,
/// same as the top-bar cluster's own button padding), then
/// [`name_field_width`] for the title.
fn status_line_layout(total_w: usize, w: &StatusLineWidths) -> (usize, StatusLineLayout) {
    let gap = 2;
    let prev = (0, w.prev);
    let playpause = (w.prev + 1, w.playpause);
    let next = (w.prev + 1 + w.playpause + 1, w.next);
    let cluster_w = w.prev + 1 + w.playpause + 1 + w.next;
    let title_start = cluster_w + gap;
    let reserved =
        gap + w.curtime + 1 + w.bar + 1 + w.totaltime + gap + w.bpm + 1 + w.shuffle;
    let name_w = name_field_width(total_w, title_start, reserved);

    let mut x = title_start + name_w + gap;
    x += w.curtime + 1;
    let scrubber = (x, w.bar);
    x += w.bar + 1 + w.totaltime + gap;
    let bpm = (x, w.bpm);
    x += w.bpm + 1;
    let shuffle = (x, w.shuffle);
    (name_w, StatusLineLayout { prev, playpause, next, scrubber, bpm, shuffle })
}

/// `true` if column `x` falls inside `(start, width)`.
fn in_span(x: usize, (start, width): (usize, usize)) -> bool {
    x >= start && x < start + width
}

fn progress_bar(pos: u32, dur: u32, width: usize) -> String {
    if dur == 0 {
        return "-".repeat(width);
    }
    let filled = ((pos as f64 / dur as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "━".repeat(filled), "╍".repeat(width - filled))
}

/// `▶`/`⏸`/`⏹` for the given playback state — the single source of truth
/// for this icon, shared between the status line (above) and the terminal
/// window title (`app`'s event loop), so the two never disagree.
pub fn player_state_icon(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "▶",
        PlayerState::Paused => "⏸",
        PlayerState::Stopped => "⏹",
    }
}

/// The play/pause *button*'s icon: the action pressing it would take, not
/// the state it's in — `⏸` while playing, `▶` while paused. Stopped keeps
/// `⏹`, the same indicator the rest of the UI uses for it.
fn player_action_icon(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "⏸",
        PlayerState::Paused => "▶",
        PlayerState::Stopped => "⏹",
    }
}

/// `player_action_icon` with `player_state_glyph`'s leading-space padding —
/// what every clickable play/pause button (top-bar cluster, status line)
/// draws.
fn player_action_glyph(state: &PlayerState) -> String {
    format!(" {}", player_action_icon(state))
}

/// `player_state_icon`, with an extra leading space — most terminal fonts
/// render `▶` a column narrower than `⏸`/`⏹`, so without it the play glyph
/// looks shifted left of where pause/stop sit. Every place the icon is
/// actually displayed (top-bar cluster, status line, window title) uses
/// this instead of the raw glyph, so they all stay visually aligned.
pub fn player_state_glyph(state: &PlayerState) -> String {
    format!(" {}", player_state_icon(state))
}

/// Bracketed BPM-scan status tag shown next to the status line's scrubber,
/// regardless of whether anything's playing: `[bd]` with no scan driver or
/// while disabled. Otherwise the first letter names the mode — `B` active,
/// `b` cache-only (lowercase throughout means "not fetching over the
/// network right now") — and the second names the live per-track status:
/// `w` idle/waiting (also the no-track-loaded case — there's nothing to
/// report a per-track status for), `d` downloading, `e` errored, `s`
/// skipped. E.g. `[Bd]` actively downloading, `[bw]` idle in cache-only mode.
fn bpm_status_tag(s: &Session, track: Option<&core::Track>) -> String {
    let Some(scan) = s.scan.as_ref() else {
        return "[bd]".to_string();
    };
    let mode_letter = match scan.mode() {
        core::ScanMode::Disabled => return "[bd]".to_string(),
        core::ScanMode::CacheOnly => 'b',
        core::ScanMode::Active => 'B',
    };
    // Purely a plugin-status indicator, never the resolved value itself —
    // that already has its own cell in `visible_track_attrs`. A resolved
    // track has no live status (see `ScanStatus`'s doc), so it falls into
    // the idle arm below same as "never attempted", both being steady
    // states with nothing left for this plugin to do.
    let status_letter = match track.and_then(|t| scan.status("bpm", t.id)) {
        Some(core::ScanStatus::Downloading) => 'd',
        Some(core::ScanStatus::Error) => 'e',
        Some(core::ScanStatus::Skipped) => 's',
        None => 'w',
    };
    format!("[{mode_letter}{status_letter}]")
}

/// Full, un-scrolled track text for the terminal window title:
/// `"{artist} - {title}"`, matching the convention used elsewhere (e.g.
/// `core::app::build_entry`). Falls back to a bare `"medley"` when nothing
/// is loaded. Deliberately excludes the play/pause icon — that's a fixed
/// prefix `app`'s `WindowTitle` adds outside the scrolled portion, so it
/// never scrolls along with the title text (see `player_state_glyph`).
pub fn window_title_track_text(track: Option<&core::Track>) -> String {
    match track {
        Some(t) => format!("{} - {}", t.display_artist(), t.title),
        None => "medley".to_string(),
    }
}

/// Separator inserted between loop repeats by [`scroll_title`] — exposed so
/// callers driving the scroll offset from wall-clock time can compute the
/// same cycle length (`full.chars().count() + SCROLL_GAP.chars().count()`)
/// without duplicating the literal.
pub const SCROLL_GAP: &str = "   ";

/// Offset into a `cycle_len`-long looping [`scroll_title`] after `elapsed`
/// real time, advancing one column per second — shared by the terminal
/// window title (`app`'s `WindowTitle`) and the tab bar's own marquee
/// (`draw_tab_bar`) so both scroll at the same, wall-clock-correct speed
/// regardless of how often their surrounding redraw happens to wake.
pub fn marquee_offset(elapsed: Duration, cycle_len: usize) -> usize {
    if cycle_len == 0 {
        return 0;
    }
    (elapsed.as_secs() as usize) % cycle_len
}

/// A marquee-style `width`-character window over `full`, sliding by one
/// character per unit of `offset`. Used when `full` is too long for the
/// terminal-title's usual max width (see `app`'s event loop, which advances
/// `offset` once per second of real time).
///
/// Once the window has scrolled past the end, it wraps back to the start
/// with a short gap (so it reads as one continuously looping ticker rather
/// than snapping). The edge(s) of the window that currently sit mid-string
/// (not the gap, and not the true start/end of `full`) are marked with `…`
/// — e.g. `"Darude - Sandstorm"` mid-scroll might render as `"…de - Sandst…"`.
///
/// Counts in `chars()`, not bytes, throughout — track/artist names can
/// contain multi-byte UTF-8.
pub fn scroll_title(full: &str, width: usize, offset: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = full.chars().collect();
    let len = chars.len();
    if len <= width {
        return full.to_string();
    }

    let gap: Vec<char> = SCROLL_GAP.chars().collect();
    let cycle_len = len + gap.len();
    let start = offset % cycle_len;

    let mut window: Vec<char> = Vec::with_capacity(width);
    for i in 0..width {
        let pos = (start + i) % cycle_len;
        window.push(if pos < len { chars[pos] } else { gap[pos - len] });
    }

    // Ellipsis at an edge only when that edge sits strictly inside `full`
    // itself (not in the gap, and not exactly at `full`'s own start/end).
    if start < len && start > 0 {
        window[0] = '…';
    }
    let end = (start + width - 1) % cycle_len;
    if end < len.saturating_sub(1) {
        window[width - 1] = '…';
    }
    window.into_iter().collect()
}

