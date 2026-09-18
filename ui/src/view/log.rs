use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cursive::Printer;
use cursive::theme::ColorStyle;

use core::LogBuf;

use crate::command::Pane;

use super::panes::pane_title;
use super::scroll::bound_offset;
use super::text::{pad, wrap};

/// Per-line wrapped-row counts for the current width, kept in step with `LogBuf`'s own eviction
/// instead of re-wrapping the whole log on every draw/scroll.
struct WrapCache {
    /// Wrapped row count per buffered line, oldest-first, aligned with `LogBuf`'s current contents.
    counts: VecDeque<usize>,
    /// `LogBuf::total_pushed` this cache reflects.
    pushed: usize,
    /// Width the counts were computed for; `None` forces a resync.
    width: Option<usize>,
}

/// The Log pane: `app`'s shared log buffer viewed from its tail, plus how far back the view is scrolled.
pub(super) struct LogPane {
    buf: Arc<LogBuf>,
    /// Wrapped rows scrolled up from the live tail; 0 follows the tail.
    scroll: usize,
    /// Buffer length when the tail was left, so lines arriving afterwards queue up out of view.
    pin: Option<usize>,
    cache: Mutex<WrapCache>,
}

impl LogPane {
    pub(super) fn new(buf: Arc<LogBuf>) -> Self {
        Self { buf, scroll: 0, pin: None, cache: Mutex::new(WrapCache { counts: VecDeque::new(), pushed: 0, width: None }) }
    }

    /// How many of the buffer's `len` current lines are in view: all of them while
    /// following the tail, else only what existed when the view was left (pin semantics).
    fn visible_len(&self, len: usize) -> usize {
        if self.scroll == 0 { len } else { self.pin.unwrap_or(len).min(len) }
    }

    /// Brings the wrapped-row cache up to date for `width` (appending counts for newly
    /// pushed lines and dropping evicted ones, or fully rebuilding on a width change),
    /// and returns the buffer length it synced against.
    fn sync(&self, width: usize) -> usize {
        let mut cache = self.cache.lock().unwrap();
        let pushed = self.buf.total_pushed();
        let len = self.buf.len();
        let delta = pushed.saturating_sub(cache.pushed);
        if cache.width != Some(width) || delta >= len {
            cache.counts = self.buf.snapshot().iter().map(|l| wrap(l, width).len()).collect();
        } else if delta > 0 {
            for line in self.buf.tail(delta) {
                cache.counts.push_back(wrap(&line, width).len());
            }
            while cache.counts.len() > len {
                cache.counts.pop_front();
            }
        }
        cache.pushed = pushed;
        cache.width = Some(width);
        len
    }

    /// The (line index, sub-row) for wrapped row `row`, counting from the front of `counts`.
    fn locate(counts: impl Iterator<Item = usize>, mut row: usize) -> (usize, usize) {
        for (i, c) in counts.enumerate() {
            if row < c {
                return (i, row);
            }
            row -= c;
        }
        (0, 0)
    }

    /// Scrolls `step` wrapped rows; `dims` is the content area's `(width, rows)`, `None` while not laid out.
    pub(super) fn scroll_by(&mut self, up: bool, step: usize, dims: Option<(usize, usize)>) {
        self.scroll = if up { self.scroll + step } else { self.scroll.saturating_sub(step) };
        let len = self.buf.len();
        if self.scroll > 0 && self.pin.is_none() {
            self.pin = Some(len);
        }
        if let Some((width, h)) = dims {
            let len = self.sync(width);
            let wrapped_len: usize = self.cache.lock().unwrap().counts.iter().take(self.visible_len(len)).sum();
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

        let width = printer.size.x;
        let visible_len = self.visible_len(self.sync(width));
        let h = printer.size.y.saturating_sub(1);

        let total: usize = self.cache.lock().unwrap().counts.iter().take(visible_len).sum();
        // Clamp to the top-most full window, so scrolling past the oldest line freezes there.
        let end = total.saturating_sub(self.scroll.min(total.saturating_sub(h)));
        let start = end.saturating_sub(h);
        if end == start {
            return;
        }

        let (line_lo, sub_lo) = Self::locate(self.cache.lock().unwrap().counts.iter().take(visible_len).copied(), start);
        let mut skip = sub_lo;
        let mut row_y = 1;
        let mut emitted = 0;
        // Wrap only the lines the visible window actually needs, not the whole buffer.
        for line in self.buf.range(line_lo, visible_len) {
            for w in wrap(&line, width).into_iter().skip(skip) {
                if emitted == end - start {
                    return;
                }
                printer.print((0, row_y), &w);
                row_y += 1;
                emitted += 1;
            }
            skip = 0;
        }
    }
}
