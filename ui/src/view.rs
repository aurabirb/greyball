//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.

use std::thread;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cursive::{Cursive, Printer, Rect, Vec2, View};
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle, Effect, Style};
use cursive::view::CannotFocus;
use cursive::views::Dialog;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use unicode_width::UnicodeWidthStr;

use core::{
    Axis, BindError, BrowseNode, Command, CoreEvent, Dispatch, HotkeyMembership, HotkeyTarget, LogBuf,
    PaneLayoutConfig, PaneMode, PlayerState, Playlist, PlaylistId, Plugin, PluginHealth, ScanMode, Session,
    SetupKind, Side, SourceId, TOGGLABLE_SOURCES, TrackId,
};

use crate::{SessionHandle, command, keybindings};
use crate::command::Pane;
use crate::keybindings::Action;
use crate::row::RowItem;

use scroll::{
    CursorWindow, LIST_JUMP_STEP, PAGE_SCROLL_STEP, WHEEL_STEP, bound_offset, draw_scrollbar,
    follow_cursor_offset, modal_list_h, stepped_cursor,
};
use text::{in_span, ms, pad_right_aligned, truncate, truncate_ellipsis, wrap};

mod scroll;
mod text;

pub(crate) use text::pad;
pub use text::{SCROLL_GAP, marquee_offset, scroll_title};

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

fn list_screen_for_pane(pane: Pane) -> Option<usize> {
    match pane {
        Pane::Queue => Some(QUEUE),
        Pane::History => Some(HIST),
        Pane::Log | Pane::Settings | Pane::Vis => None,
    }
}

/// Rows reserved at the very top of the terminal and bottom.
const TAB_BAR_ROWS: usize = 1;
const BOTTOM_BAR_ROWS: usize = 2;
/// Two clicks on the same row within this long count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
/// Row the warnings modal's plugin list starts on (row 0 = title, row 1 = blank spacer).
const WARNINGS_LIST_TOP: usize = 2;
/// Cap on how many plugin messages the warnings modal's bottom section shows.
const WARNINGS_MESSAGES_MAX: usize = 5;
/// Row the hotkey-menu modal's playlist list starts on — same shape as `WARNINGS_LIST_TOP`.
const HOTKEY_LIST_TOP: usize = 2;
/// Row the "Add to Playlist" picker's list starts on — same shape as `HOTKEY_LIST_TOP`.
const PLAYLIST_PICKER_LIST_TOP: usize = 2;
/// Row the help screen's content starts on.
const HELP_LIST_TOP: usize = 1;

/// `Action::CyclePaneLayout`'s rotation, one `(side, stack)` step per press.
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

/// One row of the Playlists screen's top-level list.
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

/// Display name for a `TopRow`.
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

/// A remote browse folder/playlist as tracked by `open_remote`/`RememberedPlaylist::Remote`.
type RemoteOpen = (SourceId, String, BrowseNode);

/// Which kind of playlist view was open on the Playlists screen when it was left for another screen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RememberedPlaylist {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode),
}

/// Pure state transition backing `leave_playlists`.
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

/// What to reopen on switching back to Playlists; a remembered local playlist that no longer exists is dropped.
fn resolve_remembered_playlist(
    remembered: Option<RememberedPlaylist>,
    playlists: &[Playlist],
) -> Option<RememberedPlaylist> {
    remembered.filter(|r| match r {
        RememberedPlaylist::Local(id) => playlists.iter().any(|p| p.id == *id),
        RememberedPlaylist::Remote(..) => true,
    })
}


/// A run of one cell's text sharing a style; `color: None` draws in the row's own color.
#[derive(Clone)]
struct Span {
    text: String,
    color: Option<Color>,
    italic: bool,
}

/// One column's rendered cell, from `render_cell`.
#[derive(Clone)]
struct Cell {
    spans: Vec<Span>,
}

impl Cell {
    fn plain(text: impl Into<String>) -> Self {
        Self::colored(text, None)
    }

    fn colored(text: impl Into<String>, color: Option<Color>) -> Self {
        Self { spans: vec![Span { text: text.into(), color, italic: false }] }
    }

    fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn styled(&self) -> bool {
        self.spans.iter().any(|s| s.italic || s.color.is_some())
    }
}

/// A rendered list row; every column is a `Cell` produced by `render_cell`.
struct Row {
    tags: Cell,
    main: Cell,
    /// One letter per hotkey-bound playlist holding this track, italic while that membership is pending.
    hotkeys: Cell,
    source: Cell,
    duration: Cell,
    current: bool,
    /// This row's own presence in the list on screen is still settling (a remote add/remove in flight).
    pending: bool,
}

/// A `Row` with only its main column set — placeholder messages and the Playlists top level.
fn plain_row(main: impl Into<String>) -> Row {
    Row {
        tags: Cell::plain(""),
        main: Cell::plain(main),
        source: Cell::plain(""),
        duration: Cell::plain(""),
        hotkeys: Cell::plain(""),
        current: false,
        pending: false,
    }
}

/// `MedleyView::filter_cache`'s contents.
struct FilterCache {
    screen: usize,
    /// `(open_playlist, open_remote)` identity, so two same-length playlists never share a cache entry.
    list_id: (Option<PlaylistId>, Option<(SourceId, BrowseNode)>),
    query: String,
    source_len: usize,
    result: Vec<core::Track>,
}

/// The `/`-filter's rank for one row against `query`, low-to-high, `None` if it doesn't match at all.
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

    /// `Screen`-mode: `pane` fullscreen, title/content on top, an `Esc to close` hint on the bottom row.
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
            // `toggle_pane` never routes these two here.
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

    /// "{id}: {msg}" for every plugin currently reporting a non-`Ok` health.
    fn warnings_messages(&self) -> Vec<String> {
        self.with_session(|s| {
            s.plugin_statuses()
                .into_iter()
                .filter_map(|(id, health)| health.message().map(|m| format!("{id}: {m}")))
                .collect()
        })
    }

    /// Rows the bottom messages section reserves.
    fn warnings_messages_h(&self) -> usize {
        let n = self.warnings_messages().len();
        if n == 0 { 0 } else { 1 + n.min(WARNINGS_MESSAGES_MAX) }
    }

    /// Visible plugin rows in the warnings modal's navigable list, given the whole-screen height.
    fn warnings_list_h(&self, screen_h: usize) -> usize {
        modal_list_h(screen_h, WARNINGS_LIST_TOP).saturating_sub(self.warnings_messages_h())
    }

    /// Fullscreen plugin-warnings modal.
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

    /// Fullscreen "Hotkeys" modal (backtick, off the Playlists screen).
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

    /// Standalone "press a key to bind" modal for a Playlists-screen row.
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

    /// The help screen's content lines.
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

    /// Fullscreen help/shortcuts modal (`?` or `:help`).
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

    /// Move `help_scroll` by `step` rows, clamped to the scrollable range.
    fn jump_help(&mut self, up: bool, step: usize) {
        self.help_scroll =
            if up { self.help_scroll.saturating_sub(step) } else { self.help_scroll.saturating_add(step) };
        let len = self.help_lines().len();
        let h = self.last_screen_size.y.saturating_sub(1).saturating_sub(HELP_LIST_TOP);
        self.help_scroll = bound_offset(self.help_scroll, len, h);
    }

    /// `pane`'s own placement: its `pane_mode_overrides` entry, else the shared default.
    fn pane_mode(&self, pane: Pane) -> PaneMode {
        self.pane_mode_overrides.get(&pane).copied().unwrap_or(self.pane_cfg.mode)
    }

    /// Open/close `pane`, per its own `pane_mode` — `:log`, `:settings`, bare `:vis`, `:queue`, `:history`.
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

    /// Sync cursive's own redraw rate to whether/how fast the Vis pane needs to animate.
    fn vis_fps_cb(&self) -> EventResult {
        let vis_open = self.open_panes.contains(&Pane::Vis) || self.screen_pane == Some(Pane::Vis);
        self.vis.set_enabled(vis_open);
        let fps = if vis_open { crate::vis::FPS } else { crate::BASELINE_FPS };
        EventResult::with_cb(move |siv| siv.set_fps(fps))
    }

    /// Line-scroll for Log/Vis only.
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
        // Pin (or release) the Log pane's view against `log`'s current length.
        self.log_pin = log_pin_after_scroll(self.log_scroll, self.log_pin, self.log.snapshot().len());
        self.clamp_pane_scroll(pane);
    }

    /// Settings pane's row cursor.
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

    /// The (width, content-row-count) `draw_pane` actually renders `pane` into right now.
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

    /// Keep `log_scroll` inside the actual scrollable range for `pane`'s current content and on-screen size.
    fn clamp_pane_scroll(&mut self, pane: Pane) {
        let Some((width, h)) = self.pane_content_dims(pane) else { return };
        let lines = match pane {
            Pane::Log => self.log_render_lines().0,
            Pane::Settings | Pane::Vis | Pane::Queue | Pane::History => return,
        };
        let wrapped_len: usize = lines.iter().map(|l| wrap(l, width).len()).sum();
        self.log_scroll = bound_offset(self.log_scroll, wrapped_len, h);
    }

    /// The Log pane's content/scroll for this frame.
    fn log_render_lines(&self) -> (Vec<String>, usize) {
        let snapshot = self.log.snapshot();
        let len = log_visible_len(self.log_scroll, self.log_pin, snapshot.len());
        (snapshot[..len].to_vec(), self.log_scroll)
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

    /// The live hotkey remap table, collected into the shape `keybindings::map`/`hotkey_toggle` take.
    fn hotkeys_map(&self) -> HashMap<char, HotkeyTarget> {
        self.with_session(|s| s.hotkeys()).into_iter().collect()
    }

    // ---- snapshot helpers -------------------------------------------------

    /// The Playlists screen's combined top-level list.
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
    fn hotkey_rows(&self) -> Vec<core::BuiltinAction> {
        core::BuiltinAction::ALL.iter().map(|&(a, _)| a).collect()
    }

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

    /// Every track already loaded for `screen`'s list, unwindowed.
    fn all_tracks_for_screen(&self, s: &Session, screen: usize) -> Vec<core::Track> {
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

    /// Cheap count of `all_tracks_for_screen`'s source list.
    fn filterable_source_len(&self, s: &Session, screen: usize) -> usize {
        match screen {
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

    /// Whether `/` on `screen` should filter it locally rather than jump to Search.
    fn filterable_screen(&self, screen: usize) -> bool {
        match screen {
            NOW_PLAYING | QUEUE | HIST => true,
            PLAYLISTS => self.open_playlist.is_some() || self.open_remote.is_some(),
            _ => false,
        }
    }

    /// The active local filter query.
    fn active_filter(&self) -> Option<&str> {
        match &self.editing {
            Editing::Filter => Some(self.buffer.as_str()),
            _ => self.filter_query.as_deref(),
        }
    }

    /// `screen`'s tracks narrowed and ranked by the active local filter.
    fn filtered_tracks(&self, s: &Session, screen: usize) -> Option<Vec<core::Track>> {
        let query = self.active_filter()?;
        if query.is_empty() || !self.filterable_screen(screen) {
            return None;
        }
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

    /// Selects row `idx` of `screen`'s list.
    fn click_row(&mut self, screen: usize, idx: usize) -> EventResult {
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

    /// The local playlist selected/open on the Playlists screen.
    fn selected_playlist(&self, s: &Session) -> Option<PlaylistId> {
        if self.screen != PLAYLISTS {
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

    /// The playlist (local *or* remote) selected/open on the Playlists screen.
    fn selected_hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        if self.screen != PLAYLISTS {
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

    /// Reset the current screen's cursor/scroll to the top.
    fn reset_filter_selection(&mut self) {
        let screen = self.screen;
        self.cursor[screen] = 0;
        self.list_offset[screen] = 0;
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

    /// Move `warnings_cursor` by `step` rows, keeping `warnings_offset` following it via `CursorWindow`.
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

    /// Resync `warnings_offset` to `warnings_cursor` without moving the cursor.
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

    /// Number of plugins currently reporting a non-`Ok` health.
    fn warn_count(&self) -> usize {
        self.with_session(|s| s.plugin_statuses().iter().filter(|(_, h)| !h.is_ok()).count())
    }

    /// `Enter` (or a click) on the selected warnings-modal row.
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

    fn open_hotkey_menu(&mut self) {
        self.hotkey_menu_open = true;
        self.hotkey_menu_cursor = 0;
        self.hotkey_menu_offset = 0;
        self.hotkey_capture = None;
        self.hotkey_feedback = None;
        self.with_session(|s| s.clear_membership_feedback());
    }

    /// Enter on the selected hotkey-menu row.
    fn open_hotkey_capture(&mut self) {
        let Some(&action) = self.hotkey_rows().get(self.hotkey_menu_cursor) else {
            return;
        };
        self.hotkey_capture = Some(HotkeyTarget::Builtin(action));
        self.hotkey_feedback = None;
    }

    /// Opens the standalone "press a key to bind" modal for `target` on the Playlists screen.
    fn open_playlist_hotkey_modal(&mut self, target: HotkeyTarget) {
        self.hotkey_capture = Some(target);
        self.hotkey_feedback = None;
    }

    /// This row's display name, looked up fresh.
    fn hotkey_row_name_for(&self, target: &HotkeyTarget) -> String {
        match target {
            HotkeyTarget::Builtin(action) => action.label().to_string(),
            HotkeyTarget::Local(_) | HotkeyTarget::Remote(..) => self.with_session(|s| {
                let playlists = s.playlists();
                self.top_rows(s).into_iter().find(|r| &r.target() == target).map(|r| top_row_name(&r, &playlists))
            }).unwrap_or_default(),
        }
    }

    /// Binds `key` to the `hotkey_capture` target, reports the result in `hotkey_feedback`, closes the sub-popup.
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
            Err(BindError::BuiltinKey(blocking)) => {
                let blocking_name = self.hotkey_row_name_for(&blocking);
                format!("Can't bind '{key}': already used by built-in {blocking_name}")
            }
            Err(BindError::SyntheticPlaylist) => {
                format!("Can't bind '{key}': {name} isn't a real playlist — use like/unlike instead")
            }
        });
        EventResult::consumed()
    }

    /// Backspace on the selected hotkey-menu row: clears that row's binding, if it has one.
    fn clear_selected_hotkey(&mut self) -> EventResult {
        let Some(&action) = self.hotkey_rows().get(self.hotkey_menu_cursor) else {
            return EventResult::consumed();
        };
        self.clear_hotkey(HotkeyTarget::Builtin(action))
    }

    /// Backspace on the standalone playlist hotkey modal.
    fn clear_captured_hotkey(&mut self) -> EventResult {
        let Some(target) = self.hotkey_capture.take() else {
            return EventResult::consumed();
        };
        self.clear_hotkey(target)
    }

    /// Shared by `clear_selected_hotkey`/`clear_captured_hotkey`.
    fn clear_hotkey(&mut self, target: HotkeyTarget) -> EventResult {
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        self.hotkey_feedback = key.map(|k| format!("Unbound '{k}'"));
        EventResult::consumed()
    }

    /// Run a plugin's `setup()` on a background thread.
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
            // See `Session::plugin_statuses`.
            guard.record_setup_result(id, health);
            drop(guard);
            // The UI's cue to redraw the warnings panel / pick up whatever just got registered.
            bus.send(CoreEvent::PluginStatusChanged);
            if succeeded {
                bus.send(CoreEvent::PluginLoginSucceeded);
            }
        });
    }

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

    /// Mouse handling for the main list — kept entirely separate from the keyboard path in `on_event`.
    fn handle_mouse(&mut self, offset: Vec2, position: Vec2, event: MouseEvent) -> Option<EventResult> {
        let local = position.checked_sub(offset)?;
        let rect = self.last_main_rect;
        let (rx, ry) = (rect.top_left().x, rect.top_left().y);
        if local.x < rx || local.x >= rx + rect.width() || local.y < ry || local.y >= ry + rect.height() {
            return None;
        }
        self.focus = Focus::Main;
        let screen = self.screen;
        match event {
            // Scrolls the *window* only.
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
            // Row 0 is the list's title row, not a clickable list row.
            MouseEvent::Press(MouseButton::Left) => {
                let row = local.y - ry;
                if row < LIST_TITLE_ROWS || row >= LIST_TITLE_ROWS + self.list_h() {
                    return Some(EventResult::consumed());
                }
                let idx = self.list_offset[screen] + (row - LIST_TITLE_ROWS);
                let len = self.with_session(|s| self.list_len(s, screen));
                if idx < len {
                    return Some(self.click_row(screen, idx));
                }
                Some(EventResult::consumed())
            }
            _ => None,
        }
    }

    /// Mouse handling for one open pane's rect.
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
        // Log is free-form terminal output the user wants to select/copy with the mouse.
        if pane == Pane::Log && matches!(event, MouseEvent::Press(_) | MouseEvent::Hold(_) | MouseEvent::Release(_))
        {
            return None;
        }
        self.focus = Focus::Pane(pane);

        let Some(screen) = list_screen_for_pane(pane) else {
            // Log/Settings/Vis: no per-row click target, but the wheel still scrolls or is simply absorbed.
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

    /// `:open <url-or-path>`'s remote-link case.
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

    /// `:open <url-or-path>`'s argument case.
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
        if self.screen == PLAYLISTS && self.open_playlist.is_none() && self.open_remote.is_none() {
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

/// Content for the help/shortcuts screen.
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

/// The single per-column cell renderer — a pure function of the track plus the state handed in.
fn render_cell(col: Column, t: &core::Track, cached: bool, visible: &[String], hotkeys: &[HotkeyMembership]) -> Cell {
    match col {
        Column::Tags => {
            let color = visible.iter().find_map(|attr| t.attrs.get(attr).and_then(|v| tag_color(attr, v)));
            Cell::colored(t.tags(visible), color)
        }
        Column::Main => Cell::plain(t.main()),
        Column::Source => Cell::plain(t.source(cached)),
        Column::Duration => Cell::plain(t.duration()),
        Column::Hotkeys => Cell {
            spans: hotkeys
                .iter()
                .filter_map(|m| {
                    let italic = m.pending.contains(&t.id);
                    (italic || m.members.contains(&t.id))
                        .then(|| Span { text: m.key.to_string(), color: None, italic })
                })
                .collect(),
        },
    }
}

/// The one track-row builder every list and docked pane goes through.
fn tracks_to_rows(s: &Session, tracks: Vec<core::Track>, pending: &HashSet<TrackId>) -> Vec<Row> {
    let now_playing = s.now_playing_id();
    let visible = &s.cfg.visible_track_attrs;
    let hotkeys = s.hotkey_memberships();
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
                pending: pending.contains(&t.id),
            }
        })
        .collect()
}

/// Per-tag-attr cell renderer, keyed by attr name (a `Config::visible_track_attrs` entry).
fn tag_color(attr: &str, value: &str) -> Option<Color> {
    match attr {
        "bpm" => bpm_color(value),
        _ => None,
    }
}

/// Colors bpm on a blue→green→red gradient clamped to 60-180 bpm.
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

/// Partitions the full screen into the main content rect and one rect per currently-open **embedded** pane.
fn split(total: Vec2, open_panes: &[Pane], cfg: PaneLayoutConfig) -> (Rect, Vec<(Pane, Rect)>) {
    let band = Vec2::new(total.x, total.y.saturating_sub(TAB_BAR_ROWS + BOTTOM_BAR_ROWS));
    if open_panes.is_empty() {
        return (Rect::from_size((0, TAB_BAR_ROWS), band), Vec::new());
    }
    // One cell is reserved between main and the pane block for the "│"/"─" `draw()` prints there.
    const GUTTER: usize = 1;
    let n = open_panes.len();

    let (main, panes) = match cfg.side {
        // Left/Right: pane block is a narrow column alongside main, full band height.
        Side::Left | Side::Right => {
            let avail = band.x.saturating_sub(GUTTER);
            // Fixed fraction, floored so it never eats the whole screen; MVP — no per-pane resizing yet.
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
                        // Side by side across the full width — "split down the middle" for two panes.
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

/// Effective config as togglable/info rows, for both the embedded pane and the screen-mode modal.
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

/// Settings pane's title + rows, with a highlight on `cursor`.
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

/// Rows every list view spends on its title, in the main area and docked panes alike.
const LIST_TITLE_ROWS: usize = 1;

/// A title row plus a window of rows and a scrollbar, shared by the main list and docked panes.
fn draw_row_list(printer: &Printer, title: &str, rows: &[Row], offset: usize, sel: usize, total: usize) {
    // Reserve the rightmost column of the list body as a scrollbar gutter.
    let content_w = printer.size.x.saturating_sub(1);
    let indent = main_col_start(content_w);
    printer.with_color(ColorStyle::title_primary(), |p| {
        p.print((0, 0), &pad(&format!("{:indent$}{title}", ""), content_w));
    });
    let body_h = printer.size.y.saturating_sub(LIST_TITLE_ROWS);
    let body = printer.windowed(Rect::from_size((0, LIST_TITLE_ROWS), (printer.size.x, body_h)));
    draw_list_body(&body, rows, offset, sel, total);
}

/// Fills every row of `printer` with `rows` plus a scrollbar gutter — no title row of its own.
fn draw_list_body(printer: &Printer, rows: &[Row], offset: usize, sel: usize, total: usize) {
    let content_w = printer.size.x.saturating_sub(1);
    let list_h = printer.size.y;
    let layout = column_layout(content_w.saturating_sub(ROW_MARK_W));
    for (y, row) in rows.iter().enumerate() {
        let selected = y + offset == sel;
        let mark = if row.current { "> " } else { "  " };
        let cells = [&row.tags, &row.main, &row.hotkeys, &row.source, &row.duration];
        let [tags, main, hotkeys, source, duration] = cells.map(Cell::text);
        let cols = five_col(&tags, &main, &hotkeys, &source, &duration, content_w.saturating_sub(ROW_MARK_W));
        let line = pad(&format!("{mark}{cols}"), content_w);
        let mut row_style = Style::from(if selected {
            ColorStyle::highlight()
        } else if row.current {
            ColorStyle::secondary()
        } else {
            ColorStyle::primary()
        });
        if row.pending {
            row_style = row_style.combine(Effect::Italic).combine(Effect::Dim);
        }
        printer.with_style(row_style, |p| p.print((0, y), &line));
        // The selection/now-playing color takes the whole line; a span's own color only shows on a plain row.
        let plain = !selected && !row.current;
        for (&(start, width, right_aligned), cell) in layout.iter().zip(cells) {
            if !cell.styled() {
                continue;
            }
            let end = ROW_MARK_W + start + width;
            let indent = if right_aligned { width.saturating_sub(cell.text().width()) } else { 0 };
            let mut x = ROW_MARK_W + start + indent;
            for span in &cell.spans {
                let text = truncate(&span.text, end.saturating_sub(x));
                let mut style = row_style;
                if plain && let Some(color) = span.color {
                    style = style.combine(ColorStyle::front(color));
                }
                // Combining an effect twice toggles it back off.
                if span.italic && !row.pending {
                    style = style.combine(Effect::Italic);
                }
                printer.with_style(style, |p| p.print((x, y), &text));
                x += text.width();
            }
        }
    }
    draw_scrollbar(printer, content_w, list_h, offset, total);
}

/// Each `five_col` column's `(start, width, right-aligned)` in `Row` field order.
fn column_layout(width: usize) -> Vec<(usize, usize, bool)> {
    let fixed = TAGS_COL_W + SOURCE_COL_W + DURATION_COL_W + HOTKEYS_COL_W + 4;
    if width <= fixed {
        return Vec::new();
    }
    let main_w = width - fixed;
    let main_start = TAGS_COL_W + 1;
    let hotkeys_start = main_start + main_w + 1;
    let source_start = hotkeys_start + HOTKEYS_COL_W + 1;
    let duration_start = source_start + SOURCE_COL_W + 1;
    vec![
        (0, TAGS_COL_W, true),
        (main_start, main_w, false),
        (hotkeys_start, HOTKEYS_COL_W, false),
        (source_start, SOURCE_COL_W, false),
        (duration_start, DURATION_COL_W, false),
    ]
}

/// Width of a row's leading now-playing marker (`"> "`/`"  "`).
const ROW_MARK_W: usize = 2;

/// Column a row's main text starts at — what the title row is indented by to line up with it.
fn main_col_start(content_w: usize) -> usize {
    let layout = column_layout(content_w.saturating_sub(ROW_MARK_W));
    ROW_MARK_W + layout.get(1).map_or(0, |&(start, ..)| start)
}

/// The main content's row-0 tabs, `(screen, bare name)`, in both display and hotkey order.
const TABS: [(usize, &str); 5] = [
    (NOW_PLAYING, "Now Playing"),
    (PLAYLISTS, "Playlists"),
    (SEARCH, "Search"),
    (HIST, "History"),
    (QUEUE, "Queue"),
];

/// Background for the active tab only — every other tab uses the terminal's default colors, unstyled.
const ACTIVE_TAB_BG: Color = Color::Dark(BaseColor::Red);

/// `screen`'s bare tab name, shared by the tab strip and the docked Queue/History pane title.
fn screen_name(screen: usize) -> &'static str {
    TABS.iter().find(|&&(s, _)| s == screen).map_or("", |&(_, name)| name)
}

/// A tab's rendered button text.
fn tab_label(index: usize, name: &str, collapsed: bool) -> String {
    if collapsed {
        let letter = name.chars().next().unwrap_or('?');
        format!(" {letter} ")
    } else {
        format!(" [{}] {name} ", index + 1)
    }
}

/// Total width of every tab label plus the gaps between them, for the given `collapsed` mode.
fn tabs_width(collapsed: bool) -> usize {
    let gap = 1;
    TABS.iter().enumerate().map(|(i, &(_, label))| tab_label(i, label, collapsed).chars().count()).sum::<usize>()
        + gap * TABS.len().saturating_sub(1)
}

/// Each tab's screen, start column and width, from column 0 with a 1-column gap between tabs.
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

/// Which tab (if any) occupies column `x` of a tab bar `width` columns wide.
fn tab_at_x(x: usize, width: usize, state: &PlayerState) -> Option<usize> {
    let collapsed = tab_bar_collapsed(width, state);
    tab_layout(collapsed)
        .into_iter()
        .find(|&(_, start, w)| x >= start && x < start + w && start < width)
        .map(|(screen, ..)| screen)
}

/// Whether the tab labels must collapse to single letters to leave room for the transport strip.
fn tab_bar_collapsed(content_w: usize, state: &PlayerState) -> bool {
    let gap = TRANSPORT_GAP;
    let transport_w = transport_layout(0, state).last().map_or(0, |&(_, s, w)| s + w);
    tabs_width(false) + gap + transport_w > content_w
}

/// One of the top-bar's transport buttons, drawn right after the tabs.
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

/// Prev/next glyphs, shared by the top-bar transport strip and the bottom status line.
const PREV_ICON: &str = "⏮";
const NEXT_ICON: &str = "⏭";

/// Gap on either side of the top-bar transport cluster.
const TRANSPORT_GAP: usize = 2;

/// The three transport buttons' text, space-padded like `tab_label`.
fn transport_labels(state: &PlayerState) -> [(Transport, String); 3] {
    [
        (Transport::Prev, format!(" {PREV_ICON} ")),
        (Transport::PlayPause, format!(" {} ", player_action_glyph(state))),
        (Transport::Next, format!(" {NEXT_ICON} ")),
    ]
}

/// Transport buttons' start column and width, packed left-to-right from `start` with no gap between them.
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

/// Which transport button occupies column `x` of a tab bar `width` columns wide.
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

/// Row 0 of the whole screen.
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
fn log_pin_after_scroll(scroll: usize, pin: Option<usize>, live_len: usize) -> Option<usize> {
    if scroll == 0 { None } else { Some(pin.unwrap_or(live_len)) }
}

/// Length of the Log snapshot to actually render this frame.
fn log_visible_len(scroll: usize, pin: Option<usize>, live_len: usize) -> usize {
    if scroll == 0 { live_len } else { pin.unwrap_or(live_len).min(live_len) }
}

/// Draw a pane's title + content into its own (already-windowed) printer.
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
    // Each *wrapped* line counts as a row, so a long line takes the space it needs.
    let visible: Vec<&String> = if pane == Pane::Log {
        let total = wrapped.len();
        // Clamp to the top-most full window, so scrolling past the oldest line freezes there.
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

/// Only ever called for `count > 0` — the button isn't drawn at all when there are no warnings.
fn warnings_label(count: usize) -> String {
    format!(" ⚠ warnings ({count}) ")
}

/// Fixed widths for the tags/hotkeys/source/duration columns of a track row (see `ui::row::RowItem`).
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

/// Whether a click on (`screen`, `idx`) at `now`, given the previous click `last`, counts as a double-click.
fn is_double_click(last: Option<(Instant, usize, usize)>, now: Instant, screen: usize, idx: usize) -> bool {
    matches!(
        last,
        Some((t, s, i)) if s == screen && i == idx && now.duration_since(t) <= DOUBLE_CLICK_WINDOW
    )
}

/// Whether a keyboard event arriving while `Focus::Warnings` is focused should knock focus off the button.
fn defocuses_warnings(event: &Event) -> bool {
    !matches!(event, Event::Key(Key::Enter))
}

/// Width left for the status line's track-name field after the leading icon and the trailing block.
fn name_field_width(total_w: usize, prefix_w: usize, reserved_w: usize) -> usize {
    total_w.saturating_sub(prefix_w).saturating_sub(reserved_w)
}

/// The status line's scrubber width.
const STATUS_BAR_WIDTH: usize = 24;

/// Click targets on the bottom status line — `(start column, width)` each, in screen columns.
struct StatusLineLayout {
    prev: (usize, usize),
    playpause: (usize, usize),
    next: (usize, usize),
    scrubber: (usize, usize),
    bpm: (usize, usize),
    shuffle: (usize, usize),
}

/// Every segment's already-rendered display width, for [`status_line_layout`].
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

/// Column layout for the status line.
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

fn progress_bar(pos: u32, dur: u32, width: usize) -> String {
    if dur == 0 {
        return "-".repeat(width);
    }
    let filled = ((pos as f64 / dur as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "━".repeat(filled), "╍".repeat(width - filled))
}

/// `▶`/`⏸`/`⏹` for the given playback state.
pub fn player_state_icon(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "▶",
        PlayerState::Paused => "⏸",
        PlayerState::Stopped => "⏹",
    }
}

/// The play/pause *button*'s icon: the action a press would take, not the state it's in.
fn player_action_icon(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "⏸",
        PlayerState::Paused | PlayerState::Stopped => "▶",
    }
}

/// `player_action_icon` with `player_state_glyph`'s leading-space padding.
fn player_action_glyph(state: &PlayerState) -> String {
    format!(" {}", player_action_icon(state))
}

/// `player_state_icon`, with an extra leading space.
pub fn player_state_glyph(state: &PlayerState) -> String {
    format!(" {}", player_state_icon(state))
}

/// Bracketed BPM-scan status tag shown next to the status line's scrubber.
fn bpm_status_tag(s: &Session, track: Option<&core::Track>) -> String {
    let Some(scan) = s.scan.as_ref() else {
        return "[bd]".to_string();
    };
    let mode_letter = match scan.mode() {
        core::ScanMode::Disabled => return "[bd]".to_string(),
        core::ScanMode::CacheOnly => 'b',
        core::ScanMode::Active => 'B',
    };
    // Purely a plugin-status indicator, never the resolved value itself.
    let status_letter = match track.and_then(|t| scan.status("bpm", t.id)) {
        Some(core::ScanStatus::Downloading) => 'd',
        Some(core::ScanStatus::Error) => 'e',
        Some(core::ScanStatus::Skipped) => 's',
        None => 'w',
    };
    format!("[{mode_letter}{status_letter}]")
}

/// Full, un-scrolled track text for the terminal window title.
pub fn window_title_track_text(track: Option<&core::Track>) -> String {
    match track {
        Some(t) => format!("{} - {}", t.display_artist(), t.title),
        None => "medley".to_string(),
    }
}
