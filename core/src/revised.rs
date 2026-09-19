use std::ops::Deref;

/// A value readable through `Deref` whose only write path bumps `revision`.
pub(crate) struct Revised<T> {
    value: T,
    revision: u64,
}

impl<T> Revised<T> {
    pub(crate) fn new(value: T) -> Self {
        Self { value, revision: 0 }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// For a change to state held elsewhere.
    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn write(&mut self) -> &mut T {
        self.touch();
        &mut self.value
    }
}

impl<T> Deref for Revised<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}
