//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.

use std::sync::Arc;

use cursive::{Printer, Rect, Vec2, View};
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle};
use cursive::view::CannotFocus;

use unicode_width::UnicodeWidthStr;

use core::{Command, HotkeyTarget, LogBuf, PaneLayoutConfig, Session};

use crate::{SessionHandle, keybindings};
use crate::keybindings::Action;
use crate::screen::Screen;

use frame::Chrome;
use input::{Editing, key_name};
use memo::Memo;
use modal::{Modal, modal_body};
use panes::{draw_float_frame, draw_separator, float_body, float_rect, split};
use status_line::StatusLine;
use tab_bar::{TabBar, TabBarHit};
use text::Marquee;
use track_list::TrackList;
use warnings::{defocuses_warnings, warnings_label};
use window::{Ctx, Placement, WindowFrame, WindowId, WindowOutcome, Windows};

mod frame;
mod help;
mod hotkeys;
mod input;
mod log;
mod memo;
mod modal;
mod notice;
mod panes;
mod playlist_picker;
mod rows;
mod scroll;
mod settings;
mod status_line;
mod tab_bar;
mod text;
mod track_list;
mod transport;
mod warnings;
mod window;

pub(crate) use notice::Notice;
pub(crate) use text::pad;
pub use text::{SCROLL_GAP, marquee_offset, scroll_title};
pub use status_line::window_title_track_text;
pub use transport::player_state_glyph;

/// What navigation keys (arrows/j-k/PgUp/PgDn) go to.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Window(WindowId),
    /// The bottom-row warnings button — `Enter` opens the warnings modal.
    Warnings,
}

pub struct MedleyView {
    session: SessionHandle,
    /// The active tab.
    screen: Screen,
    windows: Windows,
    /// Whole-terminal size as of the last layout pass.
    last_screen_size: Vec2,
    editing: Editing,
    buffer: String,
    /// The hint row's one transient message, cleared by the next input event.
    feedback: Option<String>,
    /// Dock side and stacking, shared by every docked window.
    pane_cfg: PaneLayoutConfig,
    /// Open non-tab windows, oldest first — dock order, and z-order — each with the focus it opened over.
    open: Vec<(WindowId, Focus)>,
    /// Which window currently receives nav keys; `Tab` cycles it.
    focus: Focus,
    vis: Arc<crate::vis::Vis>,
    modal: Option<Modal>,
    marquee: Marquee,
    /// What `follow_scan` last reported: window, its list's generation, its `follow_key`.
    follow_sig: Memo<(WindowId, u64, (u64, usize))>,
    chrome: Memo<u64, Arc<Chrome>>,
}

impl MedleyView {
    pub fn new(session: SessionHandle, initial_screen: &str, log: Arc<LogBuf>) -> Self {
        let pane_cfg = session.lock().unwrap().cfg.panes;
        let vis = crate::vis::Vis::spawn(session.clone());
        let screen = Screen::from_config(initial_screen);
        let windows = Windows::new(log, vis.clone(), pane_cfg.mode);
        Self {
            session,
            screen,
            focus: Focus::Window(windows.tab(screen)),
            windows,
            last_screen_size: Vec2::new(0, 0),
            editing: Editing::None,
            buffer: String::new(),
            feedback: None,
            pane_cfg,
            open: Vec::new(),
            vis,
            modal: None,
            marquee: Marquee::new(),
            follow_sig: Memo::default(),
            chrome: Memo::default(),
        }
    }

    fn main_id(&self) -> WindowId {
        self.windows.tab(self.screen)
    }

    fn focused_id(&self) -> WindowId {
        match self.focus {
            Focus::Window(id) => id,
            Focus::Warnings => self.main_id(),
        }
    }

    /// The newest open fullscreen window, shown alone.
    fn fullscreen(&self) -> Option<WindowId> {
        self.open_in(Placement::Screen).last()
    }

    /// Every shown window and its rect, bottom first: the active tab, the docked windows, the floating ones.
    fn placed(&self) -> Vec<(WindowId, Rect)> {
        let size = self.last_screen_size;
        if let Some(id) = self.fullscreen() {
            return vec![(id, modal_body(Rect::from_size((0, 0), size), false))];
        }
        let docked: Vec<WindowId> = self.open_in(Placement::Docked).collect();
        let (main_rect, docked) = split(size, &docked, self.pane_cfg);
        let floating = self.open_in(Placement::Floating).map(|id| (id, float_body(float_rect(size))));
        std::iter::once((self.main_id(), main_rect)).chain(docked).chain(floating).collect()
    }

    fn visible(&self) -> Vec<WindowId> {
        self.placed().into_iter().map(|(id, _)| id).collect()
    }

    /// The list selection-based commands act on: the focused window's, else the active tab's.
    fn active_list_id(&self) -> WindowId {
        if self.windows[self.focused_id()].list().is_some() { self.focused_id() } else { self.main_id() }
    }

    fn active_list(&self) -> Option<&TrackList> {
        self.windows[self.active_list_id()].list()
    }

    fn ctx<'a>(&self, s: &'a Session) -> Ctx<'a> {
        Ctx { s, pane_cfg: self.pane_cfg, searching: self.editing == Editing::Search }
    }

    /// Offers `event` to each of `ids` under one session lock; the first window not ignoring it, and its outcome.
    fn send(&mut self, ids: &[WindowId], event: &Event) -> Option<(WindowId, WindowOutcome)> {
        let (session, pane_cfg, searching) = (self.session.clone(), self.pane_cfg, self.editing == Editing::Search);
        let guard = session.lock().unwrap();
        let ctx = Ctx { s: &guard, pane_cfg, searching };
        ids.iter().find_map(|&id| match self.windows[id].on_event(event, &ctx) {
            WindowOutcome::Ignored => None,
            outcome => Some((id, outcome)),
        })
    }

    fn apply(&mut self, outcome: WindowOutcome) -> EventResult {
        match outcome {
            WindowOutcome::Ignored => EventResult::Ignored,
            WindowOutcome::Consumed => EventResult::consumed(),
            WindowOutcome::Run(cmd) => self.run(cmd),
            WindowOutcome::ToggleSetting(row) => {
                self.toggle_setting(row);
                EventResult::consumed()
            }
        }
    }

    /// Hands each visible window its rect, under the one lock a layout pass takes; effects on change run here, never in `draw`.
    fn layout(&mut self) {
        let session = self.session.clone();
        let warn_count = {
            let s = session.lock().unwrap();
            for (id, rect) in self.placed() {
                self.windows[id].relayout(rect, &s);
            }
            self.follow_scan(&s);
            s.plugin_warning_count()
        };
        self.clamp_focus_given(warn_count);
    }

    /// Keep the active tab's and the focused window's scroll windows around their cursors.
    fn clamp_scroll(&mut self) {
        for id in [self.main_id(), self.focused_id()] {
            self.windows[id].follow();
        }
    }

    /// Feeds the scan walk the active list, re-reporting only when the list or the cursor in it changed.
    fn follow_scan(&self, s: &Session) {
        let (Some(scan), id) = (&s.scan, self.active_list_id()) else { return };
        let Some(list) = self.windows[id].list() else { return };
        if self.follow_sig.changed((id, list.list_gen(s), list.follow_key())) {
            scan.follow_view(list.visible_track_ids(s), list.cursor());
        }
    }

    /// Every shown window, bottom first, then the warnings button; `warn_count` comes from the caller's lock.
    fn focus_order_given(&self, warn_count: usize) -> Vec<Focus> {
        let mut order: Vec<Focus> = self.visible().into_iter().map(Focus::Window).collect();
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
        match order[(idx + 1) % order.len()] {
            Focus::Window(id) => self.focus_window(id),
            Focus::Warnings => self.focus = Focus::Warnings,
        }
    }

    fn clamp_focus_given(&mut self, warn_count: usize) {
        if !self.focus_order_given(warn_count).contains(&self.focus) {
            self.focus = Focus::Window(self.main_id());
        }
    }

    /// Keys no window took.
    fn on_shell_key(&mut self, event: &Event) -> EventResult {
        match event {
            Event::Key(Key::Tab) => {
                self.cycle_focus();
                EventResult::consumed()
            }
            Event::Key(Key::Right) => self.run(Command::Seek(5000)),
            Event::Key(Key::Left) => self.run(Command::Seek(-5000)),
            Event::Char(':') => self.handle_action(Action::CommandLine),
            Event::Char('x') => match self.with_session(|s| self.hotkey_target(s)) {
                Some(HotkeyTarget::Local(id)) => self.run(Command::ExportM3u(id)),
                _ => EventResult::Ignored,
            },
            ev => {
                let Some(key) = key_name(ev) else { return EventResult::Ignored };
                let (sel, target, hotkeys) = self.with_session(|s| {
                    let sel = self.active_list().and_then(|list| list.selected_track(s));
                    (sel, self.hotkey_target(s), s.hotkeys().into_iter().collect())
                });
                // With a playlist selected or open, backtick binds that playlist instead of opening the menu.
                if let ("`", Some(target)) = (key.as_str(), target) {
                    self.open_hotkey_capture(target);
                    return EventResult::consumed();
                }
                // Per-user playlist hotkeys win over a built-in command when a key names a playlist target.
                match keybindings::hotkey_toggle(&key, sel, &hotkeys) {
                    Some(cmd) => self.run(cmd),
                    None => self.handle_action(keybindings::map(&key, sel, &hotkeys)),
                }
            }
        }
    }

    /// The playlist the focused window, else the active tab's, has selected or open.
    fn hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        [self.focused_id(), self.main_id()].iter().find_map(|&id| self.windows[id].list()?.selected_hotkey_target(s))
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
        if let Some(modal) = &self.modal {
            self.draw_modal(modal, printer);
            return;
        }
        if let Some(id) = self.fullscreen() {
            self.draw_fullscreen(id, printer);
            return;
        }

        // One lock for the whole frame: every session-derived value comes out here, then rendering runs without it.
        let visible = self.visible();
        let frame = self.frame(&visible);
        if self.open_in(Placement::Docked).next().is_some() {
            draw_separator(self.pane_cfg.side, printer, self.windows[self.main_id()].rect());
        }
        for (&id, window_frame) in visible.iter().zip(&frame.windows) {
            let window = &self.windows[id];
            // The active tab's window carries no focus marker.
            let marked = id != self.main_id() && self.focus == Focus::Window(id);
            if window.placement == Placement::Floating {
                draw_float_frame(printer, float_rect(self.last_screen_size), marked);
            }
            window.draw(printer, marked, window_frame);
        }

        let chrome = &frame.chrome;
        let marquee_offset = self.marquee.offset(&frame.status.now_playing);
        TabBar { active: self.screen, state: &frame.status.state }
            .draw(printer, &frame.status.now_playing, marquee_offset);

        // command / hint line (row above the status line).
        let main = match &frame.windows[0] {
            WindowFrame::List(list) => Some(list),
            _ => None,
        };
        let bottom = printer.size.y.saturating_sub(2);
        let hotkey_target = main.is_some_and(|list| list.hotkey_target);
        let line = self.hint_line(hotkey_target, chrome.help_key);
        printer.print((0, bottom), &pad(&line, printer.size.x));

        // Cursor position in the main list / its length, right-aligned before the warnings button.
        let warn_w = if chrome.warn_count > 0 {
            warnings_label(chrome.warn_count).chars().count().min(printer.size.x)
        } else {
            0
        };
        let cursor = self.windows[self.main_id()].list().map_or(0, TrackList::cursor);
        if let Some(list) = main.filter(|list| list.total > 0) {
            let more = if list.loading { "+" } else { "" };
            let readout = format!("{}/{}{more} {}", cursor.min(list.total - 1) + 1, list.total, list.unit);
            let x = printer.size.x.saturating_sub(warn_w + readout.width() + 1);
            if x >= line.width() + 2 {
                printer.print((x, bottom), &readout);
            }
        }

        let y = printer.size.y.saturating_sub(1);
        frame.status.draw(&printer.windowed(Rect::from_size((0, y), (printer.size.x, 1))), marquee_offset);

        // Warnings button — right-aligned on the hint line, drawn last so it overwrites that tail.
        if chrome.warn_count > 0 {
            let label = warnings_label(chrome.warn_count);
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
        let resized = constraint != self.last_screen_size;
        self.last_screen_size = constraint;
        self.layout();
        self.relayout_modal(resized);
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
        let result = self.route(&event);
        // cursive drains type-ahead before its next layout pass, so what this event changed is laid out right away.
        self.layout();
        // A wheel scroll must stay where it is; any key brings the cursor back into view.
        if !matches!(event, Event::Mouse { .. }) {
            self.clamp_scroll();
        }
        result
    }
}

impl MedleyView {
    fn route(&mut self, event: &Event) -> EventResult {
        // Transient queue/wedge feedback shows for one keypress.
        let is_mouse_followup =
            matches!(event, Event::Mouse { event: MouseEvent::Release(_) | MouseEvent::Hold(_), .. });
        if !is_mouse_followup {
            self.feedback = None;
        }

        if let Some(result) = self.on_edit_event(event) {
            return result;
        }
        if self.modal.is_some() {
            return self.on_modal_event(event);
        }
        if let Some(id) = self.fullscreen() {
            return self.on_fullscreen_event(id, event);
        }

        // Fixed rows of the whole screen: the tab bar on top, the hint row and the status line at the bottom.
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(*offset)
            && local.x < self.last_screen_size.x
        {
            let size = self.last_screen_size;
            if local.y == 0 {
                let state = self.with_session(|s| s.player_status().state);
                let hit = TabBar { active: self.screen, state: &state }.click(local.x, size.x);
                if let Some(TabBarHit::Transport(button)) = hit {
                    return self.run(button.command());
                }
                self.focus = Focus::Window(self.main_id());
                return match hit {
                    Some(TabBarHit::Tab(target)) => self.handle_action(Action::Screen(target)),
                    _ => EventResult::consumed(),
                };
            }
            if local.y == size.y.saturating_sub(2) && self.warn_count() > 0 {
                self.open_warnings();
                return EventResult::consumed();
            }
            if local.y == size.y.saturating_sub(1) {
                let status = self.with_session(StatusLine::snapshot);
                return match status.click(local.x, size.x) {
                    Some(cmd) => self.run(cmd),
                    None => EventResult::consumed(),
                };
            }
        }

        // A mouse event goes to the topmost window it lands in, border included, which takes focus; wheel scrolls stay put.
        if let Event::Mouse { offset, position, .. } = event {
            let hit = position.checked_sub(*offset).and_then(|pos| {
                self.visible().into_iter().rev().find(|&id| {
                    let window = &self.windows[id];
                    let floating = window.placement == Placement::Floating;
                    if floating { float_rect(self.last_screen_size) } else { window.rect() }.contains(pos)
                })
            });
            let Some((id, outcome)) = hit.and_then(|id| self.send(&[id], event)) else { return EventResult::Ignored };
            self.focus_window(id);
            return self.apply(outcome);
        }

        if self.focus == Focus::Warnings {
            if !defocuses_warnings(event) {
                self.open_warnings();
                return EventResult::consumed();
            }
            self.focus = Focus::Window(self.main_id());
        }

        // A key goes to the focused window, then the active tab's, then the shell.
        let ids = [self.focused_id(), self.main_id()];
        // Esc the focused floating window has no use for closes it, before the tab beneath sees it.
        let closing = *event == Event::Key(Key::Esc) && self.windows[ids[0]].placement == Placement::Floating;
        match self.send(if ids[0] == ids[1] || closing { &ids[..1] } else { &ids }, event) {
            Some((_, outcome)) => self.apply(outcome),
            None if closing => {
                self.close_window(ids[0]);
                self.vis_fps_cb()
            }
            None => self.on_shell_key(event),
        }
    }
}
