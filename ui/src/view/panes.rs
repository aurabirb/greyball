use std::collections::HashMap;

use cursive::{Printer, Rect, Vec2};
use cursive::event::EventResult;

use core::{Axis, PaneLayoutConfig, PaneMode, Side};

use crate::command::Pane;
use crate::keybindings::Action;
use crate::screen::Screen;

use super::MedleyView;
use super::modal::Modal;

/// Rows reserved at the very top of the terminal and bottom.
const TAB_BAR_ROWS: usize = 1;

const BOTTOM_BAR_ROWS: usize = 2;

/// `Action::CyclePaneLayout`'s rotation, one `(side, stack)` step per press.
pub(crate) const PANE_LAYOUT_CYCLE: [(Side, Axis); 4] = [
    (Side::Right, Axis::Vertical),
    (Side::Bottom, Axis::Horizontal),
    (Side::Left, Axis::Vertical),
    (Side::Top, Axis::Horizontal),
];

/// Which optional panes are docked around the main content, and where.
pub(super) struct PaneLayout {
    /// Docked panes in stack order, first = nearest the main content.
    pub(super) open: Vec<Pane>,
    /// Shared default placement; `:panes` changes it live, `side`/`stack` apply to every docked pane.
    pub(super) cfg: PaneLayoutConfig,
    /// Per-pane override of `cfg.mode`, applied the next time that pane is toggled.
    pub(super) mode_overrides: HashMap<Pane, PaneMode>,
}

impl PaneLayout {
    pub(super) fn new(cfg: PaneLayoutConfig) -> Self {
        Self {
            open: Vec::new(),
            cfg,
            mode_overrides: HashMap::new(),
        }
    }

    fn mode(&self, pane: Pane) -> PaneMode {
        self.mode_overrides.get(&pane).copied().unwrap_or(self.cfg.mode)
    }

    /// The main content rect and each docked pane's rect on a `size` screen.
    pub(super) fn split(&self, size: Vec2) -> (Rect, Vec<(Pane, Rect)>) {
        split(size, &self.open, self.cfg)
    }
}

/// Partitions the full screen into the main content rect and one rect per currently-open **embedded** pane.
fn split(total: Vec2, open_panes: &[Pane], cfg: PaneLayoutConfig) -> (Rect, Vec<(Pane, Rect)>) {
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
            // Fixed fraction, floored so it never eats the whole screen.
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
    /// Open/close `pane`, per its own placement mode — `:log`, `:settings`, bare `:vis`, `:queue`, `:history`.
    pub(super) fn toggle_pane(&mut self, pane: Pane) {
        if self.panes.mode(pane) == PaneMode::Screen {
            if let Some(screen) = Screen::from_pane(pane) {
                self.handle_action(Action::Screen(screen));
            } else {
                self.modal = Some(Modal::Pane(pane));
            }
        } else if let Some(i) = self.panes.open.iter().position(|&p| p == pane) {
            self.panes.open.remove(i);
        } else {
            self.panes.open.push(pane);
        }
        self.clamp_focus();
    }

    /// Sync cursive's own redraw rate to whether/how fast the Vis pane needs to animate.
    pub(super) fn vis_fps_cb(&self) -> EventResult {
        let vis_open = self.panes.open.contains(&Pane::Vis) || self.is_fullscreen(Pane::Vis);
        self.vis.set_enabled(vis_open);
        let fps = if vis_open { crate::vis::FPS } else { crate::BASELINE_FPS };
        EventResult::with_cb(move |siv| siv.set_fps(fps))
    }

    pub(super) fn is_fullscreen(&self, pane: Pane) -> bool {
        matches!(self.modal, Some(Modal::Pane(p)) if p == pane)
    }
}

impl PaneLayout {
    /// The one-cell rule between the main content and the docked pane block.
    pub(super) fn draw_separator(&self, printer: &Printer, main_rect: Rect) {
        match self.cfg.side {
            Side::Left | Side::Right => {
                let x = if self.cfg.side == Side::Left {
                    main_rect.top_left().x - 1
                } else {
                    main_rect.top_left().x + main_rect.width()
                };
                let (y0, y1) = (main_rect.top_left().y, main_rect.top_left().y + main_rect.height());
                for y in y0..y1 {
                    printer.print((x, y), "│");
                }
            }
            Side::Top | Side::Bottom => {
                let y = if self.cfg.side == Side::Top {
                    main_rect.top_left().y - 1
                } else {
                    main_rect.top_left().y + main_rect.height()
                };
                for x in 0..printer.size.x {
                    printer.print((x, y), "─");
                }
            }
        }
    }
}
