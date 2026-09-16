//! Terminal window title: `{icon} {Artist} - {Title}`, scrolled marquee-style
//! once the full text is wider than a classic 80-column terminal (the
//! `scroll_title` windowing itself lives in `ui`, shared with the status
//! line's icon logic).
//!
//! Driven from `main.rs`'s event loop, right where it already reacts to
//! "track/playback state changed" once per iteration for the MPRIS/media-keys
//! plumbing — not a separate callback path. The visible window only advances
//! when the surrounding loop wakes, which happens at least every ~500ms
//! while something is actively playing (the player workers' `Progress`
//! ticks); it freezes while paused/stopped, which is fine — nothing's "now
//! playing" to scroll urgently about then.

use std::time::{Duration, Instant};

use cursive::Cursive;
use medley_core::{PlayerState, Track};

/// Classic 80-column terminal width — a long-standing, widely-used
/// convention for "the usual max width" a terminal title is shown at.
const MAX_WIDTH: usize = 80;

/// How many seconds' worth of real time correspond to one column of scroll.
const SECS_PER_STEP: u64 = 1;

/// Offset into a `cycle_len`-long looping text after `elapsed` real time,
/// advancing one step every [`SECS_PER_STEP`] seconds. Pure wall-clock math
/// (not a per-loop-iteration counter), so the scroll speed stays correct
/// regardless of how often the surrounding loop happens to wake.
fn scroll_offset(elapsed: Duration, cycle_len: usize) -> usize {
    if cycle_len == 0 {
        return 0;
    }
    ((elapsed.as_secs() / SECS_PER_STEP) as usize) % cycle_len
}

/// Stateful driver: tracks the full title text and when its scroll started,
/// and only calls `Cursive::set_window_title` when the rendered text
/// actually changes (never every loop iteration with the same value).
pub struct WindowTitle {
    full: String,
    scroll_start: Instant,
    last_set: Option<String>,
}

impl WindowTitle {
    pub fn new() -> Self {
        Self { full: String::new(), scroll_start: Instant::now(), last_set: None }
    }

    /// Recompute the title for the current `track`/`state` and push it to
    /// `siv` if it differs from what was last set there.
    pub fn update(&mut self, siv: &mut Cursive, track: Option<&Track>, state: &PlayerState) {
        let full = ui::window_title_text(track, state);
        if full != self.full {
            self.full = full;
            self.scroll_start = Instant::now();
        }

        let len = self.full.chars().count();
        let text = if len <= MAX_WIDTH {
            self.full.clone()
        } else {
            let cycle_len = len + ui::SCROLL_GAP.chars().count();
            let offset = scroll_offset(self.scroll_start.elapsed(), cycle_len);
            ui::scroll_title(&self.full, MAX_WIDTH, offset)
        };

        if self.last_set.as_deref() != Some(text.as_str()) {
            siv.set_window_title(text.clone());
            self.last_set = Some(text);
        }
    }
}

impl Default for WindowTitle {
    fn default() -> Self {
        Self::new()
    }
}
