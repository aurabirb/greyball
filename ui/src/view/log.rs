use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cursive::Printer;
use cursive::theme::ColorStyle;

use core::LogBuf;

use crate::command::Pane;

use super::panes::pane_title;
use super::scroll::bound_offset;
use super::text::{pad, wrap};

/// Wrapped rows per buffered line, kept in step with `LogBuf`'s own eviction instead of re-wrapping the whole log every draw/scroll.
struct WrapCache {
    /// Wrapped sub-rows per buffered line, oldest-first, aligned with `LogBuf`'s current contents.
    rows: VecDeque<Vec<String>>,
    /// `LogBuf`'s pushed counter this cache reflects.
    pushed: usize,
    /// Width the rows were wrapped for; `None` forces a resync.
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
        Self { buf, scroll: 0, pin: None, cache: Mutex::new(WrapCache { rows: VecDeque::new(), pushed: 0, width: None }) }
    }

    /// How many of the buffer's `len` current lines are in view: all of them while
    /// following the tail, else only what existed when the view was left (pin semantics).
    fn visible_len(&self, len: usize) -> usize {
        if self.scroll == 0 { len } else { self.pin.unwrap_or(len).min(len) }
    }

    /// Brings the wrap cache up to date for `width` and returns the buffer length it synced against.
    fn sync(&self, width: usize) -> usize {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let (pushed, len, new_lines, full) = self.buf.with(|lines, pushed| {
            let delta = pushed.saturating_sub(cache.pushed);
            let full = cache.width != Some(width) || delta >= lines.len();
            let take = if full { lines.len() } else { delta };
            (pushed, lines.len(), lines.iter().rev().take(take).rev().cloned().collect::<Vec<_>>(), full)
        });
        if full {
            cache.rows = new_lines.iter().map(|l| wrap(l, width)).collect();
        } else {
            cache.rows.extend(new_lines.iter().map(|l| wrap(l, width)));
            while cache.rows.len() > len {
                cache.rows.pop_front();
            }
        }
        cache.pushed = pushed;
        cache.width = Some(width);
        len
    }

    /// The (line index, sub-row) for wrapped row `row`, counting from the front of `rows`.
    fn locate(rows: impl Iterator<Item = usize>, mut row: usize) -> (usize, usize) {
        for (i, c) in rows.enumerate() {
            if row < c {
                return (i, row);
            }
            row -= c;
        }
        (0, 0)
    }

    /// Total wrapped rows over the first `visible_len` cached lines.
    fn wrapped_len(cache: &WrapCache, visible_len: usize) -> usize {
        cache.rows.iter().take(visible_len).map(Vec::len).sum()
    }

    /// Scrolls `step` wrapped rows; `dims` is the content area's `(width, rows)`, `None` while not laid out.
    pub(super) fn scroll_by(&mut self, up: bool, step: usize, dims: Option<(usize, usize)>) {
        self.scroll = if up { self.scroll + step } else { self.scroll.saturating_sub(step) };
        let raw_len = self.buf.with(|lines, _| lines.len());
        if self.scroll > 0 && self.pin.is_none() {
            self.pin = Some(raw_len);
        }
        if let Some((width, h)) = dims {
            let len = self.sync(width);
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let wrapped_len = Self::wrapped_len(&cache, self.visible_len(len));
            drop(cache);
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

        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let total = Self::wrapped_len(&cache, visible_len);
        // Clamp to the top-most full window, so scrolling past the oldest line freezes there.
        let end = total.saturating_sub(self.scroll.min(total.saturating_sub(h)));
        let start = end.saturating_sub(h);
        if end == start {
            return;
        }

        let (line_lo, mut skip) = Self::locate(cache.rows.iter().take(visible_len).map(Vec::len), start);
        let mut row_y = 1;
        let mut emitted = 0;
        // Render only the already-wrapped rows the visible window needs, from the same locked cache used above.
        'outer: for wrapped in cache.rows.iter().skip(line_lo).take(visible_len - line_lo) {
            for w in wrapped.iter().skip(skip) {
                if emitted == end - start {
                    break 'outer;
                }
                printer.print((0, row_y), w);
                row_y += 1;
                emitted += 1;
            }
            skip = 0;
        }
    }
}
