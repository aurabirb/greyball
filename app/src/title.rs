//! Terminal window title: a fixed `{icon} ` prefix (never scrolled — see
//! `player_state_glyph`) followed by `{Artist} - {Title}`, scrolled
//! marquee-style once that track text alone is wider than the remaining
//! budget of a classic 80-column terminal (the `scroll_title` windowing
//! itself lives in `ui`, shared with the tab bar's own marquee).
//!
//! Driven from `main.rs`'s event loop, right where it already reacts to
//! "track/playback state changed" once per iteration for the MPRIS/media-keys
//! plumbing — not a separate callback path. The visible window only advances
//! when the surrounding loop wakes, which happens at least every ~500ms
//! while something is actively playing (the player workers' `Progress`
//! ticks); it freezes while paused/stopped, which is fine — nothing's "now
//! playing" to scroll urgently about then.

use std::time::Instant;

use cursive::Cursive;
use medley_core::{PlayerState, Track};

/// Classic 80-column terminal width — a long-standing, widely-used
/// convention for "the usual max width" a terminal title is shown at.
const MAX_WIDTH: usize = 80;

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
        let full = ui::window_title_track_text(track);
        if full != self.full {
            self.full = full;
            self.scroll_start = Instant::now();
        }

        // The icon is a fixed prefix outside the scrolled window, so it's
        // always visible and never eats into the marquee's own timing.
        let prefix = match track {
            Some(_) => format!("{} ", ui::player_state_glyph(state)),
            None => String::new(),
        };
        let avail = MAX_WIDTH.saturating_sub(prefix.chars().count());

        let len = self.full.chars().count();
        let body = if len <= avail {
            self.full.clone()
        } else {
            let cycle_len = len + ui::SCROLL_GAP.chars().count();
            let offset = ui::marquee_offset(self.scroll_start.elapsed(), cycle_len);
            ui::scroll_title(&self.full, avail, offset)
        };
        let text = format!("{prefix}{body}");

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
