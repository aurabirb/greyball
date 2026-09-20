use cursive::{Printer, Rect};
use cursive::event::{Event, EventResult};
use cursive::theme::ColorStyle;

use core::Command;

use crate::screen::Corners;

use super::{Focus, MedleyView};
use super::input::Editing;
use super::playlist_picker::PlaylistPicker;
use super::text::pad;
use super::warnings::{Warnings, WarningsModal};

/// The one exclusive layer over the main view; every variant swallows all input while open.
pub(super) enum Modal {
    Warnings(WarningsModal),
    Picker(PlaylistPicker),
}

impl Modal {
    /// A modal takes all input, so nothing may draw in a corner under or over it.
    pub(super) const CORNERS: Corners = Corners::NONE;

    /// Whether the main view is not drawn under it.
    pub(super) fn covers_screen(&self) -> bool {
        matches!(self, Modal::Warnings(_))
    }
}

/// What an event meant to the open modal, for `MedleyView` to act on.
pub(super) enum ModalOutcome {
    Stay,
    Close,
    /// Close, then dispatch.
    Run(Command),
    /// Run setup for the plugin at this row of `plugin_statuses`.
    Setup(usize),
}

/// `rect` minus its footer row and, when `titled`, its title row — shared by `draw_modal_frame` and hit-tests.
pub(super) fn modal_body(rect: Rect, titled: bool) -> Rect {
    let top = usize::from(titled);
    Rect::from_size((rect.left(), rect.top() + top), (rect.width(), rect.height().saturating_sub(top + 1)))
}

/// A modal's list area: its body minus a spacer row above.
pub(super) fn modal_list(rect: Rect) -> Rect {
    let body = modal_body(rect, true);
    Rect::from_size((body.left(), body.top() + 1), (body.width(), body.height().saturating_sub(1)))
}

/// Title bar and footer hint at the edges of `rect`; returns the printer for the body between them.
pub(super) fn draw_modal_frame<'a, 'b>(
    printer: &Printer<'a, 'b>,
    rect: Rect,
    title: Option<&str>,
    footer: &str,
) -> Printer<'a, 'b> {
    let frame = printer.windowed(rect);
    if let Some(title) = title {
        frame.with_color(ColorStyle::title_primary(), |p| p.print((0, 0), &pad(title, p.size.x)));
    }
    frame.with_color(ColorStyle::highlight_inactive(), |p| {
        p.print((0, p.size.y.saturating_sub(1)), &pad(footer, p.size.x));
    });
    printer.windowed(modal_body(rect, title.is_some()))
}

impl MedleyView {
    /// The picker sizes and places itself from the screen; the warnings modal covers it.
    fn modal_rect(&self) -> Rect {
        Rect::from_size((0, 0), self.last_screen_size)
    }

    pub(super) fn close_modal(&mut self) -> EventResult {
        self.modal = None;
        if self.focus == Focus::Warnings {
            self.focus = Focus::Window(self.main_id());
        }
        EventResult::consumed()
    }

    /// Layout-pass upkeep for the open modal, under one session lock.
    pub(super) fn relayout_modal(&mut self, resized: bool) {
        let (rect, session) = (self.modal_rect(), self.session.clone());
        let s = session.lock().unwrap();
        match &mut self.modal {
            Some(Modal::Warnings(m)) => m.relayout(resized, rect, &Warnings::read(&s)),
            Some(Modal::Picker(picker)) => picker.relayout(resized, self.last_screen_size, &s),
            None => {}
        }
    }

    pub(super) fn draw_modal(&self, modal: &Modal, printer: &Printer) {
        let rect = Rect::from_size((0, 0), printer.size);
        match modal {
            Modal::Warnings(m) => {
                let (warnings, prompt) = self.with_session(|s| {
                    let prompt = match &self.editing {
                        Editing::PluginSetup(id, answers) => {
                            Some(s.plugin(id).and_then(|p| p.setup_prompt(answers)).unwrap_or_default())
                        }
                        _ => None,
                    };
                    (Warnings::read(s), prompt)
                });
                m.draw(printer, rect, &warnings, prompt.as_deref().map(|p| (p, self.buffer.as_str())));
            }
            Modal::Picker(picker) => picker.draw(printer, printer.size),
        }
    }

    pub(super) fn on_modal_event(&mut self, event: &Event) -> EventResult {
        let rect = self.modal_rect();
        let session = self.session.clone();
        let outcome = match &mut self.modal {
            None => return EventResult::Ignored,
            Some(Modal::Warnings(m)) => m.on_event(event, rect, &Warnings::read(&session.lock().unwrap())),
            Some(Modal::Picker(picker)) => picker.on_event(event, self.last_screen_size),
        };
        match outcome {
            ModalOutcome::Stay => {}
            ModalOutcome::Close => return self.close_modal(),
            ModalOutcome::Run(cmd) => {
                self.close_modal();
                return self.run(cmd);
            }
            ModalOutcome::Setup(row) => self.activate_warning(row),
        }
        EventResult::consumed()
    }
}
