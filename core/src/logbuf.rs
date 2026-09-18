//! `LogBuf` — a small in-memory ring of recent log lines, so `ui` can render a
//! tail of them (the Log pane) without depending on the `log`/file-logging
//! machinery that lives in `app`. Pure data; nothing here touches `log::Log` —
//! `app` pushes formatted lines in, `ui` reads a snapshot out.

use std::collections::VecDeque;
use std::sync::Mutex;

struct Inner {
    lines: VecDeque<String>,
    /// Monotonic count of lines ever pushed, unaffected by eviction.
    pushed: usize,
}

pub struct LogBuf {
    cap: usize,
    inner: Mutex<Inner>,
}

impl LogBuf {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            inner: Mutex::new(Inner { lines: VecDeque::with_capacity(cap), pushed: 0 }),
        }
    }

    /// Append one line, dropping the oldest once `cap` is exceeded.
    pub fn push(&self, line: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.lines.len() >= self.cap {
            inner.lines.pop_front();
        }
        inner.lines.push_back(line);
        inner.pushed += 1;
    }

    /// Runs `f` under one lock with the lines and pushed counter for a consistent read; `f` must not log.
    pub fn with<R>(&self, f: impl FnOnce(&VecDeque<String>, usize) -> R) -> R {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&inner.lines, inner.pushed)
    }
}

impl Default for LogBuf {
    fn default() -> Self {
        Self::new(200)
    }
}

