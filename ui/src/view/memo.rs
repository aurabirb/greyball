use std::sync::{Mutex, MutexGuard};

/// A one-entry cache: the value built for the last key, behind a `Mutex` only because `draw` takes `&self`.
pub(super) struct Memo<K, V = ()>(Mutex<Option<(K, V)>>);

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<K: PartialEq, V> Memo<K, V> {
    fn slot(&self) -> MutexGuard<'_, Option<(K, V)>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The value for `key`, rebuilt only when `key` differs from the last call's; keep `V` cheap to clone (`Arc`).
    pub(super) fn get_or_build(&self, key: K, build: impl FnOnce() -> V) -> V
    where
        V: Clone,
    {
        let mut slot = self.slot();
        match &*slot {
            Some((k, v)) if *k == key => v.clone(),
            _ => slot.insert((key, build())).1.clone(),
        }
    }
}

impl<K: PartialEq> Memo<K> {
    /// Whether `key` differs from the last call's (true on the first), remembering it.
    pub(super) fn changed(&self, key: K) -> bool {
        let mut slot = self.slot();
        let changed = slot.as_ref().is_none_or(|(k, ())| *k != key);
        *slot = Some((key, ()));
        changed
    }
}
