use std::sync::Arc;

use cursive::Printer;
use cursive::theme::ColorStyle;

use unicode_width::UnicodeWidthStr;

use core::{Command, PlayerState, Session, TrackId, waveform};

use crate::screen::Corners;

use super::text::{in_span, ms, pad, scroll_title};
use super::transport::{NEXT_ICON, PREV_ICON, player_action_glyph};

/// The scrubber's width.
const BAR_WIDTH: usize = 24;

/// The revision-cacheable part of the status line — position/duration/bpm are read live instead.
pub(super) struct StatusCore {
    now_playing: String,
    now_playing_id: Option<TrackId>,
    /// Empty until the track has been scanned.
    waveform: Arc<[u8]>,
    state: PlayerState,
    shuffle: bool,
}

impl StatusCore {
    pub(super) fn snapshot(s: &Session) -> Self {
        let track = s.now_playing();
        Self {
            now_playing: track
                .as_ref()
                .map(|t| format!("{} - {}", t.display_artist(), t.title))
                .unwrap_or_else(|| "nothing playing".to_string()),
            now_playing_id: s.now_playing_id(),
            waveform: track.and_then(|t| t.attrs.get(waveform::ATTR).map(|hex| waveform::decode(hex))).unwrap_or_default().into(),
            state: s.player_status().state,
            shuffle: s.shuffle(),
        }
    }

    pub(super) fn now_playing_id(&self) -> Option<TrackId> {
        self.now_playing_id
    }
}

/// One frame's status line: cached `StatusCore` plus live position/duration/bpm.
pub(super) struct StatusLine {
    /// "artist - title" of the playing track.
    pub(super) now_playing: String,
    pub(super) now_playing_id: Option<TrackId>,
    pub(super) waveform: Arc<[u8]>,
    pub(super) state: PlayerState,
    pub(super) position_ms: u32,
    pub(super) duration_ms: u32,
    pub(super) bpm_tag: ScanTag,
    pub(super) shuffle: bool,
    /// The playing track's `Session::liked_mark`.
    pub(super) liked: Option<bool>,
}

/// Where the line's segments sit — `(start column, width)` each — for both drawing and hit-testing.
struct Layout {
    name_w: usize,
    prev: (usize, usize),
    playpause: (usize, usize),
    next: (usize, usize),
    scrubber: (usize, usize),
}

impl StatusLine {
    pub(super) const CORNERS: Corners = Corners::RIGHT;

    pub(super) fn snapshot(s: &Session) -> Self {
        let core = StatusCore::snapshot(s);
        let st = s.player_status();
        let bpm_tag = bpm_status_tag(s, core.now_playing_id);
        let liked = core.now_playing_id.and_then(|id| s.liked_mark(id));
        Self::assemble(&core, st.position_ms, st.duration_ms, bpm_tag, liked)
    }

    /// Combines a cached `StatusCore` with this frame's freshly-read per-tick data.
    pub(super) fn assemble(core: &StatusCore, position_ms: u32, duration_ms: u32, bpm_tag: ScanTag, liked: Option<bool>) -> Self {
        Self {
            now_playing: core.now_playing.clone(),
            now_playing_id: core.now_playing_id,
            waveform: core.waveform.clone(),
            state: core.state,
            position_ms,
            duration_ms,
            bpm_tag,
            shuffle: core.shuffle,
            liked,
        }
    }

    /// Unknown length (nothing loaded yet, or a decoder that can't tell) reads as such, not 0:00.
    fn total_time(&self) -> String {
        if self.duration_ms == 0 { "?:??".to_string() } else { ms(self.duration_ms) }
    }

    fn layout(&self, total_w: usize) -> Layout {
        let gap = 2;
        let (prev_w, playpause_w, next_w) =
            (PREV_ICON.width(), player_action_glyph(&self.state).width(), NEXT_ICON.width());
        let (curtime_w, totaltime_w) = (ms(self.position_ms).width(), self.total_time().width());

        let prev = (0, prev_w);
        let playpause = (prev_w + 1, playpause_w);
        let next = (prev_w + 1 + playpause_w + 1, next_w);
        let title_start = next.0 + next_w + gap;
        let reserved = gap + curtime_w + 1 + BAR_WIDTH + 1 + totaltime_w;
        let name_w = total_w.saturating_sub(title_start).saturating_sub(reserved);

        let scrubber = (title_start + name_w + gap + curtime_w + 1, BAR_WIDTH);
        Layout { name_w, prev, playpause, next, scrubber }
    }

    /// Draws the line across row 0 of `printer`; the track name scrolls by `marquee_offset` when it doesn't fit.
    pub(super) fn draw(&self, printer: &Printer, marquee_offset: usize) {
        let name_w = self.layout(printer.size.x).name_w;
        let status = format!(
            "{PREV_ICON} {} {NEXT_ICON}  {}  {} {} {}",
            player_action_glyph(&self.state),
            pad(&scroll_title(&self.now_playing, name_w, marquee_offset), name_w),
            ms(self.position_ms),
            progress_bar(self.position_ms, self.duration_ms, BAR_WIDTH),
            self.total_time(),
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
    format!("{}{}", "=".repeat(filled), "-".repeat(width - filled))
}

/// The analyzer tag is one letter.
pub(super) const SCAN_TAG_W: usize = 1;

/// What the now-playing track's analysis is doing, shown as the tag's background.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanLight {
    Idle,
    Working,
    Errored,
}

/// `B` while scanning may fetch, `b` when cache-only or off.
pub(super) struct ScanTag {
    pub(super) letter: char,
    pub(super) light: ScanLight,
}

/// The analyzer tag — read fresh every frame, never cached.
pub(super) fn bpm_status_tag(s: &Session, now_playing: Option<TrackId>) -> ScanTag {
    let idle = ScanTag { letter: 'b', light: ScanLight::Idle };
    let Some(scan) = s.scan.as_ref() else { return idle };
    let letter = match scan.mode() {
        core::ScanMode::Disabled => return idle,
        core::ScanMode::CacheOnly => 'b',
        core::ScanMode::Active => 'B',
    };
    let light = match now_playing.and_then(|id| scan.status("bpm", id)) {
        Some(core::ScanStatus::Downloading) => ScanLight::Working,
        Some(core::ScanStatus::Error) => ScanLight::Errored,
        Some(core::ScanStatus::Skipped) | None => ScanLight::Idle,
    };
    ScanTag { letter, light }
}


/// Full, un-scrolled track text for the terminal window title.
pub fn window_title_track_text(track: Option<&core::Track>) -> String {
    match track {
        Some(t) => format!("{} - {}", t.display_artist(), t.title),
        None => "medley".to_string(),
    }
}
