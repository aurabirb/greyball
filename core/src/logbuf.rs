//! `LogBuf` — a small in-memory ring of recent log lines, so `ui` can render a
//! tail of them (the Log pane) without depending on the `log`/file-logging
//! machinery that lives in `app`. Pure data; nothing here touches `log::Log` —
//! `app` pushes formatted lines in, `ui` reads a snapshot out.

use std::collections::VecDeque;
use std::sync::Mutex;

pub struct LogBuf {
    cap: usize,
    lines: Mutex<VecDeque<String>>,
}

impl LogBuf {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            lines: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    /// Append one line, dropping the oldest once `cap` is exceeded.
    pub fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= self.cap {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Oldest-first snapshot of everything currently buffered.
    pub fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

impl Default for LogBuf {
    fn default() -> Self {
        Self::new(200)
    }
}

