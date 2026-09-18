use cursive::Printer;
use cursive::theme::ColorStyle;

use crate::command::Pane;

use super::MedleyView;
use super::panes::pane_title;
use super::text::{pad, wrap};

/// Updates the Log pane's pin point (`log_pin`) after `scroll` changes.
pub(super) fn log_pin_after_scroll(scroll: usize, pin: Option<usize>, live_len: usize) -> Option<usize> {
    if scroll == 0 { None } else { Some(pin.unwrap_or(live_len)) }
}

/// Length of the Log snapshot to actually render this frame.
pub(super) fn log_visible_len(scroll: usize, pin: Option<usize>, live_len: usize) -> usize {
    if scroll == 0 { live_len } else { pin.unwrap_or(live_len).min(live_len) }
}

/// Draw a pane's title + content into its own (already-windowed) printer.
pub(super) fn draw_pane(pane: Pane, printer: &Printer, lines: &[String], scroll: usize, focused: bool) {
    let mut title = match (pane, scroll > 0) {
        (Pane::Log, true) => format!("{} (scrolled, PgDn to catch up)", pane_title(pane)),
        (Pane::Settings, true) => format!("{} (scrolled)", pane_title(pane)),
        _ => pane_title(pane).to_string(),
    };
    if focused {
        title = format!("[{title}]");
    }
    printer.with_color(ColorStyle::title_secondary(), |p| {
        p.print((0, 0), &pad(&title, p.size.x));
    });
    let width = printer.size.x;
    let wrapped: Vec<String> = lines.iter().flat_map(|l| wrap(l, width)).collect();
    let h = printer.size.y.saturating_sub(1);
    // Each *wrapped* line counts as a row, so a long line takes the space it needs.
    let visible: Vec<&String> = if pane == Pane::Log {
        let total = wrapped.len();
        // Clamp to the top-most full window, so scrolling past the oldest line freezes there.
        let max_scroll = total.saturating_sub(h);
        let end = total.saturating_sub(scroll.min(max_scroll));
        let start = end.saturating_sub(h);
        wrapped[start..end].iter().collect()
    } else {
        let max_scroll = wrapped.len().saturating_sub(h);
        wrapped.iter().skip(scroll.min(max_scroll)).take(h).collect()
    };
    for (i, line) in visible.into_iter().enumerate() {
        printer.print((0, i + 1), line);
    }
}

impl MedleyView {
    /// The Log pane's content/scroll for this frame.
    pub(super) fn log_render_lines(&self) -> (Vec<String>, usize) {
        let snapshot = self.log.snapshot();
        let len = log_visible_len(self.log_scroll, self.log_pin, snapshot.len());
        (snapshot[..len].to_vec(), self.log_scroll)
    }
}
