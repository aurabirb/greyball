use cursive::Printer;
use cursive::theme::ColorStyle;

use unicode_width::UnicodeWidthStr;

use core::{Command, PlayerState, Session};

use super::text::{in_span, ms, pad, scroll_title};
use super::transport::{NEXT_ICON, PREV_ICON, player_action_glyph};

/// The scrubber's width.
const BAR_WIDTH: usize = 24;

/// The player-status line's content for one frame, snapshotted from the session.
pub(super) struct StatusLine {
    /// "artist - title" of the playing track.
    pub(super) now_playing: String,
    pub(super) state: PlayerState,
    position_ms: u32,
    duration_ms: u32,
    bpm_tag: String,
    shuffle: bool,
}

/// Where the line's segments sit — `(start column, width)` each — for both drawing and hit-testing.
struct Layout {
    name_w: usize,
    prev: (usize, usize),
    playpause: (usize, usize),
    next: (usize, usize),
    scrubber: (usize, usize),
    bpm: (usize, usize),
    shuffle: (usize, usize),
}

impl StatusLine {
    pub(super) fn snapshot(s: &Session) -> Self {
        let track = s.now_playing();
        let st = s.player_status();
        Self {
            now_playing: track
                .as_ref()
                .map(|t| format!("{} - {}", t.display_artist(), t.title))
                .unwrap_or_else(|| "nothing playing".to_string()),
            state: st.state,
            position_ms: st.position_ms,
            duration_ms: st.duration_ms,
            bpm_tag: bpm_status_tag(s, track.as_ref()),
            shuffle: s.shuffle(),
        }
    }

    fn shuffle_tag(&self) -> &'static str {
        if self.shuffle { "[S]" } else { "[s]" }
    }

    fn layout(&self, total_w: usize) -> Layout {
        let gap = 2;
        let (prev_w, playpause_w, next_w) =
            (PREV_ICON.width(), player_action_glyph(&self.state).width(), NEXT_ICON.width());
        let (curtime_w, totaltime_w) = (ms(self.position_ms).width(), ms(self.duration_ms).width());
        let (bpm_w, shuffle_w) = (self.bpm_tag.width(), self.shuffle_tag().width());

        let prev = (0, prev_w);
        let playpause = (prev_w + 1, playpause_w);
        let next = (prev_w + 1 + playpause_w + 1, next_w);
        let title_start = next.0 + next_w + gap;
        let reserved = gap + curtime_w + 1 + BAR_WIDTH + 1 + totaltime_w + gap + bpm_w + 1 + shuffle_w;
        let name_w = total_w.saturating_sub(title_start).saturating_sub(reserved);

        let scrubber = (title_start + name_w + gap + curtime_w + 1, BAR_WIDTH);
        let bpm = (scrubber.0 + BAR_WIDTH + 1 + totaltime_w + gap, bpm_w);
        let shuffle = (bpm.0 + bpm_w + 1, shuffle_w);
        Layout { name_w, prev, playpause, next, scrubber, bpm, shuffle }
    }

    /// Draws the line across row 0 of `printer`; the track name scrolls by `marquee_offset` when it doesn't fit.
    pub(super) fn draw(&self, printer: &Printer, marquee_offset: usize) {
        let name_w = self.layout(printer.size.x).name_w;
        let status = format!(
            "{PREV_ICON} {} {NEXT_ICON}  {}  {} {} {}  {} {}",
            player_action_glyph(&self.state),
            pad(&scroll_title(&self.now_playing, name_w, marquee_offset), name_w),
            ms(self.position_ms),
            progress_bar(self.position_ms, self.duration_ms, BAR_WIDTH),
            ms(self.duration_ms),
            self.bpm_tag,
            self.shuffle_tag(),
        );
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, 0), &pad(&status, p.size.x));
        });
    }

    /// The command a left click at column `x` of a `total_w`-wide line stands for.
    pub(super) fn click(&self, x: usize, total_w: usize) -> Option<Command> {
        let layout = self.layout(total_w);
        if in_span(x, layout.playpause) {
            Some(Command::PlayPause)
        } else if in_span(x, layout.prev) {
            Some(Command::Previous)
        } else if in_span(x, layout.next) {
            Some(Command::Next)
        } else if in_span(x, layout.scrubber) && self.duration_ms > 0 {
            let frac = (x - layout.scrubber.0) as f64 / layout.scrubber.1 as f64;
            let target_ms = (frac * self.duration_ms as f64).round() as u32;
            Some(Command::Seek(target_ms as i64 - self.position_ms as i64))
        } else if in_span(x, layout.bpm) {
            Some(Command::ToggleScan)
        } else if in_span(x, layout.shuffle) {
            Some(Command::ToggleShuffle)
        } else {
            None
        }
    }
}

fn progress_bar(pos: u32, dur: u32, width: usize) -> String {
    if dur == 0 {
        return "-".repeat(width);
    }
    let filled = ((pos as f64 / dur as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "━".repeat(filled), "╍".repeat(width - filled))
}

/// Bracketed BPM-scan status tag shown next to the status line's scrubber.
fn bpm_status_tag(s: &Session, track: Option<&core::Track>) -> String {
    let Some(scan) = s.scan.as_ref() else {
        return "[bd]".to_string();
    };
    let mode_letter = match scan.mode() {
        core::ScanMode::Disabled => return "[bd]".to_string(),
        core::ScanMode::CacheOnly => 'b',
        core::ScanMode::Active => 'B',
    };
    // Purely a plugin-status indicator, never the resolved value itself.
    let status_letter = match track.and_then(|t| scan.status("bpm", t.id)) {
        Some(core::ScanStatus::Downloading) => 'd',
        Some(core::ScanStatus::Error) => 'e',
        Some(core::ScanStatus::Skipped) => 's',
        None => 'w',
    };
    format!("[{mode_letter}{status_letter}]")
}

/// Full, un-scrolled track text for the terminal window title.
pub fn window_title_track_text(track: Option<&core::Track>) -> String {
    match track {
        Some(t) => format!("{} - {}", t.display_artist(), t.title),
        None => "medley".to_string(),
    }
}
