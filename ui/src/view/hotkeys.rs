use cursive::event::EventResult;

use core::{BindError, HotkeyTarget};

use crate::items;
use crate::keybindings::{self, Taken};

use super::{MedleyView, Notice};
use super::input::confirm;
use super::track_list::hotkey_target_name;

impl MedleyView {
    /// This row's display name, looked up fresh.
    fn hotkey_row_name_for(&self, target: &HotkeyTarget) -> String {
        match target {
            HotkeyTarget::Builtin(action) => items::describe(*action).to_string(),
            HotkeyTarget::Local(_) | HotkeyTarget::Remote(..) => self.with_session(|s| hotkey_target_name(s, target)),
        }
    }

    /// Binds `key` to `target`, asking first when that takes the key from a playlist or replaces a playlist's key.
    pub(super) fn bind_hotkey(&mut self, target: HotkeyTarget, key: char) -> EventResult {
        let held = |by: &HotkeyTarget, view: &Self| format!("Can't bind '{key}': already used by built-in {}", view.hotkey_row_name_for(by));
        // A key the shell or a list reads first would never reach its binding.
        match self.with_session(|s| keybindings::taken(key, &s.hotkeys().into_iter().collect())) {
            Some(Taken::Fixed) => return self.refuse(format!("Can't bind '{key}': it is a fixed key")),
            Some(Taken::Builtin(action)) if target != HotkeyTarget::Builtin(action) => {
                return self.refuse(held(&HotkeyTarget::Builtin(action), self));
            }
            _ => {}
        }
        let (holder, old) = self.with_session(|s| (s.hotkey_for(key), s.playlist_hotkey(&target)));
        let name = self.hotkey_row_name_for(&target);
        // A built-in is rebound on purpose; only a playlist's own key going is news.
        let old = old.filter(|&old| old != key && !matches!(target, HotkeyTarget::Builtin(_)));
        let question = match (holder.filter(|holder| *holder != target), old) {
            (Some(holder), None) => format!("Take '{key}' from {:?} and bind it to {name:?}?", self.hotkey_row_name_for(&holder)),
            (Some(holder), Some(old)) => {
                format!("Take '{key}' from {:?} and bind it to {name:?}, replacing its key '{old}'?", self.hotkey_row_name_for(&holder))
            }
            (None, Some(old)) => format!("Replace the key '{old}' of {name:?} with '{key}'?"),
            (None, None) => return self.commit_bind(target, key),
        };
        // The dialog holds what it named, so nothing that re-sorts or rebinds meanwhile can retarget it.
        confirm("Bind key", question, "Bind", move |view| view.commit_bind(target.clone(), key))
    }

    fn commit_bind(&mut self, target: HotkeyTarget, key: char) -> EventResult {
        let held = |by: &HotkeyTarget, view: &Self| format!("Can't bind '{key}': already used by built-in {}", view.hotkey_row_name_for(by));
        let result = self.with_session_mut(|s| s.bind_hotkey(key, target.clone()));
        let name = self.hotkey_row_name_for(&target);
        match result {
            Ok(Some(stolen_from)) => {
                let stolen_name = self.hotkey_row_name_for(&stolen_from);
                self.notify(Notice::Status { text: format!("Bound '{key}' to {name} (moved from {stolen_name})"), refused: false })
            }
            Ok(None) => self.notify(Notice::Status { text: format!("Bound '{key}' to {name}"), refused: false }),
            Err(BindError::BuiltinKey(blocking)) => self.refuse(held(&blocking, self)),
            Err(BindError::SyntheticPlaylist) => {
                self.refuse(format!("Can't bind '{key}': {name} isn't a real playlist — use the like key instead"))
            }
        }
    }

    fn refuse(&mut self, text: String) -> EventResult {
        self.notify(Notice::Status { text, refused: true })
    }

    pub(super) fn clear_hotkey(&mut self, target: HotkeyTarget) -> EventResult {
        // A built-in falling back onto a default that a playlist took meanwhile would be shadowed by it.
        if let HotkeyTarget::Builtin(action) = &target
            && let Some(default) = action.default_key()
            && let Some(holder) = self.with_session(|s| s.hotkey_for(default)).filter(|holder| holder != &target)
        {
            let holder = self.hotkey_row_name_for(&holder);
            return self.refuse(format!("Can't restore '{default}': it is bound to {holder}, clear that first"));
        }
        let key = self.with_session(|s| s.playlist_hotkey(&target));
        self.with_session_mut(|s| s.unbind_hotkey(&target));
        // A built-in without its own entry answers to its default again.
        let default = self.with_session(|s| s.effective_hotkey(&target));
        let Some(key) = key else { return EventResult::consumed() };
        let text = match default {
            Some(default) => format!("Unbound '{key}': back on '{default}'"),
            None => format!("Unbound '{key}'"),
        };
        self.notify(Notice::Status { text, refused: false })
    }
}
