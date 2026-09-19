use cursive::{Printer, Rect};
use cursive::event::{Event, Key};

use core::{BindError, HotkeyTarget, Session};

use crate::keybindings;

use super::MedleyView;
use super::input::key_name;
use super::modal::{Modal, ModalOutcome, draw_modal_frame, modal_list};
use super::track_list::{top_row_name, top_rows};
use super::scroll::{ListEvent, ListState, Nav};
use super::text::{pad, truncate_ellipsis};

/// The fullscreen Hotkeys menu: every `core::BuiltinAction`, plus the row awaiting a new key, if any.
#[derive(Default)]
pub(super) struct HotkeyMenu {
    list: ListState,
    capture: Option<(HotkeyTarget, String)>,
}

fn menu_rows() -> Vec<core::BuiltinAction> {
    core::BuiltinAction::ALL.iter().map(|&(a, _)| a).collect()
}

/// A keypress while a target awaits its new key.
fn capture_event(event: &Event, target: &HotkeyTarget) -> ModalOutcome {
    match event {
        Event::Key(Key::Esc) => ModalOutcome::Close,
        Event::Key(Key::Backspace) => ModalOutcome::Unbind(target.clone()),
        ev => {
            let name = key_name(ev).unwrap_or_default();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(key), None) => ModalOutcome::Bind(target.clone(), key),
                _ => ModalOutcome::Stay,
            }
        }
    }
}

impl HotkeyMenu {
    /// Each `menu_rows` entry's bound key, `-` if none.
    pub(super) fn keys(s: &Session) -> Vec<String> {
        menu_rows()
            .into_iter()
            .map(|a| s.effective_hotkey(&HotkeyTarget::Builtin(a)).map_or("-".to_string(), |k| k.to_string()))
            .collect()
    }

    pub(super) fn relayout(&mut self, resized: bool, rect: Rect) {
        self.list.relayout(resized, menu_rows().len(), modal_list(rect).height());
    }

    pub(super) fn draw(&self, printer: &Printer, rect: Rect, keys: &[String], feedback: Option<&str>) {
        let footer = if let Some((_, name)) = &self.capture {
            format!("  press a key to bind to {name:?}   [Esc] cancel")
        } else if let Some(msg) = feedback {
            format!("  {msg}")
        } else {
            "  select a row and press Enter   [Backspace] clear   [Esc] close".to_string()
        };
        draw_modal_frame(printer, rect, Some("Hotkeys"), &footer);
        let name_w = rect.width().saturating_sub(3);
        let lines: Vec<String> = menu_rows()
            .iter()
            .zip(keys)
            .map(|(action, key)| format!("{} {key}", pad(&truncate_ellipsis(action.label(), name_w), name_w)))
            .collect();
        self.list.draw(&printer.windowed(modal_list(rect)), &lines);
    }

    pub(super) fn on_event(&mut self, event: &Event, rect: Rect) -> ModalOutcome {
        if let Some((target, _)) = &self.capture {
            let outcome = capture_event(event, target);
            if !matches!(outcome, ModalOutcome::Stay) {
                self.capture = None;
            }
            return match outcome {
                ModalOutcome::Close => ModalOutcome::Stay,
                outcome => outcome,
            };
        }
        let outcome = self.list.on_event(event, menu_rows().len(), modal_list(rect));
        let selected = || menu_rows().get(self.list.cursor).copied();
        match outcome {
            ListEvent::Close => ModalOutcome::Close,
            ListEvent::Activate => {
                self.capture = selected().map(|a| (HotkeyTarget::Builtin(a), a.label().to_string()));
                ModalOutcome::Stay
            }
            ListEvent::Unhandled if *event == Event::Key(Key::Backspace) => {
                selected().map_or(ModalOutcome::Stay, |a| ModalOutcome::Unbind(HotkeyTarget::Builtin(a)))
            }
            ListEvent::Clicked | ListEvent::Moved | ListEvent::Unhandled => ModalOutcome::Stay,
        }
    }
}

impl MedleyView {
    pub(super) fn open_hotkey_menu(&mut self) {
        self.modal = Some(Modal::HotkeyMenu(HotkeyMenu::default()));
    }

    /// This row's display name, looked up fresh.
    fn hotkey_row_name_for(&self, target: &HotkeyTarget) -> String {
        match target {
            HotkeyTarget::Builtin(action) => action.label().to_string(),
            HotkeyTarget::Local(_) | HotkeyTarget::Remote(..) => self.with_session(|s| {
                let playlists = s.playlists();
                top_rows(s).into_iter().find(|r| &r.target() == target).map(|r| top_row_name(&r, &playlists))
            }).unwrap_or_default(),
        }
    }

    /// Binds `key` to `target` and reports the result as feedback.
    pub(super) fn bind_hotkey(&mut self, target: HotkeyTarget, key: char) {
        // A key the shell or a list reads first would never reach its binding.
        if keybindings::is_fixed(key) || Nav::of(&Event::Char(key)).is_some() {
            self.feedback = Some(format!("Can't bind '{key}': it is a fixed key"));
            return;
        }
        let result = self.with_session_mut(|s| s.bind_hotkey(key, target.clone()));
        let name = self.hotkey_row_name_for(&target);
        self.feedback = Some(match result {
            Ok(Some(stolen_from)) => {
                let stolen_name = self.hotkey_row_name_for(&stolen_from);
                format!("Bound '{key}' to {name} (moved from {stolen_name})")
            }
            Ok(None) => format!("Bound '{key}' to {name}"),
            Err(BindError::BuiltinKey(blocking)) => {
                let blocking_name = self.hotkey_row_name_for(&blocking);
                format!("Can't bind '{key}': already used by built-in {blocking_name}")
            }
            Err(BindError::SyntheticPlaylist) => {
                format!("Can't bind '{key}': {name} isn't a real playlist — use like/unlike instead")
            }
        });
    }

    pub(super) fn clear_hotkey(&mut self, target: HotkeyTarget) {
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        self.feedback = key.map(|k| format!("Unbound '{k}'"));
    }
}
