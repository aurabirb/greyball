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

    /// Oldest-first snapshot of everything currently buffered.
    pub fn snapshot(&self) -> Vec<String> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).lines.iter().cloned().collect()
    }

    /// Current buffered line count (<= its cap).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Monotonic count of lines ever pushed — lets a caller detect how many
    /// new lines arrived since it last checked, even across eviction.
    pub fn total_pushed(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).pushed
    }

    /// The last `n` buffered lines (fewer if the buffer holds less).
    pub fn tail(&self, n: usize) -> Vec<String> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let skip = inner.lines.len().saturating_sub(n);
        inner.lines.iter().skip(skip).cloned().collect()
    }

    /// Oldest-first buffered lines in `[lo, hi)`, clamped to what's buffered.
    pub fn range(&self, lo: usize, hi: usize) -> Vec<String> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let hi = hi.min(inner.lines.len());
        let lo = lo.min(hi);
        inner.lines.iter().skip(lo).take(hi - lo).cloned().collect()
    }
}

impl Default for LogBuf {
    fn default() -> Self {
        Self::new(200)
    }
}

