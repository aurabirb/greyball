use std::collections::HashMap;

use crate::app::{BuiltinAction, HotkeyTarget};

/// The key → target table, with a generation for caches of what it shows.
#[derive(Default)]
pub(crate) struct Hotkeys {
    map: HashMap<char, HotkeyTarget>,
    generation: u64,
}

impl Hotkeys {
    pub(crate) fn map(&self) -> &HashMap<char, HotkeyTarget> {
        &self.map
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Binds `key` to `target`, returning who it was stolen from, or the built-in that holds `key`.
    pub(crate) fn bind(&mut self, key: char, target: HotkeyTarget) -> Result<Option<HotkeyTarget>, HotkeyTarget> {
        let stolen = bind_hotkey(&mut self.map, key, target)?;
        self.generation += 1;
        Ok(stolen)
    }

    /// Keeps only the bindings `keep` accepts; returns whether any went.
    pub(crate) fn retain(&mut self, keep: impl FnMut(&char, &mut HotkeyTarget) -> bool) -> bool {
        let bound = self.map.len();
        self.map.retain(keep);
        let changed = self.map.len() != bound;
        self.generation += u64::from(changed);
        changed
    }

    pub(crate) fn replace(&mut self, map: HashMap<char, HotkeyTarget>) {
        self.map = map;
        self.generation += 1;
    }
}

/// Whatever answers to `key`: its table entry, else the built-in defaulting to it that isn't remapped elsewhere.
fn effective_target_at(hotkeys: &HashMap<char, HotkeyTarget>, key: char) -> Option<HotkeyTarget> {
    if let Some(t) = hotkeys.get(&key) {
        return Some(t.clone());
    }
    BuiltinAction::ALL.iter().find_map(|&(action, default)| {
        let target = HotkeyTarget::Builtin(action);
        (default == key && !hotkeys.values().any(|t| *t == target)).then_some(target)
    })
}

/// Drops `target`'s previous key and steals `key`; a rebind to the same key isn't a steal.
fn bind_hotkey(
    hotkeys: &mut HashMap<char, HotkeyTarget>,
    key: char,
    target: HotkeyTarget,
) -> std::result::Result<Option<HotkeyTarget>, HotkeyTarget> {
    if let Some(occupant) = effective_target_at(hotkeys, key)
        && occupant != target
        && matches!(occupant, HotkeyTarget::Builtin(_))
    {
        return Err(occupant);
    }
    hotkeys.retain(|&k, p| *p != target || k == key);
    Ok(hotkeys.insert(key, target.clone()).filter(|p| *p != target))
}
