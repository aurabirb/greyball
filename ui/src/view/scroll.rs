use cursive::{Printer, Rect, Vec2};
use cursive::event::{Event, Key, MouseButton, MouseEvent};
use cursive::theme::ColorStyle;

use super::text::pad;

/// Cursor stepping shared by every index-into-a-list screen/modal.
fn stepped_cursor(cur: usize, len: usize, up: bool, step: usize) -> usize {
    if up {
        cur.saturating_sub(step)
    } else if len == 0 {
        cur
    } else {
        (cur + step).min(len - 1)
    }
}

/// A list's selected row plus the first visible row of the window that follows it.
#[derive(Clone, Copy, Default)]
pub(super) struct ListState {
    pub(super) cursor: usize,
    pub(super) offset: usize,
}

/// What an event meant to a `ListState`, for its owner to act on.
pub(super) enum ListEvent {
    Close,
    Activate,
    Clicked,
    Moved,
    Unhandled,
}

impl ListState {
    /// Resync `offset` to `cursor` without moving `cursor` itself.
    pub(super) fn follow(&mut self, view_h: usize) {
        self.offset = follow_cursor_offset(self.cursor, self.offset, view_h);
    }

    /// Move `step` rows up or down through `len` rows of content shown in a `view_h`-row viewport.
    pub(super) fn jump(&mut self, up: bool, step: usize, len: usize, view_h: usize) {
        self.cursor = stepped_cursor(self.cursor, len, up, step);
        self.follow(view_h);
    }

    /// Moves the window only, leaving the cursor where it is.
    pub(super) fn scroll(&mut self, up: bool, step: usize, len: usize, view_h: usize) {
        let offset = if up { self.offset.saturating_sub(step) } else { self.offset + step };
        self.offset = bound_offset(offset, len, view_h);
    }

    /// Layout-pass upkeep: re-follow the cursor after a resize, else only keep the window in range.
    pub(super) fn relayout(&mut self, resized: bool, len: usize, view_h: usize) {
        if resized {
            self.follow(view_h);
        } else {
            self.offset = bound_offset(self.offset, len, view_h);
        }
    }

    /// The row index shown at `pos`, if `pos` is inside `rect` and on a real row.
    fn row_at(&self, pos: Vec2, rect: Rect, len: usize) -> Option<usize> {
        if !rect.contains(pos) {
            return None;
        }
        let idx = self.offset + (pos.y - rect.top());
        (idx < len).then_some(idx)
    }

    /// Draws the visible window of `lines` into `printer`, highlighting the cursor row.
    pub(super) fn draw(&self, printer: &Printer, lines: &[String]) {
        for (i, line) in lines.iter().enumerate().skip(self.offset).take(printer.size.y) {
            let y = i - self.offset;
            let line = pad(line, printer.size.x);
            if i == self.cursor {
                printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
            } else {
                printer.print((0, y), &line);
            }
        }
    }

    /// Nav keys/wheel move the cursor, a left click on a row in `rect` selects it; the rest is reported back.
    pub(super) fn on_event(&mut self, event: &Event, len: usize, rect: Rect) -> ListEvent {
        if let Some(nav) = Nav::of(event) {
            let (up, step) = nav.step(LIST_JUMP_STEP);
            self.jump(up, step, len, rect.height());
            return ListEvent::Moved;
        }
        match event {
            Event::Key(Key::Esc) => ListEvent::Close,
            Event::Key(Key::Enter) => ListEvent::Activate,
            Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } => {
                match position.checked_sub(*offset).and_then(|pos| self.row_at(pos, rect, len)) {
                    Some(idx) => {
                        self.cursor = idx;
                        ListEvent::Clicked
                    }
                    None => ListEvent::Unhandled,
                }
            }
            _ => ListEvent::Unhandled,
        }
    }
}

/// A scroll gesture shared by every list, pane and modal; the `bool` is "up".
pub(crate) enum Nav {
    Line(bool),
    Page(bool),
    Wheel(bool),
}

impl Nav {
    pub(crate) fn of(event: &Event) -> Option<Self> {
        match event {
            Event::Key(Key::Up) | Event::Char('k') => Some(Nav::Line(true)),
            Event::Key(Key::Down) | Event::Char('j') => Some(Nav::Line(false)),
            Event::Key(Key::PageUp) | Event::Char('K') => Some(Nav::Page(true)),
            Event::Key(Key::PageDown) | Event::Char('J') => Some(Nav::Page(false)),
            Event::Mouse { event: MouseEvent::WheelUp, .. } => Some(Nav::Wheel(true)),
            Event::Mouse { event: MouseEvent::WheelDown, .. } => Some(Nav::Wheel(false)),
            _ => None,
        }
    }

    /// `(up, rows)` for this gesture, a page being `page` rows.
    pub(super) fn step(self, page: usize) -> (bool, usize) {
        match self {
            Nav::Line(up) => (up, 1),
            Nav::Page(up) => (up, page),
            Nav::Wheel(up) => (up, WHEEL_STEP),
        }
    }
}

/// Rows per `PageUp`/`PageDown`/Shift-J/Shift-K press on a raw-scroll-offset pane.
pub(super) const PAGE_SCROLL_STEP: usize = 10;

/// Rows per `PageUp`/`PageDown`/Shift-J/Shift-K jump on any index-into-a-list cursor screen/modal.
pub(super) const LIST_JUMP_STEP: usize = 10;

/// Rows per mouse-wheel tick on a list/pane's single-row nav.
pub(super) const WHEEL_STEP: usize = 3;

/// The window offset that keeps `cursor` visible in a `list_h`-row viewport.
fn follow_cursor_offset(cursor: usize, offset: usize, list_h: usize) -> usize {
    if cursor < offset {
        cursor
    } else if list_h > 0 && cursor >= offset + list_h {
        cursor + 1 - list_h
    } else {
        offset
    }
}

/// `offset` kept within the scrollable range of `len` rows.
pub(super) fn bound_offset(offset: usize, len: usize, list_h: usize) -> usize {
    offset.min(len.saturating_sub(list_h))
}

/// Draw a scrollbar thumb in the column at `x = gutter_x` of `printer`, covering rows `0..list_h`; every other gutter cell is blanked.
/// `playing` (a row index) adds a `>` at its proportional gutter row, over the thumb too.
pub(super) fn draw_scrollbar(printer: &Printer, gutter_x: usize, list_h: usize, offset: usize, total: usize, playing: Option<usize>) {
    let thumb = if total > list_h {
        let thumb_len = (list_h * list_h / total).max(1).min(list_h);
        let track = list_h - thumb_len;
        let start = ((offset * track) / (total - list_h)).min(track);
        start..start + thumb_len
    } else {
        0..0
    };
    for y in 0..list_h {
        if thumb.contains(&y) {
            printer.with_color(ColorStyle::highlight(), |p| p.print((gutter_x, y), " "));
        } else {
            printer.print((gutter_x, y), " ");
        }
    }
    if let Some(row) = playing.filter(|_| total > 0 && list_h > 0) {
        let y = if total <= list_h || total == 1 { row } else { row * (list_h - 1) / (total - 1) }.min(list_h - 1);
        let style = if thumb.contains(&y) { ColorStyle::highlight() } else { ColorStyle::secondary() };
        printer.with_color(style, |p| p.print((gutter_x, y), ">"));
    }
}
