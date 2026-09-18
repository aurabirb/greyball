//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use cursive::{Printer, Rect, Vec2, View};
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle};
use cursive::view::CannotFocus;

use fuzzy_matcher::skim::SkimMatcherV2;

use unicode_width::UnicodeWidthStr;

use core::{
    BrowseNode, Command, HotkeyTarget, LogBuf, PaneLayoutConfig, PaneMode, PlaylistId, Session, Side,
    SourceId,
};

use crate::{SessionHandle, keybindings};
use crate::command::Pane;
use crate::keybindings::Action;

use filter::FilterCache;
use help::HelpModal;
use hotkeys::HotkeyUi;
use input::{Editing, key_name};
use log::draw_pane;
use panes::{list_screen_for_pane, split};
use playlist_picker::PlaylistPicker;
use playlists::RememberedPlaylist;
use rows::{Row, draw_row_list};
use scroll::{LIST_JUMP_STEP, ListState, PAGE_SCROLL_STEP};
use settings::{draw_settings_pane, settings_entries};
use status_line::StatusLine;
use tab_bar::{TabBar, TabBarHit};
use text::Marquee;
use warnings::{WarningsModal, defocuses_warnings, warnings_label};

mod filter;
mod help;
mod hotkeys;
mod input;
mod lists;
mod log;
mod mouse;
mod panes;
mod playlist_picker;
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
/// Number of screens — sizes `MedleyView::lists`.
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
    /// Each screen's cursor and scroll window; a wheel scroll moves only the window, `clamp_scroll` re-follows.
    lists: [ListState; N_SCREENS],
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
    /// The Settings pane's entry list.
    settings: ListState,
    /// Which pane currently receives nav keys; `Tab` cycles it.
    focus: Focus,
    /// The Vis pane's background worker + last computed frame.
    vis: Arc<crate::vis::Vis>,
    /// `PaneMode::Screen`'s pane, shown fullscreen in place of the normal 3 screens.
    screen_pane: Option<Pane>,
    /// The plugin-warnings modal.
    warnings: Option<WarningsModal>,
    /// (when, screen, row index) of the last left-click on a list row, for double-click detection.
    last_click: Option<(Instant, usize, usize)>,
    hotkeys: HotkeyUi,
    help: Option<HelpModal>,
    playlist_picker: Option<PlaylistPicker>,
    marquee: Marquee,
}

impl MedleyView {
    pub fn new(session: SessionHandle, initial_screen: &str, log: Arc<LogBuf>) -> Self {
        let screen = startup_screen(initial_screen);
        let pane_cfg = session.lock().unwrap().cfg.panes;
        let vis = crate::vis::Vis::spawn(session.clone());
        Self {
            session,
            screen,
            lists: [ListState::default(); N_SCREENS],
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
            settings: ListState::default(),
            focus: Focus::Main,
            vis,
            screen_pane: None,
            warnings: None,
            last_click: None,
            hotkeys: HotkeyUi::default(),
            help: None,
            playlist_picker: None,
            marquee: Marquee::new(),
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
    // `session` is a non-reentrant `Mutex`: always lock via `with_session`, never twice in one statement.
    fn with_session<R>(&self, f: impl FnOnce(&Session) -> R) -> R {
        let guard = self.session.lock().unwrap();
        f(&guard)
    }

    fn with_session_mut<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut guard = self.session.lock().unwrap();
        f(&mut guard)
    }
}
impl View for MedleyView {
    fn draw(&self, printer: &Printer) {
        if let Some(pane) = self.screen_pane {
            self.draw_screen_pane(pane, printer);
            return;
        }
        if let Some(modal) = &self.warnings {
            self.draw_warnings(modal, printer);
            return;
        }
        if self.draw_hotkey_ui(printer) {
            return;
        }
        if let Some(picker) = &self.playlist_picker {
            picker.draw(printer, &self.with_session(|s| s.playlists()));
            return;
        }
        if let Some(help) = &self.help {
            self.draw_help(help, printer);
            return;
        }

        let (main_rect, panes) = split(printer.size, &self.open_panes, self.pane_cfg);

        // Resolved before the list so `rows` is only ever asked for the visible window.
        let list_h = self.list_h();
        let sel = self.lists[self.screen].cursor;
        // Persisted, not recomputed from `sel` — see `list_offset`'s doc.
        let offset = self.lists[self.screen].offset;

        // A docked Queue/History pane needs the same triple the main content does, for its own rect/screen/cursor.
        let list_panes: Vec<(Pane, Rect, usize, usize, usize)> = panes
            .iter()
            .filter_map(|&(pane, rect)| {
                let screen = list_screen_for_pane(pane)?;
                let pane_h = rect.height().saturating_sub(1);
                Some((pane, rect, screen, self.lists[screen].offset, pane_h))
            })
            .collect();

        // One lock for the whole frame: pull every session-derived value out here, then render without the guard.
        let want_settings = panes.iter().any(|(p, _)| *p == Pane::Settings);
        let (
            rows,
            total,
            status,
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
                let settings = if want_settings { settings_entries(s, self.pane_cfg) } else { Vec::new() };
                let warn_count = s.plugin_statuses().iter().filter(|(_, h)| !h.is_ok()).count();
                // Feed the scan walk the visible list every redraw so it's prioritized over store order.
                if let Some(scan) = &s.scan {
                    let view_screen = self.active_screen();
                    let ids = self.visible_track_ids(s, view_screen);
                    let highlighted = self.lists[view_screen].cursor;
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
                    StatusLine::snapshot(s),
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
                    self.lists[screen].offset,
                    self.lists[screen].cursor,
                    *total,
                );
                continue;
            }
            if pane == Pane::Settings {
                draw_settings_pane(&printer.windowed(rect), &settings, self.settings.offset, self.settings.cursor, focused);
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
        let marquee_offset = self.marquee.offset(&status.now_playing);
        TabBar { active: self.screen, state: &status.state }.draw(printer, &status.now_playing, marquee_offset);
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
                .or(self.hotkeys.feedback.clone().map(|m| format!("  {m}")))
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

        let y = printer.size.y.saturating_sub(1);
        status.draw(&printer.windowed(Rect::from_size((0, y), (printer.size.x, 1))), marquee_offset);

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
        if self.warnings.is_some() {
            let statuses = self.with_session(|s| s.plugin_statuses());
            if let Some(modal) = &mut self.warnings {
                modal.relayout(screen_size_changed, constraint, &statuses);
            }
        }
        self.hotkeys.relayout(screen_size_changed, constraint);
        if self.playlist_picker.is_some() {
            let n = self.with_session(|s| s.playlists().len());
            if let Some(picker) = &mut self.playlist_picker {
                picker.relayout(screen_size_changed, constraint, n);
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
        self.relayout_list(self.screen, main_h_changed, list_h);
        for i in 0..self.last_pane_rects.len() {
            let (pane, rect) = self.last_pane_rects[i];
            if let Some(screen) = list_screen_for_pane(pane) {
                let h = rect.height().saturating_sub(1);
                let changed = old_pane_heights.iter().find(|(p, _)| *p == pane).map(|&(_, oh)| oh) != Some(h);
                self.relayout_list(screen, changed, h);
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
            self.hotkeys.feedback = None;
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

        if self.warnings.is_some() {
            return self.on_warnings_event(&event);
        }
        if self.playlist_picker.is_some() {
            return self.on_playlist_picker_event(&event);
        }
        if let Some(result) = self.on_hotkey_ui_event(&event) {
            return result;
        }
        if self.help.is_some() {
            return self.on_help_event(&event);
        }

        // The tab bar lives on the fixed top row of the whole screen, never `last_main_rect`.
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.y == 0
            && local.x < self.last_screen_size.x
        {
            let state = self.with_session(|s| s.player_status().state);
            let hit = TabBar { active: self.screen, state: &state }.click(local.x, self.last_screen_size.x);
            if let Some(TabBarHit::Transport(button)) = hit {
                return self.run(button.command());
            }
            self.focus = Focus::Main;
            return match hit {
                Some(TabBarHit::Tab(target)) => self.handle_action(Action::Screen(target)),
                _ => EventResult::consumed(),
            };
        }

        // The warnings button lives on the fixed bottom-2 row of the whole screen, never `last_main_rect`.
        if self.warn_count() > 0
            && let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.x < self.last_screen_size.x
            && local.y == self.last_screen_size.y.saturating_sub(2)
        {
            self.warnings = Some(WarningsModal::default());
            return EventResult::consumed();
        }

        // The bottom status line's transport cluster, scrubber, and bpm/shuffle tags.
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(offset)
            && local.x < self.last_screen_size.x
            && local.y == self.last_screen_size.y.saturating_sub(1)
        {
            let status = self.with_session(StatusLine::snapshot);
            return match status.click(local.x, self.last_screen_size.x) {
                Some(cmd) => self.run(cmd),
                None => EventResult::consumed(),
            };
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
                let c = &mut self.lists[self.screen].cursor;
                *c = c.saturating_sub(1);
                EventResult::consumed()
            }
            Event::Key(Key::Down) | Event::Char('j') if self.focus == Focus::Main => {
                let s = self.screen;
                self.lists[s].cursor = self.lists[s].cursor.saturating_add(1);
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
                self.lists[PLAYLISTS].cursor = 0;
                self.filter_query = None; // going back — the filtered list no longer applies
                EventResult::consumed()
            }
            // Nav keys past this point only apply while a pane is focused.
            Event::Key(Key::Up) | Event::Char('k') => {
                if let Focus::Pane(pane) = self.focus {
                    match list_screen_for_pane(pane) {
                        Some(screen) => {
                            let c = &mut self.lists[screen].cursor;
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
                self.warnings = Some(WarningsModal::default());
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
                    self.hotkeys.capture = Some(target);
                }
                EventResult::consumed()
            }
            Event::Key(Key::Enter) => {
                let active = self.active_screen();
                let sel = self.with_session(|s| self.selected_track(s, active));
                match keybindings::map("Enter", sel, &self.hotkeys_map()) {
                    Action::PlayFromContext(_) => {
                        self.play_track_at(active, self.lists[active].cursor)
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
