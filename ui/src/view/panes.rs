use cursive::{Printer, Rect, Vec2};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::{Axis, PaneLayoutConfig, Side};

use crate::keybindings::Action;
use crate::screen::Kind;

use super::{Focus, MedleyView};
use super::modal::draw_modal_frame;
use super::window::{Placement, WindowId};

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

/// The main content rect and one rect per docked window, first = nearest the main content.
pub(super) fn split(total: Vec2, open_panes: &[WindowId], cfg: PaneLayoutConfig) -> (Rect, Vec<(WindowId, Rect)>) {
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

            let panes: Vec<(WindowId, Rect)> = open_panes
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

            let panes: Vec<(WindowId, Rect)> = open_panes
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

/// A floating window's border box: centered, three fifths of the area between the fixed rows, never below a usable minimum.
pub(super) fn float_rect(total: Vec2) -> Rect {
    let band_h = total.y.saturating_sub(TAB_BAR_ROWS + BOTTOM_BAR_ROWS);
    let size = Vec2::new((total.x * 3 / 5).max(40).min(total.x), (band_h * 3 / 5).max(10).min(band_h));
    Rect::from_size(((total.x - size.x) / 2, TAB_BAR_ROWS + (band_h - size.y) / 2), size)
}

/// The window's rect in its border box: its own title row takes the top border's place.
pub(super) fn float_body(frame: Rect) -> Rect {
    Rect::from_size(frame.top_left() + (1, 0), frame.size().saturating_sub((2, 1)))
}

/// Clears `frame` and boxes it, leaving the top edge between the corners to the window's title row.
pub(super) fn draw_float_frame(printer: &Printer, frame: Rect, focused: bool) {
    let blank = " ".repeat(frame.width());
    for y in frame.top()..=frame.bottom() {
        printer.print((frame.left(), y), &blank);
    }
    // `Printer::print_box` would force the theme's border colour, and draw nothing under `borders = none`.
    let style = if focused { ColorStyle::title_primary() } else { ColorStyle::primary() };
    printer.with_color(style, |p| {
        let (l, t, r, b) = (frame.left(), frame.top(), frame.right(), frame.bottom());
        p.print_hline((l, b), frame.width(), "─");
        p.print_vline((l, t), frame.height(), "│");
        p.print_vline((r, t), frame.height(), "│");
        for (pos, corner) in [((l, t), "┌"), ((r, t), "┐"), ((l, b), "└"), ((r, b), "┘")] {
            p.print(pos, corner);
        }
    });
}

impl MedleyView {
    /// Closes `id` if it is open, handing focus back to where it was when `id` opened.
    pub(super) fn close_window(&mut self, id: WindowId) -> bool {
        let Some(i) = self.open.iter().position(|&(open, _)| open == id) else { return false };
        let (_, prior) = self.open.remove(i);
        if self.focus == Focus::Window(id) {
            self.focus = prior;
        }
        true
    }

    /// Open/close a pane window per its placement — `:log`, `:settings`, `:vis`, `:queue`, `:history`.
    pub(super) fn toggle_window(&mut self, id: WindowId) {
        if self.close_window(id) {
            return;
        }
        let window = &self.windows[id];
        // A fullscreen list that also has a tab switches to the tab instead of layering.
        if let (Placement::Screen, Kind::List(screen)) = (window.placement, window.kind) {
            self.handle_action(Action::Screen(screen));
            return;
        }
        self.open.push((id, self.focus));
        if window.placement != Placement::Docked {
            self.focus = Focus::Window(id);
        }
    }

    /// An open window moves to its new placement at once, except to fullscreen, which would cover the view unasked.
    pub(super) fn set_placement(&mut self, id: WindowId, placement: Placement) {
        let reopen = self.close_window(id) && placement != Placement::Screen;
        self.windows[id].placement = placement;
        if reopen {
            self.toggle_window(id);
        }
    }

    /// Open windows in `placement`, oldest first.
    pub(super) fn open_in(&self, placement: Placement) -> impl Iterator<Item = WindowId> + '_ {
        self.open.iter().map(|&(id, _)| id).filter(move |&id| self.windows[id].placement == placement)
    }

    /// Focuses `id`; a floating window also comes to the top.
    pub(super) fn focus_window(&mut self, id: WindowId) {
        self.focus = Focus::Window(id);
        if self.windows[id].placement == Placement::Floating
            && let Some(i) = self.open.iter().position(|&(open, _)| open == id)
        {
            let entry = self.open.remove(i);
            self.open.push(entry);
        }
    }

    /// Sync cursive's own redraw rate to whether/how fast the Vis pane needs to animate.
    pub(super) fn vis_fps_cb(&self) -> EventResult {
        let vis_open = self.open.iter().any(|&(id, _)| self.windows[id].kind == Kind::Vis);
        self.vis.set_enabled(vis_open);
        let fps = if vis_open { crate::vis::FPS } else { crate::BASELINE_FPS };
        EventResult::with_cb(move |siv| siv.set_fps(fps))
    }

    /// The fullscreen window: footer hint below, the window itself above.
    pub(super) fn draw_fullscreen(&self, id: WindowId, printer: &Printer) {
        let window = &self.windows[id];
        let hint = match window.kind {
            Kind::Vis => "  [Esc] close",
            Kind::Settings => "  [Esc] close   [↑/↓ j/k] move   [Enter/Space] toggle",
            _ => "  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll",
        };
        draw_modal_frame(printer, Rect::from_size((0, 0), printer.size), None, hint);
        window.draw(printer, true, &self.with_session(|s| window.frame(&self.ctx(s))));
    }

    /// Keys go to the fullscreen window and no further; the mouse never reaches it.
    pub(super) fn on_fullscreen_event(&mut self, id: WindowId, event: &Event) -> EventResult {
        match event {
            Event::Key(Key::Esc) => {
                self.close_window(id);
                self.vis_fps_cb()
            }
            Event::Mouse { .. } => EventResult::consumed(),
            _ => self.send(&[id], event).map_or_else(EventResult::consumed, |(_, outcome)| self.apply(outcome)),
        }
    }
}

/// The one-cell rule between the main content and the docked pane block.
pub(super) fn draw_separator(side: Side, printer: &Printer, main_rect: Rect) {
    match side {
        Side::Left | Side::Right => {
            let x = if side == Side::Left {
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
            let y = if side == Side::Top {
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
