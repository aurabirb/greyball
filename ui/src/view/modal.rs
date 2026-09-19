use cursive::{Printer, Rect};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::{Command, HotkeyTarget, SetupKind};

use crate::command::Pane;

use super::{Focus, MedleyView};
use super::help::HelpModal;
use super::hotkeys::{HotkeyMenu, capture_event, draw_capture};
use super::input::Editing;
use super::playlist_picker::PlaylistPicker;
use super::text::pad;
use super::warnings::WarningsModal;
use super::window::WindowId;

/// The one exclusive layer over the main view; every variant swallows all input while open.
pub(super) enum Modal {
    Warnings(WarningsModal),
    Picker(PlaylistPicker),
    HotkeyMenu(HotkeyMenu),
    /// "Press a key" for a Playlists row: the target and its display name.
    HotkeyCapture(HotkeyTarget, String),
    Help(HelpModal),
    /// A `PaneMode::Screen` pane shown fullscreen.
    Pane(Pane),
}

/// What an event meant to the open modal, for `MedleyView` to act on.
pub(super) enum ModalOutcome {
    Stay,
    Close,
    /// Close, then dispatch.
    Run(Command),
    /// Run setup for the plugin at this row of `plugin_statuses`.
    Setup(usize),
    Bind(HotkeyTarget, char),
    Unbind(HotkeyTarget),
}

/// `rect` minus its footer row and, when `titled`, its title row — shared by `draw_modal_frame` and hit-tests.
pub(super) fn modal_body(rect: Rect, titled: bool) -> Rect {
    let top = usize::from(titled);
    Rect::from_size((rect.left(), rect.top() + top), (rect.width(), rect.height().saturating_sub(top + 1)))
}

/// A modal's list area: its body minus a spacer row above and a free row below.
pub(super) fn modal_list(rect: Rect) -> Rect {
    let body = modal_body(rect, true);
    Rect::from_size((body.left(), body.top() + 1), (body.width(), body.height().saturating_sub(2)))
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
    /// Every modal covers the whole screen today.
    fn modal_rect(&self) -> Rect {
        Rect::from_size((0, 0), self.last_screen_size)
    }

    pub(super) fn close_modal(&mut self) -> EventResult {
        self.modal = None;
        if self.focus == Focus::Warnings {
            self.focus = Focus::Main;
        }
        self.vis_fps_cb()
    }

    /// Layout-pass upkeep for the open modal, under one session lock.
    pub(super) fn relayout_modal(&mut self, resized: bool) {
        let (rect, session) = (self.modal_rect(), self.session.clone());
        let s = session.lock().unwrap();
        match &mut self.modal {
            Some(Modal::Warnings(m)) => m.relayout(resized, rect, s.plugin_statuses()),
            Some(Modal::Picker(picker)) => picker.relayout(resized, rect, &s),
            Some(Modal::HotkeyMenu(menu)) => menu.relayout(resized, rect),
            Some(Modal::Help(help)) => help.relayout(rect, &s),
            Some(Modal::Pane(pane)) => self.windows[WindowId::Pane(*pane)].relayout(modal_body(rect, false), &s),
            Some(Modal::HotkeyCapture(..)) | None => {}
        }
    }

    pub(super) fn draw_modal(&self, modal: &Modal, printer: &Printer) {
        let rect = Rect::from_size((0, 0), printer.size);
        match modal {
            Modal::Warnings(m) => {
                let setup_id = match &self.editing {
                    Editing::PluginSetup(id) => Some(id),
                    _ => None,
                };
                let (statuses, prompt) = self.with_session(|s| {
                    let prompt = setup_id.map(|id| match s.plugin(id).map(|p| p.setup_kind()) {
                        Some(SetupKind::TextInput { prompt }) => prompt,
                        Some(SetupKind::Action) | None => String::new(),
                    });
                    (s.plugin_statuses().to_vec(), prompt)
                });
                m.draw(printer, rect, &statuses, prompt.as_deref().map(|p| (p, self.buffer.as_str())));
            }
            Modal::Picker(picker) => picker.draw(printer, rect),
            Modal::HotkeyMenu(menu) => {
                let keys = self.with_session(HotkeyMenu::keys);
                menu.draw(printer, rect, &keys, self.feedback.as_deref());
            }
            Modal::HotkeyCapture(target, name) => {
                let current = self.with_session(|s| s.playlist_hotkey(target));
                draw_capture(printer, rect, name, current);
            }
            Modal::Help(help) => help.draw(printer, rect),
            Modal::Pane(pane) => {
                let hint = match pane {
                    Pane::Vis => "  [Esc] close",
                    Pane::Settings => "  [Esc] close   [↑/↓ j/k] move   [Enter/Space] toggle",
                    _ => "  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll",
                };
                draw_modal_frame(printer, rect, None, hint);
                let window = &self.windows[WindowId::Pane(*pane)];
                window.draw(printer, true, &self.with_session(|s| window.frame(&self.ctx(s))));
            }
        }
    }

    pub(super) fn on_modal_event(&mut self, event: &Event) -> EventResult {
        let rect = self.modal_rect();
        let statuses = match self.modal {
            Some(Modal::Warnings(_)) => self.with_session(|s| s.plugin_statuses().to_vec()),
            _ => Vec::new(),
        };
        let outcome = match &mut self.modal {
            None => return EventResult::Ignored,
            Some(Modal::Warnings(m)) => m.on_event(event, rect, &statuses),
            Some(Modal::Picker(picker)) => picker.on_event(event, rect),
            Some(Modal::HotkeyMenu(menu)) => menu.on_event(event, rect),
            Some(Modal::HotkeyCapture(target, _)) => capture_event(event, target),
            Some(Modal::Help(help)) => help.on_event(event, rect),
            // Keys go to the window itself; the mouse never reaches a fullscreen pane.
            Some(Modal::Pane(pane)) => {
                let id = WindowId::Pane(*pane);
                return match event {
                    Event::Key(Key::Esc) => self.close_modal(),
                    Event::Mouse { .. } => EventResult::consumed(),
                    _ => self.send(&[id], event).map_or_else(EventResult::consumed, |(_, outcome)| self.apply(outcome)),
                };
            }
        };
        let rebound = match outcome {
            ModalOutcome::Stay => false,
            ModalOutcome::Close => return self.close_modal(),
            ModalOutcome::Run(cmd) => {
                self.close_modal();
                return self.run(cmd);
            }
            ModalOutcome::Setup(row) => {
                self.activate_warning(row);
                false
            }
            ModalOutcome::Bind(target, key) => {
                self.bind_hotkey(target, key);
                true
            }
            ModalOutcome::Unbind(target) => {
                self.clear_hotkey(target);
                true
            }
        };
        if rebound && matches!(self.modal, Some(Modal::HotkeyCapture(..))) {
            self.modal = None;
        }
        EventResult::consumed()
    }
}
