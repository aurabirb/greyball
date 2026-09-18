use std::sync::Arc;

use cursive::Printer;
use cursive::theme::ColorStyle;

use core::LogBuf;

use crate::command::Pane;

use super::panes::pane_title;
use super::scroll::bound_offset;
use super::text::{pad, wrap};

/// The Log pane: `app`'s shared log buffer viewed from its tail, plus how far back the view is scrolled.
pub(super) struct LogPane {
    buf: Arc<LogBuf>,
    /// Wrapped rows scrolled up from the live tail; 0 follows the tail.
    scroll: usize,
    /// Buffer length when the tail was left, so lines arriving afterwards queue up out of view.
    pin: Option<usize>,
}

impl LogPane {
    pub(super) fn new(buf: Arc<LogBuf>) -> Self {
        Self { buf, scroll: 0, pin: None }
    }

    /// The lines in view: everything while following the tail, else only what existed when it was left.
    fn lines(&self) -> Vec<String> {
        let snapshot = self.buf.snapshot();
        let len = if self.scroll == 0 { snapshot.len() } else { self.pin.unwrap_or(snapshot.len()).min(snapshot.len()) };
        snapshot[..len].to_vec()
    }

    /// Scrolls `step` wrapped rows; `dims` is the content area's `(width, rows)`, `None` while not laid out.
    pub(super) fn scroll_by(&mut self, up: bool, step: usize, dims: Option<(usize, usize)>) {
        self.scroll = if up { self.scroll + step } else { self.scroll.saturating_sub(step) };
        if self.scroll > 0 && self.pin.is_none() {
            self.pin = Some(self.buf.snapshot().len());
        }
        if let Some((width, h)) = dims {
            let wrapped_len: usize = self.lines().iter().map(|l| wrap(l, width).len()).sum();
            self.scroll = bound_offset(self.scroll, wrapped_len, h);
        }
        if self.scroll == 0 {
            self.pin = None;
        }
    }

    /// Title row plus the wrapped lines, newest at the bottom.
    pub(super) fn draw(&self, printer: &Printer, focused: bool) {
        let mut title = pane_title(Pane::Log).to_string();
        if self.scroll > 0 {
            title.push_str(" (scrolled, PgDn to catch up)");
        }
        if focused {
            title = format!("[{title}]");
        }
        printer.with_color(ColorStyle::title_secondary(), |p| {
            p.print((0, 0), &pad(&title, p.size.x));
        });
        // Each wrapped line counts as a row, so a long line takes the space it needs.
        let wrapped: Vec<String> = self.lines().iter().flat_map(|l| wrap(l, printer.size.x)).collect();
        let h = printer.size.y.saturating_sub(1);
        // Clamp to the top-most full window, so scrolling past the oldest line freezes there.
        let end = wrapped.len() - self.scroll.min(wrapped.len().saturating_sub(h));
        for (i, line) in wrapped[end.saturating_sub(h)..end].iter().enumerate() {
            printer.print((0, i + 1), line);
        }
    }
}
