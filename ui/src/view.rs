//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.

use std::thread;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use cursive::{Cursive, Printer, Rect, Vec2, View};
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle};
use cursive::view::CannotFocus;
use cursive::views::Dialog;

use fuzzy_matcher::skim::SkimMatcherV2;

use unicode_width::UnicodeWidthStr;

use core::{
    BrowseNode, Command, CoreEvent, Dispatch, HotkeyTarget, LogBuf, PaneLayoutConfig, PaneMode,
    PlaylistId, Plugin, Session, Side, SourceId, TrackId,
};

use crate::{SessionHandle, command, keybindings};
use crate::command::Pane;
use crate::keybindings::Action;

use filter::FilterCache;
use hotkeys::HOTKEY_LIST_TOP;
use log::draw_pane;
use panes::{PANE_LAYOUT_CYCLE, list_screen_for_pane, split};
use playlists::{RememberedPlaylist, TopRow, resolve_remembered_playlist};
use rows::{Cell, LIST_TITLE_ROWS, Row, draw_row_list, plain_row, tracks_to_rows};
use scroll::{
    CursorWindow, LIST_JUMP_STEP, PAGE_SCROLL_STEP, WHEEL_STEP, bound_offset, follow_cursor_offset,
    modal_list_h,
};
use settings::{draw_settings_pane, settings_entries};
use status_line::{STATUS_BAR_WIDTH, StatusLineWidths, bpm_status_tag, progress_bar, status_line_layout};
use tab_bar::{draw_tab_bar, screen_name, tab_at_x, transport_at_x};
use text::{in_span, ms};
use transport::{NEXT_ICON, PREV_ICON, player_action_glyph};
use warnings::{WARNINGS_LIST_TOP, defocuses_warnings, warnings_label};

mod filter;
mod help;
mod hotkeys;
mod log;
mod mouse;
mod panes;
mod playlists;
mod rows;
mod scroll;
mod settings;
mod status_line;
mod tab_bar;
mod text;
mod transport;
mod warnings;

pub(crate) use text::pad;
pub use text::{SCROLL_GAP, marquee_offset, scroll_title};
pub use status_line::window_title_track_text;
pub use transport::player_state_glyph;

/// The track list `Command::PlayContext` last started playing from; independent of the Playlists screen's state.
pub(crate) const NOW_PLAYING: usize = 0;
pub(crate) const QUEUE: usize = 1;
pub(crate) const PLAYLISTS: usize = 2;
/// `:hist` or the `4` key.
pub(crate) const HIST: usize = 3;
/// `/` or the `3` key.
pub(crate) const SEARCH: usize = 4;
/// Number of screens — sizes `cursor` below.
const N_SCREENS: usize = 5;

fn startup_screen(initial_screen: &str) -> usize {
    match initial_screen {
        "queue" => QUEUE,
        "playlists" => PLAYLISTS,
        "hist" => HIST,
        "now_playing" => NOW_PLAYING,
        _ => SEARCH,
    }
}

/// Row the "Add to Playlist" picker's list starts on — same shape as `HOTKEY_LIST_TOP`.
const PLAYLIST_PICKER_LIST_TOP: usize = 2;

#[derive(Clone, PartialEq)]
enum Editing {
    None,
    Search,
    CommandLine,
    /// Collecting a `SetupKind::TextInput` value for the warnings-panel plugin selected.
    PluginSetup(SourceId),
    /// Screen-local fuzzy filter (`/` on any track-list screen other than Search itself).
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


pub struct MedleyView {
    session: SessionHandle,
    screen: usize,
    cursor: [usize; N_SCREENS],
    /// First visible row per screen, persisted so the cursor moves freely within the window before it scrolls.
    list_offset: [usize; N_SCREENS],
    /// The main list's rect as of the last layout pass (matches `draw`'s `main_rect`).
    last_main_rect: Rect,
    /// Whole-terminal size as of the last layout pass.
    last_screen_size: Vec2,
    editing: Editing,
    buffer: String,
    /// Committed screen-local filter query (`Editing::Filter`, Enter to commit).
    filter_query: Option<String>,
    /// Fuzzy matcher backing the `/`-filter.
    filter_matcher: SkimMatcherV2,
    /// Memoized result of the last `filtered_tracks` computation.
    filter_cache: std::sync::Mutex<Option<FilterCache>>,
    /// Last queue/wedge result.
    queue_feedback: Option<String>,
    /// UI-local: which local playlist's tracks are shown on the playlists screen.
    open_playlist: Option<PlaylistId>,
    /// UI-local: which remote browse folder (e.g. a Spotify playlist) is shown on the playlists screen.
    open_remote: Option<(SourceId, String, BrowseNode)>,
    /// Whichever playlist was open when the Playlists screen was last left; Esc back to the top level forgets it.
    remembered_playlist: Option<RememberedPlaylist>,
    /// UI-local: which optional panes are currently open, in stack order (first = nearest the main content).
    open_panes: Vec<Pane>,
    /// Shared default placement — screen vs. embedded, which side, which stacking axis.
    pane_cfg: PaneLayoutConfig,
    /// Per-pane override of `pane_cfg.mode` — `:panes <pane> <screen| embedded>`.
    pane_mode_overrides: HashMap<Pane, PaneMode>,
    /// Each embedded pane's rect as of the last layout pass, mirroring `last_main_rect`.
    last_pane_rects: Vec<(Pane, Rect)>,
    /// Recent log lines, shared with `app`'s logger. Read-only here.
    log: Arc<LogBuf>,
    /// Text of the last committed `Command::Search`, so an empty result list can say "no results for X".
    last_query: Option<String>,
    /// UI-local: how many (wrapped) rows up from the live tail the embedded Log pane is scrolled.
    log_scroll: usize,
    /// `Some(n)` while the Log pane is scrolled away from the tail (`log_scroll != 0`).
    log_pin: Option<usize>,
    /// Selected row within the Settings pane's entry list.
    settings_cursor: usize,
    /// Scroll window into the Settings pane's list — see `warnings_offset`.
    settings_offset: usize,
    /// Which pane currently receives nav keys; `Tab` cycles it.
    focus: Focus,
    /// The Vis pane's background worker + last computed frame.
    vis: Arc<crate::vis::Vis>,
    /// `PaneMode::Screen`'s pane, shown fullscreen in place of the normal 3 screens.
    screen_pane: Option<Pane>,
    /// The plugin-warnings modal.
    warnings_open: bool,
    /// Selected row within the warnings modal.
    warnings_cursor: usize,
    /// Scroll window into the warnings modal's list.
    warnings_offset: usize,
    /// (when, screen, row index) of the last left-click on a list row, for double-click detection.
    last_click: Option<(Instant, usize, usize)>,
    /// The "Hotkeys" management modal (backtick, off the Playlists screen).
    hotkey_menu_open: bool,
    /// Selected row within the hotkey-menu modal.
    hotkey_menu_cursor: usize,
    /// Scroll window into the hotkey-menu modal's list.
    hotkey_menu_offset: usize,
    /// The "press a key to bind" sub-popup.
    hotkey_capture: Option<HotkeyTarget>,
    /// Last bind/unbind result, shown until the next keypress or the modal closes.
    hotkey_feedback: Option<String>,
    /// The help/shortcuts screen (`?` or `:help`).
    help_open: bool,
    /// Rows scrolled down from the top of the help screen's content.
    help_scroll: usize,
    /// The "Add to Playlist" picker (`+`, a track selected).
    playlist_picker_open: bool,
    /// Selected row within the playlist picker.
    playlist_picker_cursor: usize,
    /// Scroll window into the playlist picker's list — see `warnings_offset`.
    playlist_picker_offset: usize,
    /// The track being added, captured when the picker opens so it stays fixed if the list scrolls.
    playlist_picker_track: Option<TrackId>,
    /// (last now-playing text seen, when its marquee scroll started); a `Mutex` only because `draw` takes `&self`.
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

    /// `Main`, then each open pane (in stack order), then the warnings button.
    fn focus_order(&self) -> Vec<Focus> {
        // `open_panes` only ever holds panes currently placed `Embedded` (see `toggle_pane`).
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

    /// Where focus should land when it can no longer stay on the warnings button.
    fn fallback_focus(&self) -> Focus {
        Focus::Main
    }

    /// Draws `lines` as a scrollable cursor list starting at row `list_top`.
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

    /// Fullscreen "Add to Playlist" picker (`+` with a track selected).
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

    // `session` is a non-reentrant `Mutex`: always lock via `with_session`, never twice in one statement.
    fn with_session<R>(&self, f: impl FnOnce(&Session) -> R) -> R {
        let guard = self.session.lock().unwrap();
        f(&guard)
    }

    fn with_session_mut<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut guard = self.session.lock().unwrap();
        f(&mut guard)
    }

    // ---- snapshot helpers -------------------------------------------------

    /// Track ids visible on `screen`, in display order.
    fn visible_track_ids(&self, s: &Session, screen: usize) -> Vec<TrackId> {
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

    /// The track selected on `screen`.
    fn selected_track(&self, s: &Session, screen: usize) -> Option<TrackId> {
        let ids = self.visible_track_ids(s, screen);
        ids.get(self.cursor[screen]).copied()
    }

    /// Which screen index keyboard nav/selection currently targets.
    fn active_screen(&self) -> usize {
        match self.focus {
            Focus::Pane(p) => list_screen_for_pane(p).unwrap_or(self.screen),
            Focus::Main | Focus::Warnings => self.screen,
        }
    }

    /// Plays row `idx` of `screen`'s track list, same as pressing Enter on it while selected.
    fn play_track_at(&mut self, screen: usize, idx: usize) -> EventResult {
        let (tracks, sel, name) = self.with_session(|s| {
            let tracks = self.visible_track_ids(s, screen);
            let sel = tracks.get(idx).copied();
            (tracks, sel, self.context_name(s, screen))
        });
        let Some(id) = sel else {
            return EventResult::consumed();
        };
        let index = tracks.iter().position(|t| *t == id).unwrap_or(0);
        // Only the Playlists screen's own remote-browse state is ever meaningful here.
        let remote = if screen == PLAYLISTS {
            self.open_remote.as_ref().map(|(sid, _, node)| (sid.clone(), node.clone()))
        } else {
            None
        };
        self.run(Command::PlayContext { tracks, index, remote, name })
    }

    /// `screen`'s track list's display name.
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

    fn row_unit(&self, screen: usize, count: usize) -> &'static str {
        let playlists = screen == PLAYLISTS
            && self.open_playlist.is_none()
            && self.open_remote.is_none();
        match (playlists, count == 1) {
            (true, true) => "playlist",
            (true, false) => "playlists",
            (false, true) => "track",
            (false, false) => "tracks",
        }
    }

    /// `screen`'s list title row: `<name>`, optionally followed by `  (<hint>)`.
    fn list_title(&self, s: &Session, screen: usize) -> String {
        if let Some(query) = self.active_filter().filter(|q| !q.is_empty() && self.filterable_screen(screen)) {
            let total = self.visible_track_ids(s, screen).len();
            let plural = if total == 1 { "" } else { "es" };
            return format!("filter {query:?} ({total} match{plural})");
        }
        let (name, hint) = match screen {
            NOW_PLAYING => (s.playing_context_name(), None),
            SEARCH => (self.last_query.clone(), (self.editing == Editing::Search).then_some("Esc to cancel")),
            PLAYLISTS => {
                let name = match (self.open_playlist, &self.open_remote) {
                    (Some(id), _) => s.playlists().into_iter().find(|p| p.id == id).map(|p| p.name),
                    (None, Some((sid, name, _))) => Some(format!("[{sid}] {name}")),
                    (None, None) => None,
                };
                let hint = name.is_some().then_some("Esc to go back");
                (name, hint)
            }
            _ => (None, None),
        };
        let name = name.unwrap_or_else(|| {
            let total = self.list_len(s, screen);
            format!("{} ({total} {})", screen_name(screen), self.row_unit(screen, total))
        });
        match hint {
            Some(hint) => format!("{name}  ({hint})"),
            None => name,
        }
    }

    /// Resolves only the visible `offset`/`limit` window — a list can run into the thousands.
    fn rows(&self, s: &Session, screen: usize, offset: usize, limit: usize) -> Vec<Row> {
        let pending: HashSet<TrackId> = match &self.open_remote {
            Some((sid, _, node)) if screen == PLAYLISTS => s.remote_pending_ids(sid, node).into_iter().collect(),
            _ => HashSet::new(),
        };
        let track_rows = |tracks| tracks_to_rows(s, tracks, &pending);
        if let Some(matched) = self.filtered_tracks(s, screen) {
            let query = self.active_filter().unwrap_or_default();
            if matched.is_empty() {
                return vec![plain_row(format!("no matches for {query:?}"))];
            }
            return track_rows(matched.into_iter().skip(offset).take(limit).collect());
        }
        match screen {
            NOW_PLAYING => {
                if s.playing_context_len() == 0 {
                    // Nothing has ever been played this session — nothing to show a tracklist of yet.
                    vec![plain_row("nothing played yet — press Enter on a track to start playing")]
                } else {
                    track_rows(s.playing_context_window(offset, limit))
                }
            }
            SEARCH => {
                if s.results_len() == 0 {
                    match &self.last_query {
                        // A search ran and came back empty — say so.
                        Some(q) => vec![plain_row(format!(
                            "no results for {q:?} — check the Log pane (:log) for source errors"
                        ))],
                        None => vec![],
                    }
                } else {
                    track_rows(s.results_window(offset, limit))
                }
            }
            QUEUE => track_rows(s.queue_window(offset, limit)),
            HIST => track_rows(s.history_window(offset, limit)),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    track_rows(s.playlist_window(id, offset, limit))
                } else if let Some((sid, _, node)) = &self.open_remote {
                    track_rows(s.remote_playlist_window(sid, node, offset, limit))
                } else {
                    let playlists = s.playlists();
                    self.top_rows(s)
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(|row| {
                            let key = s.playlist_hotkey(&row.target());
                            let mut r = match &row {
                                TopRow::Local(id) => {
                                    let p = playlists.iter().find(|p| p.id == *id);
                                    let name = p.map(|p| p.name.clone()).unwrap_or_default();
                                    let count = p.map(|p| p.items.len()).unwrap_or(0);
                                    plain_row(format!("{name}  ({count} tracks)"))
                                }
                                TopRow::Remote(sid, name, _) => plain_row(format!("[{sid}] {name}")),
                            };
                            r.hotkeys = Cell::plain(key.map(String::from).unwrap_or_default());
                            r
                        })
                        .collect()
                }
            }
            _ => vec![],
        }
    }

    /// The current screen's full list length.
    fn list_len(&self, s: &Session, screen: usize) -> usize {
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

    // ---- key handling ---------------------------------------------------

    fn clamp_cursor(&mut self, len: usize) {
        let c = &mut self.cursor[self.screen];
        if len == 0 {
            *c = 0;
        } else if *c >= len {
            *c = len - 1;
        }
    }

    /// The screen index Shift-J/Shift-K and PageUp/PageDown's cursor-jump should act on.
    fn active_list_screen(&self) -> Option<usize> {
        match self.focus {
            Focus::Main => Some(self.screen),
            Focus::Pane(pane) => list_screen_for_pane(pane),
            Focus::Warnings => None,
        }
    }

    /// Shift-J/Shift-K and PageUp/PageDown on the main tracklist or a focused Queue/History pane.
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

    /// `clamp_cursor`, generalized to an explicit `screen` and folding in the forward step + length lookup.
    fn bump_pane_cursor(&mut self, screen: usize, step: usize) {
        self.cursor[screen] = self.cursor[screen].saturating_add(step);
        let len = self.with_session(|s| self.visible_track_ids(s, screen).len());
        let c = &mut self.cursor[screen];
        if len == 0 {
            *c = 0;
        } else if *c >= len {
            *c = len - 1;
        }
    }

    /// Visible list rows as of the last layout pass — `last_main_rect` minus its title row.
    fn list_h(&self) -> usize {
        self.last_main_rect.height().saturating_sub(LIST_TITLE_ROWS)
    }

    /// Keep `list_offset[screen]` a valid window around `cursor[screen]`.
    fn clamp_scroll(&mut self) {
        self.clamp_scroll_for(self.screen, self.list_h());
        // A focused docked list-pane has its own cursor and scroll window, sized to its own rect.
        if let Focus::Pane(pane) = self.focus
            && let Some(screen) = list_screen_for_pane(pane)
            && let Some(&(_, rect)) = self.last_pane_rects.iter().find(|(p, _)| *p == pane)
        {
            self.clamp_scroll_for(screen, rect.height().saturating_sub(1));
        }
    }

    fn clamp_scroll_for(&mut self, screen: usize, list_h: usize) {
        self.list_offset[screen] =
            follow_cursor_offset(self.cursor[screen], self.list_offset[screen], list_h);
    }

    /// Bounds-only safety clamp for `list_offset[screen]`.
    fn clamp_offset_bounds(&mut self, screen: usize, list_h: usize) {
        let len = self.with_session(|s| self.list_len(s, screen));
        self.list_offset[screen] = bound_offset(self.list_offset[screen], len, list_h);
    }

    /// Same idea as `jump_warnings`, for the "Add to Playlist" picker.
    fn jump_playlist_picker(&mut self, up: bool, step: usize) {
        let n = self.with_session(|s| s.playlists().len());
        let h = modal_list_h(self.last_screen_size.y, PLAYLIST_PICKER_LIST_TOP);
        CursorWindow { cursor: &mut self.playlist_picker_cursor, offset: &mut self.playlist_picker_offset }
            .jump(up, step, n, h);
    }

    /// Same idea as `follow_warnings_offset`, for the "Add to Playlist" picker.
    fn follow_playlist_picker_offset(&mut self) {
        let h = modal_list_h(self.last_screen_size.y, PLAYLIST_PICKER_LIST_TOP);
        CursorWindow { cursor: &mut self.playlist_picker_cursor, offset: &mut self.playlist_picker_offset }
            .follow(h);
    }

    // ---- plugin warnings (Spotify/SoundCloud login status) --------------

    // ---- help / shortcuts -------------------------------------------------

    // ---- add-to-playlist picker --------------------------------------

    fn open_playlist_picker(&mut self, track: TrackId) {
        self.playlist_picker_open = true;
        self.playlist_picker_cursor = 0;
        self.playlist_picker_offset = 0;
        self.playlist_picker_track = Some(track);
    }

    /// Enter (or a click) on the selected picker row.
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

    /// Run a plugin-registered `:`-command (e.g. `:spotify addlogin`) on a background thread.
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
                // `Screen` mode: fullscreen, one at a time.
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
                    // `side`/`stack` stay shared layout geometry regardless of `patch.pane`.
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
        // `Previous` only grows the queue when it actually wedges the just-played track back onto the front.
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

    /// `F` (`Action::ConfirmUnlike`): a Yes/No cursive dialog.
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

    fn handle_action(&mut self, action: Action) -> EventResult {
        match action {
            Action::Command(c) => self.run(c),
            // On the Search screen itself, same as switching to the Search tab (focuses the input too).
            Action::FocusSearch => {
                if self.screen == SEARCH {
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
                // Leaving the Playlists screen for anything else.
                if n != PLAYLISTS {
                    self.leave_playlists();
                } else if !was_playlists && self.open_playlist.is_none() && self.open_remote.is_none() {
                    // Switching back into Playlists fresh: restore the remembered playlist.
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
                // A different screen's list — any filter over the old one is meaningless now.
                self.filter_query = None;
                // Switching to Search focuses the input immediately, same as `/`.
                if n == SEARCH {
                    self.editing = Editing::Search;
                    self.buffer.clear();
                }
                self.clamp_scroll();
                EventResult::consumed()
            }
            Action::Activate => self.activate(),
            // The `Event::Key(Key::Enter)` handler already special-cases this and never forwards it here.
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

fn popup(msg: impl Into<String>) -> EventResult {
    let msg = msg.into();
    EventResult::with_cb(move |c: &mut Cursive| {
        c.add_layer(Dialog::info(msg.clone()));
    })
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

        // Resolved before the list so `rows` is only ever asked for the visible window.
        let list_h = self.list_h();
        let sel = self.cursor[self.screen];
        // Persisted, not recomputed from `sel` — see `list_offset`'s doc.
        let offset = self.list_offset[self.screen];

        // A docked Queue/History pane needs the same triple the main content does, for its own rect/screen/cursor.
        let list_panes: Vec<(Pane, Rect, usize, usize, usize)> = panes
            .iter()
            .filter_map(|&(pane, rect)| {
                let screen = list_screen_for_pane(pane)?;
                let pane_h = rect.height().saturating_sub(1);
                Some((pane, rect, screen, self.list_offset[screen], pane_h))
            })
            .collect();

        // One lock for the whole frame: pull every session-derived value out here, then render without the guard.
        let want_settings = panes.iter().any(|(p, _)| *p == Pane::Settings);
        let (
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
            main_title,
            list_loading,
        ) = self.with_session(|s| {
                let rows = self.rows(s, self.screen, offset, list_h);
                let total = self.list_len(s, self.screen);
                let main_title = self.list_title(s, self.screen);
                // A paginated remote list only knows what it has loaded so far.
                let list_loading = self.screen == PLAYLISTS
                    && match &self.open_remote {
                        Some((sid, _, node)) => s.remote_playlist_loading(sid, node),
                        None => {
                            self.open_playlist.is_none()
                                && s.source_ids().iter().any(|sid| s.remote_playlists_loading(sid))
                        }
                    };
                let now_playing = s.now_playing();
                let np = now_playing
                    .as_ref()
                    .map(|t| format!("{} - {}", t.display_artist(), t.title))
                    .unwrap_or_else(|| "nothing playing".to_string());
                let bpm_tag = bpm_status_tag(s, now_playing.as_ref());
                let shuffle = s.shuffle();
                let settings = if want_settings { settings_entries(s, self.pane_cfg) } else { Vec::new() };
                let warn_count = s.plugin_statuses().iter().filter(|(_, h)| !h.is_ok()).count();
                // Feed the scan walk the visible list every redraw so it's prioritized over store order.
                if let Some(scan) = &s.scan {
                    let view_screen = self.active_screen();
                    let ids = self.visible_track_ids(s, view_screen);
                    let highlighted = self.cursor[view_screen];
                    scan.follow_view(ids, highlighted);
                }
                let pane_rows: Vec<(Pane, String, Vec<Row>, usize)> = list_panes
                    .iter()
                    .map(|&(pane, _, screen, offset, pane_h)| {
                        let rows = self.rows(s, screen, offset, pane_h);
                        let total = self.list_len(s, screen);
                        (pane, self.list_title(s, screen), rows, total)
                    })
                    .collect();
                (
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
                    main_title,
                    list_loading,
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
                // `[...]` is the focus marker every pane title uses (see `draw_pane`).
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
            // One-cell separator between main content and the pane block.
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

        // Row 0 of the whole screen.
        let marquee_offset = {
            let mut m = self.tab_marquee.lock().unwrap();
            if m.0 != np {
                m.0 = np.clone();
                m.1 = Instant::now();
            }
            m.1.elapsed().as_secs() as usize
        };
        draw_tab_bar(printer, self.screen, &np, marquee_offset, &st.state);
        draw_row_list(&printer.windowed(main_rect), &main_title, &rows, offset, sel, total);

        // command / hint line (row above the status line).
        let bottom = printer.size.y.saturating_sub(2);
        let line = match &self.editing {
            Editing::Search => format!("/{}", self.buffer),
            Editing::CommandLine => format!(":{}", self.buffer),
            Editing::PluginSetup(_) => format!("> {}", self.buffer),
            Editing::Filter => format!("/{}", self.buffer),
            // `queue_feedback` (this keypress only) wins over `membership_feedback`.
            Editing::None => self
                .queue_feedback
                .clone()
                .or(membership_feedback.map(|m| format!("  {m}")))
                .or(self.hotkey_feedback.clone().map(|m| format!("  {m}")))
                .unwrap_or_else(|| {
                    // The Playlists screen's own hint replaces the generic one when a row/open playlist can take a hotkey.
                    if self.screen == PLAYLISTS
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

        // Cursor position in the main list / its length, right-aligned before the warnings button.
        let warn_w = if warn_count > 0 {
            warnings_label(warn_count).chars().count().min(printer.size.x)
        } else {
            0
        };
        if total > 0 {
            let more = if list_loading { "+" } else { "" };
            let unit = self.row_unit(self.screen, total);
            let readout = format!("{}/{total}{more} {unit}", sel.min(total - 1) + 1);
            let x = printer.size.x.saturating_sub(warn_w + readout.width() + 1);
            if x >= line.width() + 2 {
                printer.print((x, bottom), &readout);
            }
        }

        // status line, pinned to the very last row.
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

        // Warnings button — right-aligned on the hint line, drawn last so it overwrites that tail.
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
        // The one layout hook that gets `&mut self` with the resolved screen size.
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
        // Synthetic periodic wakeup.
        if event == Event::Refresh {
            return EventResult::Ignored;
        }
        // Transient queue/wedge feedback shows for one keypress.
        let is_mouse_followup =
            matches!(event, Event::Mouse { event: MouseEvent::Release(_) | MouseEvent::Hold(_), .. });
        if !is_mouse_followup {
            self.queue_feedback = None;
            self.hotkey_feedback = None;
            self.with_session(|s| s.clear_membership_feedback());
        }
        // Active text field: capture everything, except a click elsewhere or a digit as Search's first keystroke.
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
                    // The filter narrows live as you type.
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

        // Fullscreen Screen-mode pane: Esc closes it, nav keys scroll it, everything else is swallowed.
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

        // The warnings modal: fullscreen, own nav (mirrors `screen_pane` above).
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

        // The "Add to Playlist" picker (`+` on a selected track).
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

        // The "press a key to bind" sub-popup.
        if self.hotkey_capture.is_some() {
            return match event {
                Event::Key(Key::Esc) => {
                    self.hotkey_capture = None;
                    EventResult::consumed()
                }
                // Same clear gesture as the hotkey menu's row list (`clear_selected_hotkey`).
                Event::Key(Key::Backspace) => self.clear_captured_hotkey(),
                ev => match key_name(&ev) {
                    Some(k) if k.chars().count() == 1 => self.bind_captured_key(k.chars().next().unwrap()),
                    _ => EventResult::consumed(),
                },
            };
        }

        // The hotkey-menu modal: fullscreen, own nav (mirrors `warnings_open` above).
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

        // The help/shortcuts modal: fullscreen, own nav (mirrors `warnings_open`/`hotkey_menu_open` above).
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

        // The tab bar lives on the fixed top row of the whole screen, never `last_main_rect`.
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

        // The warnings button lives on the fixed bottom-2 row of the whole screen, never `last_main_rect`.
        if self.warn_count() > 0
            && let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.x < self.last_screen_size.x
            && local.y == self.last_screen_size.y.saturating_sub(2)
        {
            self.open_warnings();
            return EventResult::consumed();
        }

        // The bottom status line's transport cluster, scrubber, and bpm/shuffle tags.
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

        // Mouse: routed separately from the keyboard path below entirely, and returned early.
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

        // Any key other than Enter defocuses the warnings button, then is handled as if `fallback_focus()` had focus.
        if self.focus == Focus::Warnings && defocuses_warnings(&event) {
            self.focus = self.fallback_focus();
        }

        // One lock: taking the session guard twice in one statement deadlocks.
        let len = self.with_session(|s| {
            let tracks = self.visible_track_ids(s, self.screen).len();
            if self.screen == PLAYLISTS
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
                let c = &mut self.cursor[self.screen];
                *c = c.saturating_sub(1);
                EventResult::consumed()
            }
            Event::Key(Key::Down) | Event::Char('j') if self.focus == Focus::Main => {
                let s = self.screen;
                self.cursor[s] = self.cursor[s].saturating_add(1);
                self.clamp_cursor(len);
                EventResult::consumed()
            }
            Event::Key(Key::Right) => self.run(Command::Seek(5000)),
            Event::Key(Key::Left) => self.run(Command::Seek(-5000)),
            // Shift-J/Shift-K jump the main tracklist or a focused list-pane, or page-scroll a focused non-list pane.
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
                if self.screen == PLAYLISTS
                    && (self.open_playlist.is_some() || self.open_remote.is_some()) =>
            {
                self.open_playlist = None;
                self.open_remote = None;
                // Explicitly backing out to the list means "forget this", unlike switching screens.
                self.remembered_playlist = None;
                self.cursor[PLAYLISTS] = 0;
                self.filter_query = None; // going back — the filtered list no longer applies
                EventResult::consumed()
            }
            // Nav keys past this point only apply while a pane is focused.
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
            // PageUp/PageDown: same split as Shift-J/Shift-K above, also reaching `Focus::Main`.
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
            // With a playlist selected on the Playlists screen, backtick binds that playlist instead of opening the menu.
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
                        self.play_track_at(active, self.cursor[active])
                    }
                    action => self.handle_action(action),
                }
            }
            ev => match key_name(&ev) {
                Some(k) => {
                    let active = self.active_screen();
                    let sel = self.with_session(|s| self.selected_track(s, active));
                    // Per-user playlist hotkeys win over a built-in command when a key names a playlist target.
                    let hotkeys = self.hotkeys_map();
                    match keybindings::hotkey_toggle(&k, sel, &hotkeys) {
                        Some(cmd) => self.run(cmd),
                        None => self.handle_action(keybindings::map(&k, sel, &hotkeys)),
                    }
                }
                None => EventResult::Ignored,
            },
        };
        // Cheap and covers every arm above uniformly, including ones reached via `handle_action`/keybindings.
        self.clamp_scroll();
        result
    }
}
