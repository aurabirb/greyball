use std::collections::HashMap;

use cursive::{Printer, Rect, Vec2};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::{Axis, PaneLayoutConfig, PaneMode, Side};

use crate::command::Pane;

use super::{HIST, MedleyView, QUEUE};
use super::scroll::{Nav, PAGE_SCROLL_STEP};
use super::settings::settings_entries;
use super::text::pad;

pub(super) fn list_screen_for_pane(pane: Pane) -> Option<usize> {
    match pane {
        Pane::Queue => Some(QUEUE),
        Pane::History => Some(HIST),
        Pane::Log | Pane::Settings | Pane::Vis => None,
    }
}

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

/// Which optional panes are up and where: docked around the main content, or one of them fullscreen.
pub(super) struct PaneLayout {
    /// Docked panes in stack order, first = nearest the main content.
    pub(super) open: Vec<Pane>,
    /// Shared default placement; `:panes` changes it live, `side`/`stack` apply to every docked pane.
    pub(super) cfg: PaneLayoutConfig,
    /// Per-pane override of `cfg.mode`, applied the next time that pane is toggled.
    pub(super) mode_overrides: HashMap<Pane, PaneMode>,
    /// The pane shown fullscreen in place of the screens; Esc is the only way out.
    pub(super) fullscreen: Option<Pane>,
    /// The main list's rect as of the last layout pass.
    pub(super) main_rect: Rect,
    /// Each docked pane's rect as of the last layout pass.
    pub(super) rects: Vec<(Pane, Rect)>,
}

impl PaneLayout {
    pub(super) fn new(cfg: PaneLayoutConfig) -> Self {
        Self {
            open: Vec::new(),
            cfg,
            mode_overrides: HashMap::new(),
            fullscreen: None,
            main_rect: Rect::from_size((0, 0), (0, 0)),
            rects: Vec::new(),
        }
    }

    fn mode(&self, pane: Pane) -> PaneMode {
        self.mode_overrides.get(&pane).copied().unwrap_or(self.cfg.mode)
    }

    /// The main content rect and each docked pane's rect on a `size` screen.
    pub(super) fn split(&self, size: Vec2) -> (Rect, Vec<(Pane, Rect)>) {
        split(size, &self.open, self.cfg)
    }

    /// Recomputes the rects: `(main height changed, per docked pane (pane, body height, it changed))`.
    pub(super) fn relayout(&mut self, size: Vec2) -> (bool, Vec<(Pane, usize, bool)>) {
        let (main_rect, rects) = self.split(size);
        let main_h_changed = main_rect.height() != self.main_rect.height();
        let bodies = rects
            .iter()
            .map(|&(pane, rect)| {
                let h = rect.height().saturating_sub(1);
                (pane, h, self.body_h(pane) != Some(h))
            })
            .collect();
        self.main_rect = main_rect;
        self.rects = rects;
        (main_h_changed, bodies)
    }

    /// Rows a docked `pane` has under its title row, as of the last layout pass.
    pub(super) fn body_h(&self, pane: Pane) -> Option<usize> {
        self.rects.iter().find(|&&(p, _)| p == pane).map(|&(_, rect)| rect.height().saturating_sub(1))
    }

    /// The `(width, rows)` `pane`'s content is rendered into right now, fullscreen or docked.
    pub(super) fn content_dims(&self, pane: Pane, screen: Vec2) -> Option<(usize, usize)> {
        if self.fullscreen == Some(pane) {
            Some((screen.x, screen.y.saturating_sub(2)))
        } else {
            self.rects.iter().find(|&&(p, _)| p == pane).map(|&(_, rect)| (rect.width(), rect.height().saturating_sub(1)))
        }
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
    /// `Screen`-mode: `pane` fullscreen, title/content on top, an `Esc to close` hint on the bottom row.
    pub(super) fn draw_screen_pane(&self, pane: Pane, printer: &Printer) {
        let h = printer.size.y.saturating_sub(1);
        let content = printer.windowed(Rect::from_size((0, 0), Vec2::new(printer.size.x, h)));
        match pane {
            Pane::Vis => self.vis.draw(&content, true),
            Pane::Log => self.log.draw(&content, true),
            Pane::Settings => {
                let pane_cfg = self.panes.cfg;
                let entries = self.with_session(|s| settings_entries(s, pane_cfg));
                self.settings.draw(&content, &entries, true);
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

    /// Open/close `pane`, per its own placement mode — `:log`, `:settings`, bare `:vis`, `:queue`, `:history`.
    pub(super) fn toggle_pane(&mut self, pane: Pane) {
        if self.panes.mode(pane) == PaneMode::Screen {
            if let Some(screen) = list_screen_for_pane(pane) {
                self.screen = screen;
                self.playlists.leave();
            } else {
                self.panes.fullscreen = if self.panes.fullscreen == Some(pane) { None } else { Some(pane) };
            }
        } else if let Some(i) = self.panes.open.iter().position(|&p| p == pane) {
            self.panes.open.remove(i);
        } else {
            self.panes.open.push(pane);
        }
        self.clamp_focus();
        self.clamp_scroll(); // covers the screen-switch branch above; a no-op otherwise
    }

    /// Sync cursive's own redraw rate to whether/how fast the Vis pane needs to animate.
    pub(super) fn vis_fps_cb(&self) -> EventResult {
        let vis_open = self.panes.open.contains(&Pane::Vis) || self.panes.fullscreen == Some(Pane::Vis);
        self.vis.set_enabled(vis_open);
        let fps = if vis_open { crate::vis::FPS } else { crate::BASELINE_FPS };
        EventResult::with_cb(move |siv| siv.set_fps(fps))
    }

    /// Scrolls a non-list pane: Settings moves its row cursor, Log its view; Vis is live.
    pub(super) fn scroll_pane(&mut self, pane: Pane, up: bool, step: usize) {
        match pane {
            Pane::Settings => self.jump_settings(up, step),
            Pane::Log => {
                let dims = self.panes.content_dims(pane, self.last_screen_size);
                self.log.scroll_by(up, step, dims);
            }
            Pane::Vis | Pane::Queue | Pane::History => {}
        }
    }

    /// The fullscreen pane's events: Esc closes it, nav keys scroll it, everything else is swallowed.
    pub(super) fn on_fullscreen_pane_event(&mut self, pane: Pane, event: &Event) -> EventResult {
        match (event, Nav::of(event)) {
            (Event::Key(Key::Esc), _) => {
                self.panes.fullscreen = None;
                if pane == Pane::Vis {
                    self.vis.set_enabled(false);
                    return EventResult::with_cb(|siv| siv.set_fps(crate::BASELINE_FPS));
                }
            }
            (_, Some(nav @ (Nav::Line(_) | Nav::Page(_)))) => {
                let (up, step) = nav.step(PAGE_SCROLL_STEP);
                self.scroll_pane(pane, up, step);
            }
            _ => {}
        }
        EventResult::consumed()
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
