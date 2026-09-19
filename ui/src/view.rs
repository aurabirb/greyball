//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use cursive::{Printer, Rect, Vec2, View};
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle};
use cursive::view::CannotFocus;

use unicode_width::UnicodeWidthStr;

use core::{Command, LogBuf, Session};

use crate::{SessionHandle, keybindings};
use crate::command::Pane;
use crate::keybindings::Action;

use filter::LocalFilter;
use frame::{CachedFrame, FrameKey};
use help::HelpModal;
use hotkeys::HotkeyUi;
use input::{Editing, key_name};
use log::LogPane;
use panes::{PaneLayout, list_screen_for_pane};
use playlist_picker::PlaylistPicker;
use playlists::PlaylistNav;
use rows::draw_row_list;
use scroll::{LIST_JUMP_STEP, ListState, PAGE_SCROLL_STEP};
use settings::SettingsPane;
use status_line::StatusLine;
use tab_bar::{TabBar, TabBarHit};
use text::Marquee;
use warnings::{WarningsModal, defocuses_warnings, warnings_label};

mod filter;
mod frame;
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
    /// Whole-terminal size as of the last layout pass.
    last_screen_size: Vec2,
    editing: Editing,
    buffer: String,
    filter: LocalFilter,
    /// Last queue/wedge result.
    queue_feedback: Option<String>,
    playlists: PlaylistNav,
    panes: PaneLayout,
    log: LogPane,
    /// Text of the last committed `Command::Search`, so an empty result list can say "no results for X".
    last_query: Option<String>,
    settings: SettingsPane,
    /// Which pane currently receives nav keys; `Tab` cycles it.
    focus: Focus,
    /// The Vis pane's background worker + last computed frame.
    vis: Arc<crate::vis::Vis>,
    /// The plugin-warnings modal.
    warnings: Option<WarningsModal>,
    /// (when, screen, row index) of the last left-click on a list row, for double-click detection.
    last_click: Option<(Instant, usize, usize)>,
    hotkeys: HotkeyUi,
    help: Option<HelpModal>,
    playlist_picker: Option<PlaylistPicker>,
    marquee: Marquee,
    /// What `follow_scan` last reported to the scan walk — a cache, not view state; see its doc.
    follow_sig: Mutex<Option<lists::FollowKey>>,
    /// Revision-keyed memo of `frame()`'s cached part — see `frame.rs`.
    frame_cache: Mutex<Option<(FrameKey, Arc<CachedFrame>)>>,
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
            last_screen_size: Vec2::new(0, 0),
            editing: Editing::None,
            buffer: String::new(),
            filter: LocalFilter::default(),
            queue_feedback: None,
            playlists: PlaylistNav::default(),
            panes: PaneLayout::new(pane_cfg),
            log: LogPane::new(log),
            last_query: None,
            settings: SettingsPane::default(),
            focus: Focus::Main,
            vis,
            warnings: None,
            last_click: None,
            hotkeys: HotkeyUi::default(),
            help: None,
            playlist_picker: None,
            marquee: Marquee::new(),
            follow_sig: Mutex::new(None),
            frame_cache: Mutex::new(None),
        }
    }

    /// `Main`, then each open pane (in stack order), then the warnings button —
    /// `warn_count` passed in so a layout pass can share one session lock
    /// instead of `focus_order` taking its own (see `required_size`).
    fn focus_order_given(&self, warn_count: usize) -> Vec<Focus> {
        // `panes.open` only ever holds docked panes (see `toggle_pane`).
        let mut order = if self.panes.open.is_empty() {
            vec![Focus::Main]
        } else {
            std::iter::once(Focus::Main)
                .chain(self.panes.open.iter().map(|&p| Focus::Pane(p)))
                .collect()
        };
        if warn_count > 0 {
            order.push(Focus::Warnings);
        }
        order
    }

    fn focus_order(&self) -> Vec<Focus> {
        self.focus_order_given(self.warn_count())
    }

    fn cycle_focus(&mut self) {
        let order = self.focus_order();
        let idx = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = order[(idx + 1) % order.len()];
    }

    fn clamp_focus_given(&mut self, warn_count: usize) {
        if !self.focus_order_given(warn_count).contains(&self.focus) {
            self.focus = Focus::Main;
        }
    }

    fn clamp_focus(&mut self) {
        self.clamp_focus_given(self.warn_count());
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
        if let Some(pane) = self.panes.fullscreen {
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
            picker.draw(printer);
            return;
        }
        if let Some(help) = &self.help {
            self.draw_help(help, printer);
            return;
        }

        let (main_rect, panes) = self.panes.split(printer.size);

        // Resolved before the list so `rows` is only ever asked for the visible window.
        let list_h = self.list_h();
        let sel = self.lists[self.screen].cursor;
        // Persisted, not recomputed from `sel` — see `lists`.
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
        let frame = self.frame(list_h, offset, &list_panes, want_settings);
        let cached = &frame.cached;

        let mut list_pane_frames = cached.panes.iter();
        for &(pane, rect) in &panes {
            let focused = self.focus == Focus::Pane(pane);
            if pane == Pane::Vis {
                self.vis.draw(&printer.windowed(rect), focused);
                continue;
            }
            if let Some(screen) = list_screen_for_pane(pane) {
                // Built from this same `panes` list filtered the same way — always in lockstep.
                let pf = list_pane_frames.next().expect("a list pane always has a PaneFrame");
                // `[...]` is the focus marker every pane title uses.
                let title = if focused { format!("[{}]", pf.title) } else { pf.title.clone() };
                draw_row_list(
                    &printer.windowed(rect),
                    &title,
                    &pf.rows,
                    self.lists[screen].offset,
                    self.lists[screen].cursor,
                    pf.total,
                );
                continue;
            }
            if pane == Pane::Settings {
                self.settings.draw(&printer.windowed(rect), &cached.settings, focused);
                continue;
            }
            self.log.draw(&printer.windowed(rect), focused);
        }
        if !panes.is_empty() {
            self.panes.draw_separator(printer, main_rect);
        }

        // Row 0 of the whole screen.
        let marquee_offset = self.marquee.offset(&frame.status.now_playing);
        TabBar { active: self.screen, state: &frame.status.state }
            .draw(printer, &frame.status.now_playing, marquee_offset);
        draw_row_list(&printer.windowed(main_rect), &cached.main_title, &cached.rows, offset, sel, cached.total);

        // command / hint line (row above the status line).
        let bottom = printer.size.y.saturating_sub(2);
        let line = self.hint_line(cached.membership_feedback.clone(), cached.hotkey_target_selected, cached.help_key);
        printer.print((0, bottom), &pad(&line, printer.size.x));

        // Cursor position in the main list / its length, right-aligned before the warnings button.
        let warn_w = if cached.warn_count > 0 {
            warnings_label(cached.warn_count).chars().count().min(printer.size.x)
        } else {
            0
        };
        if cached.total > 0 {
            let more = if cached.list_loading { "+" } else { "" };
            let unit = self.row_unit(self.screen, cached.total);
            let readout = format!("{}/{}{more} {unit}", sel.min(cached.total - 1) + 1, cached.total);
            let x = printer.size.x.saturating_sub(warn_w + readout.width() + 1);
            if x >= line.width() + 2 {
                printer.print((x, bottom), &readout);
            }
        }

        let y = printer.size.y.saturating_sub(1);
        frame.status.draw(&printer.windowed(Rect::from_size((0, y), (printer.size.x, 1))), marquee_offset);

        // Warnings button — right-aligned on the hint line, drawn last so it overwrites that tail.
        if cached.warn_count > 0 {
            let label = warnings_label(cached.warn_count);
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
        let screen_size_changed = constraint != self.last_screen_size;
        self.last_screen_size = constraint;
        let (main_h_changed, pane_bodies) = self.panes.relayout(constraint);
        let list_h = self.list_h();
        let list_screens: Vec<usize> =
            pane_bodies.iter().filter_map(|&(pane, _, _)| list_screen_for_pane(pane)).collect();
        let want_statuses = self.warnings.is_some();

        // One shared lock for everything this layout pass needs from the
        // session, instead of `clamp_focus`/each `relayout_list` taking their own.
        let (warn_count, statuses, main_len, pane_lens, revision) = self.with_session(|s| {
            let warn_count = s.plugin_warning_count();
            let statuses = want_statuses.then(|| s.plugin_statuses().to_vec());
            let main_len = self.list_len(s, self.screen);
            let pane_lens: Vec<usize> = list_screens.iter().map(|&scr| self.list_len(s, scr)).collect();
            (warn_count, statuses, main_len, pane_lens, s.revision())
        });

        self.clamp_focus_given(warn_count);
        self.refresh_help(revision);
        self.refresh_playlist_picker(revision);
        if let (Some(modal), Some(statuses)) = (&mut self.warnings, statuses) {
            modal.relayout(screen_size_changed, constraint, &statuses);
        }
        self.hotkeys.relayout(screen_size_changed, constraint);
        if let Some(picker) = &mut self.playlist_picker {
            picker.relayout(screen_size_changed, constraint);
        }
        self.lists[self.screen].relayout(main_h_changed, main_len, list_h);
        let mut pane_lens = pane_lens.into_iter();
        for (pane, h, changed) in pane_bodies {
            if let Some(screen) = list_screen_for_pane(pane) {
                let len = pane_lens.next().expect("one length per list pane, built from the same list");
                self.lists[screen].relayout(changed, len, h);
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
            self.with_session_mut(|s| s.clear_membership_feedback());
        }

        if let Some(result) = self.on_edit_event(&event) {
            return result;
        }
        if let Some(pane) = self.panes.fullscreen {
            return self.on_fullscreen_pane_event(pane, &event);
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
            self.open_warnings();
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
            for (pane, rect) in self.panes.rects.clone() {
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
        let len = self.with_session(|s| self.list_len(s, self.screen));

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
                    && !self.playlists.at_top_level() =>
            {
                self.playlists.back_out();
                self.lists[PLAYLISTS].cursor = 0;
                self.filter.query = None; // going back — the filtered list no longer applies
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
                    self.open_hotkey_capture(target);
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
