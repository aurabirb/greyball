use cursive::{Rect, Vec2};

use core::Side;

use crate::screen::{Corner, Placement};

use super::modal::Modal;
use super::panes::BOTTOM_BAR_ROWS;
use super::status_line::StatusLine;
use super::warnings::warnings_rect;
use super::window::{Window, WindowId};
use super::{MedleyView, Placed};

/// The narrowest slot a command line is drawn in; a docked pane can be about 30 columns.
const INPUT_FLOOR: usize = 40;

/// The nearest bottom-line region a surface offers to one screen corner.
pub(super) struct Slot {
    /// The surface's bottom line, in screen coordinates.
    pub(super) row: Rect,
    /// Whether the row is the scrubber line rather than a window's status row.
    pub(super) scrubber: bool,
    /// The window occupying the corner.
    pub(super) host: WindowId,
}

pub(super) struct Slots {
    left: Option<Slot>,
    right: Option<Slot>,
}

impl Slots {
    pub(super) fn at(&self, corner: Corner) -> Option<&Slot> {
        match corner {
            Corner::Left => self.left.as_ref(),
            Corner::Right => self.right.as_ref(),
        }
    }
}

/// The warnings button: its cells, and the slot it is drawn in.
pub(super) struct Widget {
    pub(super) rect: Rect,
    pub(super) scrubber: bool,
    pub(super) host: WindowId,
}

impl Widget {
    /// The scrubber line's width once the button, if it sits there, has taken its cells.
    pub(super) fn status_width(widget: Option<&Widget>, total: usize) -> usize {
        total - widget.filter(|widget| widget.scrubber).map_or(0, |widget| widget.rect.width())
    }
}

impl MedleyView {
    /// The slots nearest each bottom corner, from the placed rects and what each surface offers.
    pub(super) fn slots(&self, placed: &[Placed]) -> Slots {
        Slots { left: self.slot(placed, Corner::Left), right: self.slot(placed, Corner::Right) }
    }

    /// A window docked along the bottom edge offers no bottom corner: another surface is nearer.
    fn docked_at_bottom(&self, id: WindowId) -> bool {
        self.windows.placement(id) == Placement::Docked && self.pane_cfg.side == Side::Bottom
    }

    /// The scrubber line if it is shown and offers `corner`, else the nearest status row that offers it.
    fn slot(&self, placed: &[Placed], corner: Corner) -> Option<Slot> {
        if self.modal.is_some() && !Modal::CORNERS.offers(corner) {
            return None;
        }
        let size = self.last_screen_size;
        let x = if corner == Corner::Left { 0 } else { size.x.checked_sub(1)? };
        let point = Vec2::new(x, size.y.checked_sub(BOTTOM_BAR_ROWS + 1)?);
        if self.fullscreen().is_none() && StatusLine::CORNERS.offers(corner) {
            let host = placed.iter().rev().find(|placed| placed.frame.contains(point))?.id;
            return Some(Slot { row: Rect::from_size((0, size.y - 1), (size.x, 1)), scrubber: true, host });
        }
        let reach = |row: Rect| {
            let end = if corner == Corner::Left { row.left() } else { row.left() + row.width() - 1 };
            end.abs_diff(point.x) + row.top().abs_diff(point.y)
        };
        placed
            .iter()
            .filter(|placed| !self.docked_at_bottom(placed.id))
            .filter_map(|placed| {
                let row = self.windows[placed.id].status_rect().filter(|_| Window::CORNERS.offers(corner))?;
                Some(Slot { row, scrubber: false, host: placed.id })
            })
            .min_by_key(|slot| reach(slot.row))
    }

    /// The warnings button, when there are warnings and the right slot can hold it.
    pub(super) fn warnings_widget(slots: &Slots, count: usize) -> Option<Widget> {
        let slot = slots.at(Corner::Right).filter(|_| count > 0)?;
        Some(Widget { rect: warnings_rect(count, slot.row), scrubber: slot.scrubber, host: slot.host })
    }

    /// The warnings button as drawn now, for input handling outside `draw`.
    pub(super) fn current_widget(&self) -> Option<Widget> {
        Self::warnings_widget(&self.slots(&self.placed()), self.warn_count())
    }

    /// Where the input line draws: the left slot's row up to the button when that is wide enough, else the whole bottom line.
    pub(super) fn input_rect(&self, slots: &Slots, widget: Option<&Widget>) -> Rect {
        let clipped = |row: Rect| {
            let beside = widget.filter(|widget| widget.rect.top() == row.top() && widget.rect.left() >= row.left());
            Rect::from_size(row.top_left(), (beside.map_or(row.width(), |widget| widget.rect.left() - row.left()), 1))
        };
        let size = self.last_screen_size;
        let slot = slots.at(Corner::Left).map(|slot| clipped(slot.row)).filter(|rect| rect.width() >= INPUT_FLOOR);
        slot.unwrap_or_else(|| clipped(Rect::from_size((0, size.y.saturating_sub(1)), (size.x, 1))))
    }
}
