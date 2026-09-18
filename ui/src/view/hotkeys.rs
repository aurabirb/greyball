use std::collections::HashMap;

use cursive::{Printer, Vec2};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::{BindError, HotkeyTarget};

use super::MedleyView;
use super::input::key_name;
use super::playlists::top_row_name;
use super::scroll::{ListEvent, ListState, modal_list_rect};
use super::text::{pad, truncate_ellipsis};

/// Row the Hotkeys menu's list, and the standalone capture prompt, start on.
const LIST_TOP: usize = 2;

/// Hotkey-binding UI state: the Hotkeys menu, the "press a key" capture, and the last bind result.
#[derive(Default)]
pub(super) struct HotkeyUi {
    /// The fullscreen Hotkeys menu's list; `Some` only while the menu is open.
    pub(super) menu: Option<ListState>,
    /// Target awaiting its new key — a menu row, or a Playlists-screen row with the menu closed.
    pub(super) capture: Option<HotkeyTarget>,
    /// Last bind/unbind result, shown until the next keypress.
    pub(super) feedback: Option<String>,
}

/// The Hotkeys menu's rows — every `core::BuiltinAction`.
fn menu_rows() -> Vec<core::BuiltinAction> {
    core::BuiltinAction::ALL.iter().map(|&(a, _)| a).collect()
}

impl HotkeyUi {
    pub(super) fn relayout(&mut self, resized: bool, size: Vec2) {
        if let Some(list) = &mut self.menu {
            list.relayout(resized, menu_rows().len(), modal_list_rect(size, LIST_TOP).height());
        }
    }

    /// The fullscreen Hotkeys menu; `keys` is each `menu_rows` entry's bound key, `-` if none.
    fn draw_menu(&self, list: &ListState, printer: &Printer, keys: &[String]) {
        let rows = menu_rows();
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Hotkeys", p.size.x));
        });

        let name_w = printer.size.x.saturating_sub(3);
        let lines: Vec<String> = rows
            .iter()
            .zip(keys)
            .map(|(action, key)| format!("{} {key}", pad(&truncate_ellipsis(action.label(), name_w), name_w)))
            .collect();
        list.draw(&printer.windowed(modal_list_rect(printer.size, LIST_TOP)), &lines);

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            let line = if let Some(target) = &self.capture {
                let name = rows
                    .iter()
                    .find(|&&a| HotkeyTarget::Builtin(a) == *target)
                    .map(|a| a.label().to_string())
                    .unwrap_or_else(|| "?".to_string());
                format!("  press a key to bind to {name:?}   [Esc] cancel")
            } else if let Some(msg) = &self.feedback {
                format!("  {msg}")
            } else {
                "  select a row and press Enter   [Backspace] clear   [Esc] close".to_string()
            };
            p.print((0, bottom), &pad(&line, p.size.x));
        });
    }

    /// Standalone "press a key to bind" modal for the Playlists-screen row `name`, currently bound to `current`.
    fn draw_capture(printer: &Printer, name: &str, current: Option<char>) {
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Set Hotkey", p.size.x));
        });
        printer.print((0, LIST_TOP), &format!("press a key to bind to {name:?}"));

        let bottom = printer.size.y.saturating_sub(1);
        let hint = match current {
            Some(k) => format!("  currently '{k}'   [Backspace] clear   [Esc] cancel"),
            None => "  [Esc] cancel".to_string(),
        };
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad(&hint, p.size.x));
        });
    }
}

impl MedleyView {
    /// Draws whichever hotkey modal is up; `false` if none is.
    pub(super) fn draw_hotkey_ui(&self, printer: &Printer) -> bool {
        if let Some(list) = &self.hotkeys.menu {
            let keys: Vec<String> = self.with_session(|s| {
                menu_rows()
                    .into_iter()
                    .map(|a| s.effective_hotkey(&HotkeyTarget::Builtin(a)).map_or("-".to_string(), |k| k.to_string()))
                    .collect()
            });
            self.hotkeys.draw_menu(list, printer, &keys);
            true
        } else if let Some(target) = &self.hotkeys.capture {
            let current = self.with_session(|s| s.playlist_hotkey(target));
            HotkeyUi::draw_capture(printer, &self.hotkey_row_name_for(target), current);
            true
        } else {
            false
        }
    }

    /// Handles `event` if a hotkey modal is up — the capture prompt first, as it sits on top of the menu.
    pub(super) fn on_hotkey_ui_event(&mut self, event: &Event) -> Option<EventResult> {
        if self.hotkeys.capture.is_some() {
            return Some(match event {
                Event::Key(Key::Esc) => {
                    self.hotkeys.capture = None;
                    EventResult::consumed()
                }
                Event::Key(Key::Backspace) => self.clear_captured_hotkey(),
                ev => match key_name(ev) {
                    Some(k) if k.chars().count() == 1 => self.bind_captured_key(k.chars().next().unwrap()),
                    _ => EventResult::consumed(),
                },
            });
        }
        let size = self.last_screen_size;
        let list = self.hotkeys.menu.as_mut()?;
        Some(match list.on_event(event, menu_rows().len(), modal_list_rect(size, LIST_TOP)) {
            ListEvent::Close => {
                self.hotkeys.menu = None;
                self.focus = self.fallback_focus();
                EventResult::consumed()
            }
            ListEvent::Activate => {
                self.hotkeys.capture = menu_rows().get(list.cursor).map(|&a| HotkeyTarget::Builtin(a));
                EventResult::consumed()
            }
            ListEvent::Unhandled if *event == Event::Key(Key::Backspace) => self.clear_selected_hotkey(),
            ListEvent::Clicked | ListEvent::Moved | ListEvent::Unhandled => EventResult::consumed(),
        })
    }

    /// The live hotkey remap table, collected into the shape `keybindings::map`/`hotkey_toggle` take.
    pub(super) fn hotkeys_map(&self) -> HashMap<char, HotkeyTarget> {
        self.with_session(|s| s.hotkeys()).into_iter().collect()
    }

    pub(super) fn open_hotkey_menu(&mut self) {
        self.hotkeys = HotkeyUi { menu: Some(ListState::default()), ..HotkeyUi::default() };
        self.with_session(|s| s.clear_membership_feedback());
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

    /// Binds `key` to the captured target, reports the result as feedback, closes the capture prompt.
    fn bind_captured_key(&mut self, key: char) -> EventResult {
        let Some(target) = self.hotkeys.capture.take() else {
            return EventResult::consumed();
        };
        let result = self.with_session_mut(|s| s.bind_hotkey(key, target.clone()));
        let name = self.hotkey_row_name_for(&target);
        self.hotkeys.feedback = Some(match result {
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
    fn clear_selected_hotkey(&mut self) -> EventResult {
        let selected = self.hotkeys.menu.map_or(0, |list| list.cursor);
        let Some(&action) = menu_rows().get(selected) else {
            return EventResult::consumed();
        };
        self.clear_hotkey(HotkeyTarget::Builtin(action))
    }

    /// Backspace on the standalone playlist hotkey modal.
    fn clear_captured_hotkey(&mut self) -> EventResult {
        let Some(target) = self.hotkeys.capture.take() else {
            return EventResult::consumed();
        };
        self.clear_hotkey(target)
    }

    /// Shared by `clear_selected_hotkey`/`clear_captured_hotkey`.
    fn clear_hotkey(&mut self, target: HotkeyTarget) -> EventResult {
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        self.hotkeys.feedback = key.map(|k| format!("Unbound '{k}'"));
        EventResult::consumed()
    }
}
