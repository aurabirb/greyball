use std::time::{Duration, Instant};

use cursive::{Rect, Vec2};
use cursive::event::{EventResult, MouseButton, MouseEvent};

use crate::command::Pane;
use crate::keybindings::{self, Action};

use super::{Focus, MedleyView};
use super::panes::list_screen_for_pane;
use super::rows::LIST_TITLE_ROWS;
use super::scroll::WHEEL_STEP;

/// Two clicks on the same row within this long count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Whether a click on (`screen`, `idx`) at `now`, given the previous click `last`, counts as a double-click.
fn is_double_click(last: Option<(Instant, usize, usize)>, now: Instant, screen: usize, idx: usize) -> bool {
    matches!(
        last,
        Some((t, s, i)) if s == screen && i == idx && now.duration_since(t) <= DOUBLE_CLICK_WINDOW
    )
}

impl MedleyView {
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

    /// Mouse handling for the main list — kept entirely separate from the keyboard path in `on_event`.
    pub(super) fn handle_mouse(&mut self, offset: Vec2, position: Vec2, event: MouseEvent) -> Option<EventResult> {
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
    pub(super) fn handle_pane_mouse(
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
}
