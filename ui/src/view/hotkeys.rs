use core::{BindError, HotkeyTarget};

use crate::items;
use crate::keybindings::{self, Taken};

use super::MedleyView;
use super::track_list::{top_row_name, top_rows};

impl MedleyView {
    /// This row's display name, looked up fresh.
    fn hotkey_row_name_for(&self, target: &HotkeyTarget) -> String {
        match target {
            HotkeyTarget::Builtin(action) => items::describe(*action).to_string(),
            HotkeyTarget::Local(_) | HotkeyTarget::Remote(..) => self.with_session(|s| {
                let playlists = s.playlists();
                top_rows(s).into_iter().find(|r| &r.target() == target).map(|r| top_row_name(&r, &playlists))
            }).unwrap_or_default(),
        }
    }

    /// Binds `key` to `target` and reports the result as feedback.
    pub(super) fn bind_hotkey(&mut self, target: HotkeyTarget, key: char) {
        let held = |by: &HotkeyTarget, view: &Self| format!("Can't bind '{key}': already used by built-in {}", view.hotkey_row_name_for(by));
        // A key the shell or a list reads first would never reach its binding.
        match self.with_session(|s| keybindings::taken(key, &s.hotkeys().into_iter().collect())) {
            Some(Taken::Fixed) => {
                self.feedback = Some(format!("Can't bind '{key}': it is a fixed key"));
                return;
            }
            Some(Taken::Builtin(action)) if target != HotkeyTarget::Builtin(action) => {
                self.feedback = Some(held(&HotkeyTarget::Builtin(action), self));
                return;
            }
            _ => {}
        }
        let result = self.with_session_mut(|s| s.bind_hotkey(key, target.clone()));
        let name = self.hotkey_row_name_for(&target);
        self.feedback = Some(match result {
            Ok(Some(stolen_from)) => {
                let stolen_name = self.hotkey_row_name_for(&stolen_from);
                format!("Bound '{key}' to {name} (moved from {stolen_name})")
            }
            Ok(None) => format!("Bound '{key}' to {name}"),
            Err(BindError::BuiltinKey(blocking)) => held(&blocking, self),
            Err(BindError::SyntheticPlaylist) => {
                format!("Can't bind '{key}': {name} isn't a real playlist — use like/unlike instead")
            }
        });
    }

    pub(super) fn clear_hotkey(&mut self, target: HotkeyTarget) {
        // A built-in falling back onto a default that a playlist took meanwhile would be shadowed by it.
        if let HotkeyTarget::Builtin(action) = &target
            && let Some(holder) = self.with_session(|s| s.hotkey_for(action.default_key())).filter(|holder| holder != &target)
        {
            let (default, holder) = (action.default_key(), self.hotkey_row_name_for(&holder));
            self.feedback = Some(format!("Can't restore '{default}': it is bound to {holder}, clear that first"));
            return;
        }
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        // A built-in without its own entry answers to its default again.
        let default = self.with_session(|s| s.effective_hotkey(&target));
        self.feedback = key.map(|k| match default {
            Some(default) => format!("Unbound '{k}': back on '{default}'"),
            None => format!("Unbound '{k}'"),
        });
    }
}
