//! `MedleyView` — the whole TUI in one snapshot-rendered cursive view.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cursive::{Printer, Rect, Vec2, View};
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle};
use cursive::view::CannotFocus;

use core::{LastPlayed, Layout, LogBuf, PaneLayoutConfig, Session};

use crate::{SessionHandle, keybindings};
use crate::keybindings::Action;
use crate::screen::{Kind, Placement};

use corners::Widget;
use frame::Chrome;
use input::Editing;
use memo::Memo;
use modal::Modal;
use panes::{draw_float_frame, draw_separator, float_body, float_rect, split};
use status_line::{ScanLight, StatusLine};
use tab_bar::{TabBar, TabBarHit, WaveformMemo};
use text::Marquee;
use track_list::TrackList;
use warnings::warnings_label;
use window::{Ctx, StatusCtx, WindowId, WindowOutcome, Windows};

mod clipboard;
mod corners;
mod files;
mod frame;
mod help;
mod hotkeys;
mod input;
mod kind_bar;
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
pub(crate) use scroll::Nav;
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

/// A shown window's own rect and the box it is hit-tested by: the same, but for a floating window's border box.
struct Placed {
    id: WindowId,
    rect: Rect,
    frame: Rect,
}

pub struct MedleyView {
    session: SessionHandle,
    windows: Windows,
    /// The `Tabbed` windows in tab-bar order, never empty; a window moved to `Tabbed` appends.
    tabs: Vec<WindowId>,
    /// The active tab, one of `tabs`.
    active: WindowId,
    /// Whole-terminal size as of the last layout pass.
    last_screen_size: Vec2,
    /// Mirrors `cfg.status_line`: whether the scrubber row is reserved.
    status_line: bool,
    /// Mirrors `cfg.show_hints`: whether windows draw key hints on their status row.
    show_hints: bool,
    editing: Editing,
    buffer: String,
    /// The focused window's status row's one transient message, cleared by the next input event or once `FLASH_LIFETIME` has passed.
    feedback: Option<(String, Instant)>,
    /// Dock side and stacking, shared by every docked window.
    pane_cfg: PaneLayoutConfig,
    /// Open non-tab windows, oldest first — dock order, and z-order — each with the focus it opened over.
    open: Vec<(WindowId, Focus)>,
    /// Which window currently receives nav keys; `Tab` cycles it.
    focus: Focus,
    vis: Arc<crate::vis::Vis>,
    /// The redraw rate raised for a shown Vis window; `None` while none is shown.
    vis_fast: Option<u32>,
    modal: Option<Modal>,
    marquee: Marquee,
    /// What `follow_scan` last reported: window, its list's generation, its `follow_key`.
    follow_sig: Memo<(WindowId, u64, (u64, usize))>,
    chrome: Memo<(u64, u64), Arc<Chrome>>,
    waveform: WaveformMemo,
    help_from: Option<(WindowId, Focus)>,
    /// The last-played track still to be selected, and whether its playlist is open.
    to_select: Option<(LastPlayed, bool)>,
}

impl MedleyView {
    /// `layout` is what `saved_layout` returned last run; without a usable one, the default layout on its first tab.
    pub fn new(session: SessionHandle, log: Arc<LogBuf>, layout: Option<Layout>, last_played: Option<LastPlayed>) -> Self {
        let (pane_cfg, session_status_line, session_show_hints) = {
            let mut s = session.lock().unwrap();
            // `command::parse` reads the item table first, so a plugin word spelled like an item never runs.
            for (word, _) in s.plugin_command_help().into_iter().filter(|(word, _)| crate::items::named(word).is_some()) {
                s.warn("commands", &format!("plugin command :{word} is shadowed by the built-in command of that spelling"));
            }
            (s.cfg.panes, s.cfg.status_line, s.cfg.show_hints)
        };
        let vis = crate::vis::Vis::spawn(session.clone());
        let windows = Windows::new(log, vis.clone(), pane_cfg.mode.into());
        let tabs: Vec<WindowId> = windows.ids().filter(|&id| windows.placement(id) == Placement::Tabbed).collect();
        let active = tabs[0];
        let mut view = Self {
            session,
            windows,
            tabs,
            active,
            focus: Focus::Window(active),
            last_screen_size: Vec2::new(0, 0),
            status_line: session_status_line,
            show_hints: session_show_hints,
            editing: Editing::None,
            buffer: String::new(),
            feedback: None,
            pane_cfg,
            open: Vec::new(),
            vis,
            vis_fast: None,
            modal: None,
            marquee: Marquee::new(),
            follow_sig: Memo::default(),
            chrome: Memo::default(),
            waveform: Memo::default(),
            help_from: None,
            to_select: last_played.map(|last| (last, false)),
        };
        if let Some(layout) = layout {
            view.restore(&layout);
        }
        view
    }

    /// Applies a saved layout, or none of it unless it is one `saved_layout` could have written.
    fn restore(&mut self, layout: &Layout) -> Option<()> {
        let ids = |names: &[String]| names.iter().map(|name| self.windows.named(name)).collect::<Option<Vec<_>>>();
        let (tabs, open) = (ids(&layout.tabs)?, ids(&layout.open)?);
        let active = self.windows.named(&layout.active).filter(|id| tabs.contains(id))?;
        let placed = layout
            .placements
            .iter()
            .map(|(name, word)| {
                let placement = Placement::from_word(word).filter(|&placement| placement != Placement::Tabbed)?;
                Some((self.windows.named(name)?, placement))
            })
            .collect::<Option<Vec<_>>>()?;
        let all: Vec<WindowId> = tabs.iter().copied().chain(placed.iter().map(|&(id, _)| id)).collect();
        let repeats = |ids: &[WindowId]| ids.iter().enumerate().any(|(i, id)| ids[..i].contains(id));
        let open_tab = open.iter().any(|id| tabs.contains(id));
        // A second open fullscreen window would sit unseen under the first.
        let screens = placed.iter().filter(|(id, placement)| *placement == Placement::Screen && open.contains(id)).count();
        let placed_right = |id: WindowId| {
            if self.windows.is_startup_tab(id) {
                tabs.contains(&id)
            } else {
                !self.windows.is_companion(id) || !tabs.contains(&id)
            }
        };
        let fixed = self.windows.ids().all(placed_right);
        if !fixed || repeats(&all) || all.len() != self.windows.ids().count() || repeats(&open) || open_tab || screens > 1 {
            return None;
        }
        for &id in &tabs {
            self.windows.place(id, Placement::Tabbed);
        }
        for (id, placement) in placed {
            self.windows.place(id, placement);
        }
        self.open = open.into_iter().map(|id| (id, Focus::Window(active))).collect();
        (self.tabs, self.active, self.focus) = (tabs, active, Focus::Window(active));
        (self.pane_cfg.side, self.pane_cfg.stack) = (layout.side, layout.stack);
        Some(())
    }

    /// The layout for `state.toml`: what `restore` reads back.
    pub fn saved_layout(&self) -> Layout {
        let names = |ids: &mut dyn Iterator<Item = WindowId>| ids.map(|id| self.windows.name(id).to_string()).collect();
        let placements = self.windows.placements().named().filter(|&(_, placement)| placement != Placement::Tabbed);
        Layout {
            tabs: names(&mut self.tabs.iter().copied()),
            active: self.windows.name(self.active).to_string(),
            open: names(&mut self.open.iter().map(|&(id, _)| id)),
            placements: placements.map(|(name, placement)| (name.to_string(), placement.word().to_string())).collect(),
            side: self.pane_cfg.side,
            stack: self.pane_cfg.stack,
        }
    }

    /// The window the view is built around: the fullscreen one, else the active tab.
    fn main_id(&self) -> WindowId {
        self.fullscreen().unwrap_or(self.active)
    }

    /// Each tab's name in tab order.
    fn tab_names(&self) -> Vec<String> {
        self.tabs.iter().map(|&id| self.windows[id].kind.label().to_string()).collect()
    }

    fn active_tab(&self) -> usize {
        self.tabs.iter().position(|&id| id == self.active).unwrap_or(0)
    }

    fn focused_id(&self) -> WindowId {
        match self.focus {
            Focus::Window(id) => id,
            Focus::Warnings => self.main_id(),
        }
    }

    /// The window whose status row shows what the focused one reports: itself, else the active tab's.
    fn status_id(&self) -> WindowId {
        let focused = self.focused_id();
        if self.windows[focused].shows_status() { focused } else { self.main_id() }
    }

    /// Floating or fullscreen: shown over the view rather than in it.
    fn over_view(&self, id: WindowId) -> bool {
        self.windows.placement(id).over_view()
    }

    /// The newest open fullscreen window, which covers the tab, the docked windows and every fixed row.
    fn fullscreen(&self) -> Option<WindowId> {
        self.open_in(Placement::Screen).last()
    }

    /// Every shown window, bottom first: the fullscreen window, else the active tab and the docked windows; then the floating ones.
    fn placed(&self) -> Vec<Placed> {
        let size = self.last_screen_size;
        let plain = |(id, rect)| Placed { id, rect, frame: rect };
        let (main_rect, docked) = match self.fullscreen() {
            Some(_) => (Rect::from_size((0, 0), size), Vec::new()),
            None => split(size, self.bar_rows(), &self.open_in(Placement::Docked).collect::<Vec<_>>(), self.pane_cfg),
        };
        // A float's cascade slot is its rank by id among the open ones, so raising one moves none.
        let mut slots: Vec<WindowId> = self.open_in(Placement::Floating).collect();
        slots.sort();
        let floating = self.open_in(Placement::Floating).map(|id| {
            let frame = float_rect(size, self.bar_rows(), slots.iter().position(|&slot| slot == id).unwrap_or(0));
            Placed { id, rect: float_body(frame), frame }
        });
        std::iter::once((self.main_id(), main_rect)).chain(docked).map(plain).chain(floating).collect()
    }

    fn bar_rows(&self) -> usize {
        usize::from(self.status_line)
    }

    fn visible(&self) -> Vec<WindowId> {
        self.placed().into_iter().map(|placed| placed.id).collect()
    }

    /// The list selection-based commands act on: the focused window's, else the active tab's.
    fn active_list_id(&self) -> WindowId {
        if self.windows[self.focused_id()].list().is_some() { self.focused_id() } else { self.main_id() }
    }

    fn active_list(&self) -> Option<&TrackList> {
        self.windows[self.active_list_id()].list()
    }

    fn ctx<'a>(&'a self, s: &'a Session) -> Ctx<'a> {
        Ctx { s, pane_cfg: self.pane_cfg, placements: self.windows.placements() }
    }

    /// Offers `event` to each of `ids` under one session lock; the first window not ignoring it, and its outcome.
    fn send(&mut self, ids: &[WindowId], event: &Event) -> Option<(WindowId, WindowOutcome)> {
        let session = self.session.clone();
        let guard = session.lock().unwrap();
        self.windows.send(ids, event, (&guard, self.pane_cfg))
    }

    fn apply(&mut self, outcome: WindowOutcome) -> EventResult {
        match outcome {
            WindowOutcome::Ignored => EventResult::Ignored,
            WindowOutcome::Consumed => EventResult::consumed(),
            WindowOutcome::Run(cmd) => self.run(cmd),
            WindowOutcome::AddFile(path) => {
                let playlist = self.active_list().and_then(TrackList::open_local);
                self.run(core::Command::AddFilesToPlaylist { playlist, paths: vec![path] })
            }
            WindowOutcome::ToggleSetting(row) => {
                self.toggle_setting(row);
                EventResult::consumed()
            }
            WindowOutcome::Bind(target, key) => self.bind_hotkey(target, key),
            WindowOutcome::Unbind(target) => {
                self.clear_hotkey(target)
            }
        }
    }

    /// Hands each visible window its rect, under the one lock a layout pass takes; effects on change run here, never in `draw`.
    fn layout(&mut self) {
        let session = self.session.clone();
        let warn_count = {
            let s = session.lock().unwrap();
            for placed in self.placed() {
                self.windows[placed.id].relayout(placed.rect, &s);
            }
            self.follow_scan(&s);
            s.warning_count()
        };
        self.clamp_focus_given(warn_count);
        self.select_last_played();
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

    /// Every shown window — the tab, the docked, the floats by id — then the warnings button; `warn_count` is the caller's.
    fn focus_order_given(&self, warn_count: usize) -> Vec<Focus> {
        let placed = self.placed();
        let mut shown: Vec<WindowId> = placed.iter().map(|placed| placed.id).collect();
        // Focusing a float raises it, so z-order would make `Tab` skip the one just covered.
        let floats = shown.iter().position(|&id| self.windows.placement(id) == Placement::Floating).unwrap_or(shown.len());
        shown[floats..].sort();
        let mut order: Vec<Focus> = shown.into_iter().map(Focus::Window).collect();
        if Self::status_widget(&self.slots(&placed), warn_count).is_some_and(|widget| widget.warnings.is_some()) {
            order.push(Focus::Warnings);
        }
        order
    }

    fn focus_order(&self) -> Vec<Focus> {
        self.focus_order_given(self.warn_count())
    }

    fn cycle_focus(&mut self, back: bool) {
        let order = self.focus_order();
        let idx = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let step = if back { order.len() - 1 } else { 1 };
        match order[(idx + step) % order.len()] {
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
            Event::Key(Key::Tab) | Event::Shift(Key::Tab) => {
                self.cycle_focus(matches!(event, Event::Shift(_)));
                EventResult::consumed()
            }
            Event::Key(Key::Right) => self.seek(5000),
            Event::Key(Key::Left) => self.seek(-5000),
            &Event::Char(key) => {
                let (sel, collection, open_row, hotkeys) = self.with_session(|s| {
                    let list = self.active_list();
                    let sel = list.and_then(|list| list.selected_track(s));
                    let collection = list.and_then(|list| list.selected_collection(s));
                    (sel, collection, list.and_then(|list| list.open_row(s)), s.hotkeys().into_iter().collect())
                });
                // Per-user playlist hotkeys win over a built-in command when a key names a playlist target.
                match keybindings::hotkey_toggle(key, sel, &hotkeys, open_row) {
                    Some(cmd) => self.run_confirmed(cmd),
                    None => self.handle_action(keybindings::map(key, sel, collection, &hotkeys)),
                }
            }
            _ => EventResult::Ignored,
        }
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
        if let Some(modal) = self.modal.as_ref().filter(|m| m.covers_screen()) {
            self.draw_modal(modal, printer);
            return;
        }
        // One lock for the whole frame: every session-derived value comes out here, then rendering runs without it.
        let placed = self.placed();
        let frame = self.frame(&placed);
        let chrome = &frame.chrome;
        let slots = self.slots(&placed);
        let widget = Self::status_widget(&slots, chrome.warn_count);
        // A fullscreen window covers the tab bar, the scrubber line and the docked windows.
        let covered = self.fullscreen().is_some();
        if !covered && self.open_in(Placement::Docked).next().is_some() {
            draw_separator(self.pane_cfg.side, printer, self.windows[self.main_id()].rect());
        }
        let docked = self.open_in(Placement::Docked).next().is_some();
        for (placed, window_frame) in placed.iter().zip(&frame.windows) {
            let window = &self.windows[placed.id];
            // The active tab's window carries no focus marker.
            let marked = placed.id != self.main_id() && self.focus == Focus::Window(placed.id);
            if self.windows.placement(placed.id) == Placement::Floating {
                draw_float_frame(printer, placed.frame);
            }
            let place = chrome.place_key.filter(|_| self.focus == Focus::Window(placed.id)).map(|key| {
                let target = self.windows.next_placement(placed.id).word();
                format!("[{key}] {target}")
            });
            let status = StatusCtx {
                flash: self.flash().filter(|_| self.status_id() == placed.id),
                place,
                chrome,
                docked,
                hints: self.show_hints,
                reserved: widget.as_ref().filter(|widget| !widget.scrubber && widget.host == placed.id).map_or(0, |widget| widget.rect().width()),
            };
            window.draw(printer, marked, window_frame, self.windows.placement(placed.id), &status);
        }

        let marquee_offset = self.marquee.offset(&frame.status.now_playing);
        if !covered {
            TabBar { tabs: &self.tab_names(), active: self.active_tab(), status: &frame.status, marquee_offset }
                .draw(printer, &self.waveform);
            if self.status_line {
                let y = printer.size.y.saturating_sub(1);
                let width = Widget::status_width(widget.as_ref(), printer.size.x);
                frame.status.draw(&printer.windowed(Rect::from_size((0, y), (width, 1))), marquee_offset);
            }
        }
        if let Some(input) = self.input_line() {
            let rect = self.input_rect(&slots, widget.as_ref());
            printer.windowed(rect).with_color(ColorStyle::primary(), |p| p.print((0, 0), &pad(&input, p.size.x)));
        }
        if let Some(widget) = widget {
            if let Some(rect) = widget.scan {
                let idle = if widget.scrubber { ColorStyle::highlight_inactive() } else { ColorStyle::primary() };
                let fg = Color::Dark(BaseColor::White);
                let tag = &frame.status.bpm_tag;
                let style = match tag.light {
                    ScanLight::Idle => idle,
                    ScanLight::Working => ColorStyle::new(fg, Color::Dark(BaseColor::Green)),
                    ScanLight::Errored => ColorStyle::new(fg, Color::Dark(BaseColor::Red)),
                };
                let p = printer.windowed(rect);
                p.with_color(idle, |p| p.print((0, 0), "[ ]"));
                p.with_color(style, |p| p.print((1, 0), &tag.letter.to_string()));
            }
            if let Some(rect) = widget.warnings {
                let (fg, bg) = (Color::Dark(BaseColor::White), Color::Dark(BaseColor::Red));
                let style = if self.focus == Focus::Warnings { ColorStyle::new(bg, fg) } else { ColorStyle::new(fg, bg) };
                printer.windowed(rect).with_color(style, |p| p.print((0, 0), &warnings_label(chrome.warn_count)));
            }
        }
        if let Some(modal) = &self.modal {
            self.draw_modal(modal, printer);
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
            return self.sync_vis_fps();
        }
        let result = self.route(&event);
        // cursive drains type-ahead before its next layout pass, so what this event changed is laid out right away.
        self.layout();
        // A wheel scroll must stay where it is; any key brings the cursor back into view.
        if !matches!(event, Event::Mouse { .. }) {
            self.clamp_scroll();
        }
        result.and(self.sync_vis_fps())
    }
}

const FLASH_LIFETIME: Duration = Duration::from_secs(3);

impl MedleyView {
    fn set_flash(&mut self, text: String) {
        self.feedback = Some((text, Instant::now()));
    }

    fn flash(&self) -> Option<&str> {
        self.feedback.as_ref().filter(|(_, at)| at.elapsed() < FLASH_LIFETIME).map(|(text, _)| text.as_str())
    }

    fn route(&mut self, event: &Event) -> EventResult {
        // The flash lasts until the next input where it shows: a modal that doesn't show it leaves it, its closing key included.
        let is_mouse_followup =
            matches!(event, Event::Mouse { event: MouseEvent::Release(_) | MouseEvent::Hold(_), .. });
        if !is_mouse_followup && self.modal.is_none() {
            self.feedback = None;
        }

        if let Some(result) = self.on_edit_event(event) {
            return result;
        }
        if self.modal.is_some() {
            return self.on_modal_event(event);
        }
        let fullscreen = self.fullscreen();

        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(*offset)
            && let Some(widget) = self.current_widget()
        {
            if widget.warnings.is_some_and(|rect| rect.contains(local)) {
                self.open_warnings();
                return EventResult::consumed();
            }
            if widget.scan.is_some_and(|rect| rect.contains(local)) {
                return self.run(core::Command::ToggleScan);
            }
        }

        // Fixed rows of the whole screen: the tab bar on top, the status line at the bottom.
        if fullscreen.is_none()
            && let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && let Some(local) = position.checked_sub(*offset)
            && local.x < self.last_screen_size.x
        {
            let size = self.last_screen_size;
            if local.y == 0 {
                let status = self.with_session(StatusLine::snapshot);
                let marquee_offset = self.marquee.offset(&status.now_playing);
                let hit = TabBar { tabs: &self.tab_names(), active: self.active_tab(), status: &status, marquee_offset }
                    .click(local.x, size.x);
                if let Some(TabBarHit::Transport(button)) = hit {
                    return self.handle_action(button.action());
                }
                if let Some(TabBarHit::Like) = hit {
                    return self.handle_action(Action::LikePlaying);
                }
                if let Some(TabBarHit::Seek(cmd)) = hit {
                    return self.run(cmd);
                }
                self.focus = Focus::Window(self.main_id());
                return match hit {
                    Some(TabBarHit::Tab(target)) => self.handle_action(Action::Tab(target)),
                    _ => EventResult::consumed(),
                };
            }
            if self.status_line && local.y == size.y.saturating_sub(1) {
                let status = self.with_session(StatusLine::snapshot);
                let widget = self.current_widget();
                let width = Widget::status_width(widget.as_ref(), size.x);
                return match status.click(local.x, width) {
                    Some(cmd) => self.run(cmd),
                    None => EventResult::consumed(),
                };
            }
        }

        // A mouse event goes to the topmost window whose box it lands in; a press focuses it even when it has no use for it.
        if let Event::Mouse { offset, position, event: mouse } = event {
            let hit = position
                .checked_sub(*offset)
                .and_then(|pos| self.placed().into_iter().rev().find(|placed| placed.frame.contains(pos)));
            let Some(Placed { id, .. }) = hit else { return EventResult::Ignored };
            let outcome = self.send(&[id], event);
            let pressed = matches!(mouse, MouseEvent::Press(_));
            if outcome.is_some() || pressed {
                self.focus_window(id);
            }
            return match outcome {
                Some((_, outcome)) => self.apply(outcome),
                None if pressed => EventResult::consumed(),
                None => EventResult::Ignored,
            };
        }

        // Tab and Shift-Tab cycle on from the button; any other key but Enter moves to the window hosting it.
        if self.focus == Focus::Warnings && !matches!(event, Event::Key(Key::Tab) | Event::Shift(Key::Tab)) {
            if *event == Event::Key(Key::Enter) {
                self.open_warnings();
                return EventResult::consumed();
            }
            let host = self.current_widget().map_or(self.main_id(), |widget| widget.host);
            self.focus_window(host);
        }

        // A key goes to the focused window, then the shell; nothing under a fullscreen window is ever offered one.
        let ids = [self.focused_id(), self.main_id()];
        // Esc a window that is not tabbed has no use for closes it, before the window beneath sees it.
        let help = self.windows[ids[0]].kind == Kind::Help;
        let closing = *event == Event::Key(Key::Esc) && (help || self.windows.placement(ids[0]).closes_on_esc());
        // Only Enter and Esc go on to the main list, and only past a window with no rows of its own to act on.
        let through = matches!(event, Event::Key(Key::Enter | Key::Esc))
            && self.windows[ids[1]].list().is_some()
            && !self.over_view(ids[0])
            && !matches!(self.windows[ids[0]].kind, Kind::List(_) | Kind::Help | Kind::Files);
        let alone = closing || ids[0] == ids[1] || !through;
        // Tab and Shift-Tab are a window's own only over the view; in it they cycle focus.
        let cycles = matches!(event, Event::Key(Key::Tab) | Event::Shift(Key::Tab)) && !self.over_view(ids[0]);
        let sent = if cycles { None } else { self.send(if alone { &ids[..1] } else { &ids }, event) };
        match sent {
            Some((_, outcome)) => self.apply(outcome),
            None if closing => {
                if help {
                    self.toggle_help();
                } else {
                    self.close_window(ids[0]);
                }
                EventResult::consumed()
            }
            // A window shown over the view that is no list owns the keyboard but for the keys about windows themselves.
            None if self.over_view(ids[0]) && self.windows[ids[0]].list().is_none() => {
                let action = match *event {
                    Event::Char(key) => self.with_session(|s| keybindings::map(key, None, None, &s.hotkeys().into_iter().collect())),
                    Event::Key(Key::Tab) | Event::Shift(Key::Tab) => return self.on_shell_key(event),
                    _ => Action::None,
                };
                if action.is_window_action() { self.handle_action(action) } else { EventResult::consumed() }
            }
            None => self.on_shell_key(event),
        }
    }
}
