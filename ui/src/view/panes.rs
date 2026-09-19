use cursive::{Printer, Rect, Vec2};
use cursive::event::EventResult;
use cursive::theme::ColorStyle;

use core::{Axis, PaneLayoutConfig, Session, Side};

use crate::screen::{HELP, Kind, ListKind, Placement};

use super::{Focus, MedleyView};
use super::input::Editing;
use super::track_list::TrackList;
use super::window::WindowId;

/// Rows reserved at the very top of the terminal and bottom.
const TAB_BAR_ROWS: usize = 2;

pub(super) const BOTTOM_BAR_ROWS: usize = 1;

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

/// How far each further floating window sits right of and below the one before.
const CASCADE: Vec2 = Vec2 { x: 4, y: 2 };

/// A floating window's border box: three fifths of the area between the fixed rows, cascaded from the centre by `slot`.
pub(super) fn float_rect(total: Vec2, slot: usize) -> Rect {
    let band_h = total.y.saturating_sub(TAB_BAR_ROWS + BOTTOM_BAR_ROWS);
    let size = Vec2::new((total.x * 3 / 5).max(40).min(total.x), (band_h * 3 / 5).max(11).min(band_h));
    let room = Vec2::new(total.x, band_h) - size;
    let origin = (room / 2 + CASCADE * slot).or_min(room);
    Rect::from_size(origin + (0, TAB_BAR_ROWS), size)
}

/// The window's rect in its border box: inside the border, one blank column each side.
pub(super) fn float_body(frame: Rect) -> Rect {
    Rect::from_size(frame.top_left() + (2, 1), frame.size().saturating_sub((4, 2)))
}

/// Clears `frame` and boxes it in the normal text colour.
pub(super) fn draw_float_frame(printer: &Printer, frame: Rect) {
    let blank = " ".repeat(frame.width());
    for y in frame.top()..=frame.bottom() {
        printer.print((frame.left(), y), &blank);
    }
    // `Printer::print_box` would force the theme's border colour, and draw nothing under `borders = none`.
    printer.with_color(ColorStyle::primary(), |p| {
        let (l, t, r, b) = (frame.left(), frame.top(), frame.right(), frame.bottom());
        p.print_hline((l, t), frame.width(), "─");
        p.print_hline((l, b), frame.width(), "─");
        p.print_vline((l, t), frame.height(), "│");
        p.print_vline((r, t), frame.height(), "│");
        for (pos, corner) in [((l, t), "┌"), ((r, t), "┐"), ((l, b), "└"), ((r, b), "┘")] {
            p.print(pos, corner);
        }
    });
}

impl MedleyView {
    /// Closes `id` if it is open; its focus goes to what it opened over, else the next shown window down the stack, else the tab.
    pub(super) fn close_window(&mut self, id: WindowId) -> bool {
        let Some(i) = self.open.iter().position(|&(open, _)| open == id) else { return false };
        let (_, prior) = self.open.remove(i);
        self.windows[id].blur();
        if self.focus == Focus::Window(id) {
            let shown = self.visible();
            let below = self.open[..i].iter().rev().map(|&(open, _)| Focus::Window(open));
            self.focus = std::iter::once(prior)
                .chain(below)
                .find(|focus| matches!(focus, Focus::Window(id) if shown.contains(id)))
                .unwrap_or(Focus::Window(self.main_id()));
        }
        true
    }

    /// Makes `id` the active tab and focuses it, closing what covers it.
    fn activate(&mut self, id: WindowId) {
        while let Some(screen) = self.fullscreen() {
            self.close_window(screen);
        }
        self.focus_window(id);
        self.active = id;
        self.kick_playlists(id);
    }

    /// A shown Playlists window retries a source's top-level list that failed or came back empty.
    fn kick_playlists(&self, id: WindowId) {
        if self.windows[id].kind == Kind::List(ListKind::Playlists) {
            self.with_session(Session::ensure_remote_playlists);
        }
    }

    /// Brings `id` into view: its tab, else open and focused; a Search list takes the query input at once, same as `/`.
    pub(super) fn show(&mut self, id: WindowId) {
        if self.windows.placement(id) == Placement::Tabbed {
            self.activate(id);
        } else {
            if !self.open.iter().any(|&(open, _)| open == id) {
                self.open.push((id, self.focus));
            }
            self.focus_window(id);
            self.kick_playlists(id);
        }
        if self.windows[id].kind == Kind::List(ListKind::Search) {
            self.editing = Editing::Search;
            self.buffer.clear();
        }
    }

    /// `:window <name>` and its short forms: a tab is switched to, any other window opened or closed.
    pub(super) fn toggle_window(&mut self, id: WindowId) {
        if self.windows.placement(id) == Placement::Tabbed {
            self.activate(id);
        } else if !self.close_window(id) {
            self.open.push((id, self.focus));
            self.kick_playlists(id);
            self.focus_window(id);
        }
    }

    /// The `SwitchPlaylists` key: the other Playlists window takes focus, or opens at its top level when none is shown.
    pub(super) fn switch_playlists(&mut self) {
        let Some(id) = self.windows.keyed_first() else { return };
        let shown = self.visible();
        if self.focus == Focus::Window(id) {
            if self.windows.placement(id) == Placement::Floating {
                self.close_window(id);
                return;
            }
            let prior = self.open.iter().find(|&&(open, _)| open == id).map(|&(_, prior)| prior);
            let tab = self.windows.ids().find(|&tab| self.windows.companion(tab) == Some(id));
            let back = prior.into_iter().chain(tab.map(Focus::Window)).find(|focus| matches!(focus, Focus::Window(w) if *w != id && shown.contains(w)));
            if let Some(Focus::Window(back)) = back {
                self.focus_window(back);
            }
            return;
        }
        if shown.contains(&id) {
            if let Some(entry) = self.open.iter_mut().find(|entry| entry.0 == id) {
                entry.1 = self.focus;
            }
            self.focus_window(id);
            return;
        }
        // The playlist the user came from: the one playing, else open in the active list, else in any other window.
        let lists = std::iter::once(self.active_list_id()).chain(self.windows.ids().filter(|&other| other != id));
        let open = || lists.filter_map(|id| self.windows[id].list()).find_map(TrackList::open_target);
        let from = self.with_session(Session::playing_playlist).or_else(open);
        if let Some(list) = self.windows[id].list_mut() {
            list.show_top(from);
        }
        self.show(id);
        self.focus = Focus::Window(id);
    }

    /// The `OpenHelp` key and `:help`: closes the help window when it has focus, else shows and focuses it.
    pub(super) fn toggle_help(&mut self) {
        let Some(id) = self.windows.named(HELP) else { return };
        if self.focus == Focus::Window(id) && self.close_window(id) {
            return;
        }
        self.windows[id].blur();
        self.show(id);
        self.focus = Focus::Window(id);
    }

    /// Moves `id`, keeping a shown window shown and a focused one focused; only `cover` lets it take the whole screen.
    pub(super) fn set_placement(&mut self, id: WindowId, placement: Placement, cover: bool) -> bool {
        // A startup tab never moves: its companion does, and opens if it was closed.
        let id = match self.windows.companion(id) {
            Some(companion) => {
                let opens = placement != Placement::Tabbed && (cover || placement != Placement::Screen);
                if opens && !self.open.iter().any(|&(open, _)| open == companion) {
                    self.open.push((companion, self.focus));
                }
                companion
            }
            None => id,
        };
        if placement == Placement::Tabbed && self.windows.is_companion(id) {
            self.close_window(id);
            return true;
        }
        let (from, focused) = (self.windows.placement(id), self.focus == Focus::Window(id));
        if from == placement {
            return true;
        }
        let shown = match self.tabs.iter().position(|&tab| tab == id) {
            Some(i) => {
                self.tabs.remove(i);
                let active = self.active == id;
                if active {
                    self.active = self.tabs[i.min(self.tabs.len() - 1)];
                }
                active
            }
            None => self.close_window(id),
        };
        self.windows.place(id, placement);
        let show = shown && (cover || placement != Placement::Screen);
        if placement == Placement::Tabbed {
            self.tabs.push(id);
            if show {
                self.active = id;
            }
        } else if show {
            self.open.push((id, Focus::Window(self.active)));
        }
        if show && (focused || matches!(placement, Placement::Floating | Placement::Screen)) {
            self.focus_window(id);
        }
        true
    }

    /// `Action::CyclePlacement`: the focused window moves on one placement, and its status row names it.
    pub(super) fn cycle_placement(&mut self) {
        let id = self.focused_id();
        if let Some(companion) = self.windows.companion(id) {
            if self.set_placement(id, Placement::Docked, true) {
                self.focus_window(companion);
                self.set_flash(format!("{}: {}", self.windows[id].kind.label(), Placement::Docked.word()));
            }
            return;
        }
        let Some(next) = self.windows.next_placement(id) else {
            self.close_window(id);
            self.set_flash(format!("{}: closed", self.windows[id].kind.label()));
            return;
        };
        if self.set_placement(id, next, true) {
            self.set_flash(format!("{}: {}", self.windows[id].kind.label(), next.word()));
        }
    }

    /// Open windows in `placement`, oldest first.
    pub(super) fn open_in(&self, placement: Placement) -> impl Iterator<Item = WindowId> + '_ {
        self.open.iter().map(|&(id, _)| id).filter(move |&id| self.windows.placement(id) == placement)
    }

    /// Focuses `id`; a floating window also comes to the top.
    pub(super) fn focus_window(&mut self, id: WindowId) {
        if let Focus::Window(old) = self.focus
            && old != id
        {
            self.windows[old].blur();
        }
        self.focus = Focus::Window(id);
        if self.windows.placement(id) == Placement::Floating
            && let Some(i) = self.open.iter().position(|&(open, _)| open == id)
        {
            let entry = self.open.remove(i);
            self.open.push(entry);
        }
    }

    /// Syncs cursive's redraw rate to whether a Vis window is shown; a callback only when that changed.
    pub(super) fn sync_vis_fps(&mut self) -> EventResult {
        let shown = self.visible().into_iter().any(|id| self.windows[id].kind == Kind::Vis);
        let want = shown.then(|| self.vis.fps());
        if want == self.vis_fast {
            return EventResult::Ignored;
        }
        self.vis_fast = want;
        self.vis.set_enabled(shown);
        let fps = want.unwrap_or(crate::BASELINE_FPS);
        EventResult::with_cb(move |siv| siv.set_fps(fps))
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
