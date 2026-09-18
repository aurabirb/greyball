use std::collections::HashMap;

use cursive::Printer;
use cursive::event::EventResult;
use cursive::theme::ColorStyle;

use core::{BindError, HotkeyTarget};

use super::MedleyView;
use super::playlists::top_row_name;
use super::scroll::{follow_cursor_offset, modal_list_h, stepped_cursor};
use super::text::{pad, truncate_ellipsis};

/// Row the hotkey-menu modal's playlist list starts on — same shape as `WARNINGS_LIST_TOP`.
pub(super) const HOTKEY_LIST_TOP: usize = 2;

impl MedleyView {
    /// Fullscreen "Hotkeys" modal (backtick, off the Playlists screen).
    pub(super) fn draw_hotkey_menu(&self, printer: &Printer) {
        let rows = self.hotkey_rows();
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Hotkeys", p.size.x));
        });

        let name_w = printer.size.x.saturating_sub(3);
        let lines: Vec<String> = rows
            .iter()
            .map(|&action| {
                let key = self
                    .with_session(|s| s.effective_hotkey(&HotkeyTarget::Builtin(action)))
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let name = pad(&truncate_ellipsis(action.label(), name_w), name_w);
                format!("{name} {key}")
            })
            .collect();
        self.draw_rows(printer, &lines, self.hotkey_menu_cursor, self.hotkey_menu_offset, HOTKEY_LIST_TOP);

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            let line = if let Some(target) = &self.hotkey_capture {
                let name = rows
                    .iter()
                    .find(|&&a| HotkeyTarget::Builtin(a) == *target)
                    .map(|a| a.label().to_string())
                    .unwrap_or_else(|| "?".to_string());
                format!("  press a key to bind to {name:?}   [Esc] cancel")
            } else if let Some(msg) = &self.hotkey_feedback {
                format!("  {msg}")
            } else {
                "  select a row and press Enter   [Backspace] clear   [Esc] close".to_string()
            };
            p.print((0, bottom), &pad(&line, p.size.x));
        });
    }

    /// Standalone "press a key to bind" modal for a Playlists-screen row.
    pub(super) fn draw_playlist_hotkey_modal(&self, printer: &Printer) {
        let target = self.hotkey_capture.clone().expect("only drawn while capturing");
        let name = self.hotkey_row_name_for(&target);
        let current = self.with_session(|s| s.playlist_hotkey(&target));
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Set Hotkey", p.size.x));
        });
        printer.print((0, HOTKEY_LIST_TOP), &format!("press a key to bind to {name:?}"));

        let bottom = printer.size.y.saturating_sub(1);
        let hint = match current {
            Some(k) => format!("  currently '{k}'   [Backspace] clear   [Esc] cancel"),
            None => "  [Esc] cancel".to_string(),
        };
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad(&hint, p.size.x));
        });
    }

    /// The live hotkey remap table, collected into the shape `keybindings::map`/`hotkey_toggle` take.
    pub(super) fn hotkeys_map(&self) -> HashMap<char, HotkeyTarget> {
        self.with_session(|s| s.hotkeys()).into_iter().collect()
    }

    /// The global hotkey menu's row list — every `core::BuiltinAction`.
    pub(super) fn hotkey_rows(&self) -> Vec<core::BuiltinAction> {
        core::BuiltinAction::ALL.iter().map(|&(a, _)| a).collect()
    }

    /// Same idea as `jump_warnings`, for the hotkey-menu modal.
    pub(super) fn jump_hotkey_menu(&mut self, up: bool, step: usize) {
        let n = self.hotkey_rows().len();
        self.hotkey_menu_cursor = stepped_cursor(self.hotkey_menu_cursor, n, up, step);
        self.follow_hotkey_menu_offset();
    }

    /// Same idea as `follow_warnings_offset`, for the hotkey menu.
    pub(super) fn follow_hotkey_menu_offset(&mut self) {
        let h = modal_list_h(self.last_screen_size.y, HOTKEY_LIST_TOP);
        self.hotkey_menu_offset = follow_cursor_offset(self.hotkey_menu_cursor, self.hotkey_menu_offset, h);
    }

    pub(super) fn open_hotkey_menu(&mut self) {
        self.hotkey_menu_open = true;
        self.hotkey_menu_cursor = 0;
        self.hotkey_menu_offset = 0;
        self.hotkey_capture = None;
        self.hotkey_feedback = None;
        self.with_session(|s| s.clear_membership_feedback());
    }

    /// Enter on the selected hotkey-menu row.
    pub(super) fn open_hotkey_capture(&mut self) {
        let Some(&action) = self.hotkey_rows().get(self.hotkey_menu_cursor) else {
            return;
        };
        self.hotkey_capture = Some(HotkeyTarget::Builtin(action));
        self.hotkey_feedback = None;
    }

    /// Opens the standalone "press a key to bind" modal for `target` on the Playlists screen.
    pub(super) fn open_playlist_hotkey_modal(&mut self, target: HotkeyTarget) {
        self.hotkey_capture = Some(target);
        self.hotkey_feedback = None;
    }

    /// This row's display name, looked up fresh.
    fn hotkey_row_name_for(&self, target: &HotkeyTarget) -> String {
        match target {
            HotkeyTarget::Builtin(action) => action.label().to_string(),
            HotkeyTarget::Local(_) | HotkeyTarget::Remote(..) => self.with_session(|s| {
                let playlists = s.playlists();
                self.top_rows(s).into_iter().find(|r| &r.target() == target).map(|r| top_row_name(&r, &playlists))
            }).unwrap_or_default(),
        }
    }

    /// Binds `key` to the `hotkey_capture` target, reports the result in `hotkey_feedback`, closes the sub-popup.
    pub(super) fn bind_captured_key(&mut self, key: char) -> EventResult {
        let Some(target) = self.hotkey_capture.take() else {
            return EventResult::consumed();
        };
        let result = self.with_session_mut(|s| s.bind_hotkey(key, target.clone()));
        let name = self.hotkey_row_name_for(&target);
        self.hotkey_feedback = Some(match result {
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
        EventResult::consumed()
    }

    /// Backspace on the selected hotkey-menu row: clears that row's binding, if it has one.
    pub(super) fn clear_selected_hotkey(&mut self) -> EventResult {
        let Some(&action) = self.hotkey_rows().get(self.hotkey_menu_cursor) else {
            return EventResult::consumed();
        };
        self.clear_hotkey(HotkeyTarget::Builtin(action))
    }

    /// Backspace on the standalone playlist hotkey modal.
    pub(super) fn clear_captured_hotkey(&mut self) -> EventResult {
        let Some(target) = self.hotkey_capture.take() else {
            return EventResult::consumed();
        };
        self.clear_hotkey(target)
    }

    /// Shared by `clear_selected_hotkey`/`clear_captured_hotkey`.
    fn clear_hotkey(&mut self, target: HotkeyTarget) -> EventResult {
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        self.hotkey_feedback = key.map(|k| format!("Unbound '{k}'"));
        EventResult::consumed()
    }
}
