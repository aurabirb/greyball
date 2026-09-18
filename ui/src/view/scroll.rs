use cursive::Printer;
use cursive::theme::ColorStyle;

/// Cursor stepping shared by every index-into-a-list screen/modal.
pub(super) fn stepped_cursor(cur: usize, len: usize, up: bool, step: usize) -> usize {
    if up {
        cur.saturating_sub(step)
    } else if len == 0 {
        cur
    } else {
        (cur + step).min(len - 1)
    }
}

/// Visible row count for a fullscreen modal list starting at `list_top`, given the whole-screen height.
pub(super) fn modal_list_h(screen_h: usize, list_top: usize) -> usize {
    screen_h.saturating_sub(list_top).saturating_sub(2)
}

/// One screen/modal's cursor + the viewport offset that follows it.
pub(super) struct CursorWindow<'a> {
    pub(super) cursor: &'a mut usize,
    pub(super) offset: &'a mut usize,
}

impl CursorWindow<'_> {
    /// Resync `offset` to `cursor` without moving `cursor` itself.
    pub(super) fn follow(&mut self, view_h: usize) {
        *self.offset = follow_cursor_offset(*self.cursor, *self.offset, view_h);
    }

    /// Move `step` rows up or down through `len` rows of content shown in a `view_h`-row viewport.
    pub(super) fn jump(&mut self, up: bool, step: usize, len: usize, view_h: usize) {
        *self.cursor = stepped_cursor(*self.cursor, len, up, step);
        self.follow(view_h);
    }
}

/// Rows per `PageUp`/`PageDown`/Shift-J/Shift-K press on a raw-scroll-offset pane.
pub(super) const PAGE_SCROLL_STEP: usize = 10;

/// Rows per `PageUp`/`PageDown`/Shift-J/Shift-K jump on any index-into-a-list cursor screen/modal.
pub(super) const LIST_JUMP_STEP: usize = 10;

/// Rows per mouse-wheel tick on a list/pane's single-row nav.
pub(super) const WHEEL_STEP: usize = 3;

/// `clamp_scroll_for`'s cursor-follow arithmetic.
pub(super) fn follow_cursor_offset(cursor: usize, offset: usize, list_h: usize) -> usize {
    if cursor < offset {
        cursor
    } else if list_h > 0 && cursor >= offset + list_h {
        cursor + 1 - list_h
    } else {
        offset
    }
}

/// `clamp_offset_bounds`'s arithmetic.
pub(super) fn bound_offset(offset: usize, len: usize, list_h: usize) -> usize {
    offset.min(len.saturating_sub(list_h))
}

/// Draw a scrollbar thumb in the column at `x = gutter_x` of `printer`, covering rows `1..=list_h`.
pub(super) fn draw_scrollbar(printer: &Printer, gutter_x: usize, list_h: usize, offset: usize, total: usize) {
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
