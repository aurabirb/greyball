use cursive::{Printer, Rect, Vec2};
use cursive::event::EventResult;
use cursive::theme::ColorStyle;

use core::{Axis, PaneLayoutConfig, PaneMode, Side};

use crate::command::Pane;

use super::{HIST, MedleyView, QUEUE, draw_pane, draw_settings_pane, log_pin_after_scroll, settings_entries};
use super::scroll::bound_offset;
use super::text::{pad, wrap};

pub(super) fn list_screen_for_pane(pane: Pane) -> Option<usize> {
    match pane {
        Pane::Queue => Some(QUEUE),
        Pane::History => Some(HIST),
        Pane::Log | Pane::Settings | Pane::Vis => None,
    }
}

/// Rows reserved at the very top of the terminal and bottom.
pub(super) const TAB_BAR_ROWS: usize = 1;

pub(super) const BOTTOM_BAR_ROWS: usize = 2;

/// `Action::CyclePaneLayout`'s rotation, one `(side, stack)` step per press.
pub(crate) const PANE_LAYOUT_CYCLE: [(Side, Axis); 4] = [
    (Side::Right, Axis::Vertical),
    (Side::Bottom, Axis::Horizontal),
    (Side::Left, Axis::Vertical),
    (Side::Top, Axis::Horizontal),
];

/// Partitions the full screen into the main content rect and one rect per currently-open **embedded** pane.
pub(super) fn split(total: Vec2, open_panes: &[Pane], cfg: PaneLayoutConfig) -> (Rect, Vec<(Pane, Rect)>) {
    let band = Vec2::new(total.x, total.y.saturating_sub(TAB_BAR_ROWS + BOTTOM_BAR_ROWS));
    if open_panes.is_empty() {
        return (Rect::from_size((0, TAB_BAR_ROWS), band), Vec::new());
    }
    // One cell is reserved between main and the pane block for the "│"/"─" `draw()` prints there.
    const GUTTER: usize = 1;
    let n = open_panes.len();

    let (main, panes) = match cfg.side {
        // Left/Right: pane block is a narrow column alongside main, full band height.
        Side::Left | Side::Right => {
            let avail = band.x.saturating_sub(GUTTER);
            // Fixed fraction, floored so it never eats the whole screen; MVP — no per-pane resizing yet.
            let extent = if avail < 2 { avail } else { (avail / 3).clamp(1, avail - 1) };
            let main_w = avail - extent;
            let (main_x, side_x) = if cfg.side == Side::Left {
                (extent + GUTTER, 0)
            } else {
                (0, main_w + GUTTER)
            };
            let main = Rect::from_size((main_x, 0), (main_w, band.y));

            let panes: Vec<(Pane, Rect)> = open_panes
                .iter()
                .enumerate()
                .map(|(i, &pane)| {
                    let rect = match cfg.stack {
                        Axis::Vertical => {
                            let h = band.y / n;
                            let y = i * h;
                            let h = if i + 1 == n { band.y - y } else { h }; // last slot eats rounding
                            Rect::from_size((side_x, y), (extent, h))
                        }
                        Axis::Horizontal => {
                            let w = extent / n;
                            let x = side_x + i * w;
                            let w = if i + 1 == n { extent - i * w } else { w };
                            Rect::from_size((x, 0), (w, band.y))
                        }
                    };
                    (pane, rect)
                })
                .collect();
            (main, panes)
        }
        // Top/Bottom: pane block is a full-width bar above/below main.
        Side::Top | Side::Bottom => {
            let avail = band.y.saturating_sub(GUTTER);
            let extent = if avail < 2 { avail } else { (avail / 3).clamp(1, avail - 1) };
            let main_h = avail - extent;
            let (main_y, side_y) = if cfg.side == Side::Top {
                (extent + GUTTER, 0)
            } else {
                (0, main_h + GUTTER)
            };
            let main = Rect::from_size((0, main_y), (band.x, main_h));

            let panes: Vec<(Pane, Rect)> = open_panes
                .iter()
                .enumerate()
                .map(|(i, &pane)| {
                    let rect = match cfg.stack {
                        // Side by side across the full width — "split down the middle" for two panes.
                        Axis::Horizontal => {
                            let w = band.x / n;
                            let x = i * w;
                            let w = if i + 1 == n { band.x - x } else { w };
                            Rect::from_size((x, side_y), (w, extent))
                        }
                        Axis::Vertical => {
                            let h = extent / n;
                            let y = side_y + i * h;
                            let h = if i + 1 == n { extent - i * h } else { h };
                            Rect::from_size((0, y), (band.x, h))
                        }
                    };
                    (pane, rect)
                })
                .collect();
            (main, panes)
        }
    };

    let shift = |r: Rect| Rect::from_size((r.top_left().x, r.top_left().y + TAB_BAR_ROWS), r.size());
    (shift(main), panes.into_iter().map(|(p, r)| (p, shift(r))).collect())
}

pub(super) fn pane_title(pane: Pane) -> &'static str {
    match pane {
        Pane::Log => "Log",
        Pane::Settings => "Settings",
        Pane::Vis => "Vis",
        Pane::Queue => "Queue",
        Pane::History => "History",
    }
}

impl MedleyView {
    /// `Screen`-mode: `pane` fullscreen, title/content on top, an `Esc to close` hint on the bottom row.
    pub(super) fn draw_screen_pane(&self, pane: Pane, printer: &Printer) {
        let h = printer.size.y.saturating_sub(1);
        let content = printer.windowed(Rect::from_size((0, 0), Vec2::new(printer.size.x, h)));
        match pane {
            Pane::Vis => self.vis.draw(&content, true),
            Pane::Log => {
                let (lines, scroll) = self.log_render_lines();
                draw_pane(pane, &content, &lines, scroll, true);
            }
            Pane::Settings => {
                let pane_cfg = self.pane_cfg;
                let entries = self.with_session(|s| settings_entries(s, pane_cfg));
                draw_settings_pane(&content, &entries, self.settings_offset, self.settings_cursor, true);
            }
            // `toggle_pane` never routes these two here.
            Pane::Queue | Pane::History => unreachable!("Queue/History never become screen_pane"),
        }
        let hint = match pane {
            Pane::Vis => "  [Esc] close",
            Pane::Settings => "  [Esc] close   [↑/↓ j/k] move   [Enter/Space] toggle",
            _ => "  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll",
        };
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, h), &pad(hint, p.size.x));
        });
    }

    /// `pane`'s own placement: its `pane_mode_overrides` entry, else the shared default.
    pub(super) fn pane_mode(&self, pane: Pane) -> PaneMode {
        self.pane_mode_overrides.get(&pane).copied().unwrap_or(self.pane_cfg.mode)
    }

    /// Open/close `pane`, per its own `pane_mode` — `:log`, `:settings`, bare `:vis`, `:queue`, `:history`.
    pub(super) fn toggle_pane(&mut self, pane: Pane) {
        if self.pane_mode(pane) == PaneMode::Screen {
            if let Some(screen) = list_screen_for_pane(pane) {
                self.screen = screen;
                self.leave_playlists();
            } else {
                self.screen_pane = if self.screen_pane == Some(pane) { None } else { Some(pane) };
            }
        } else if let Some(i) = self.open_panes.iter().position(|&p| p == pane) {
            self.open_panes.remove(i);
        } else {
            self.open_panes.push(pane);
        }
        self.clamp_focus();
        self.clamp_scroll(); // covers the screen-switch branch above; a no-op otherwise
    }

    /// Sync cursive's own redraw rate to whether/how fast the Vis pane needs to animate.
    pub(super) fn vis_fps_cb(&self) -> EventResult {
        let vis_open = self.open_panes.contains(&Pane::Vis) || self.screen_pane == Some(Pane::Vis);
        self.vis.set_enabled(vis_open);
        let fps = if vis_open { crate::vis::FPS } else { crate::BASELINE_FPS };
        EventResult::with_cb(move |siv| siv.set_fps(fps))
    }

    /// Line-scroll for Log/Vis only.
    pub(super) fn scroll_pane(&mut self, pane: Pane, up: bool, step: usize) {
        if pane == Pane::Settings {
            self.jump_settings(up, step);
            return;
        }
        let s = match pane {
            Pane::Log => &mut self.log_scroll,
            Pane::Settings => unreachable!("handled above"),
            Pane::Vis => return, // nothing to scroll, it's live
            Pane::Queue | Pane::History => return,
        };
        if up {
            *s += step;
        } else {
            *s = s.saturating_sub(step);
        }
        // Pin (or release) the Log pane's view against `log`'s current length.
        self.log_pin = log_pin_after_scroll(self.log_scroll, self.log_pin, self.log.snapshot().len());
        self.clamp_pane_scroll(pane);
    }

    /// The (width, content-row-count) `draw_pane` actually renders `pane` into right now.
    pub(super) fn pane_content_dims(&self, pane: Pane) -> Option<(usize, usize)> {
        if self.screen_pane == Some(pane) {
            Some((self.last_screen_size.x, self.last_screen_size.y.saturating_sub(2)))
        } else {
            self.last_pane_rects
                .iter()
                .find(|&&(p, _)| p == pane)
                .map(|&(_, rect)| (rect.width(), rect.height().saturating_sub(1)))
        }
    }

    /// Keep `log_scroll` inside the actual scrollable range for `pane`'s current content and on-screen size.
    pub(super) fn clamp_pane_scroll(&mut self, pane: Pane) {
        let Some((width, h)) = self.pane_content_dims(pane) else { return };
        let lines = match pane {
            Pane::Log => self.log_render_lines().0,
            Pane::Settings | Pane::Vis | Pane::Queue | Pane::History => return,
        };
        let wrapped_len: usize = lines.iter().map(|l| wrap(l, width).len()).sum();
        self.log_scroll = bound_offset(self.log_scroll, wrapped_len, h);
    }
}
