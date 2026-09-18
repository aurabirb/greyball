use core::Session;

/// Width left for the status line's track-name field after the leading icon and the trailing block.
fn name_field_width(total_w: usize, prefix_w: usize, reserved_w: usize) -> usize {
    total_w.saturating_sub(prefix_w).saturating_sub(reserved_w)
}

/// The status line's scrubber width.
pub(super) const STATUS_BAR_WIDTH: usize = 24;

/// Click targets on the bottom status line — `(start column, width)` each, in screen columns.
pub(super) struct StatusLineLayout {
    pub(super) prev: (usize, usize),
    pub(super) playpause: (usize, usize),
    pub(super) next: (usize, usize),
    pub(super) scrubber: (usize, usize),
    pub(super) bpm: (usize, usize),
    pub(super) shuffle: (usize, usize),
}

/// Every segment's already-rendered display width, for [`status_line_layout`].
pub(super) struct StatusLineWidths {
    pub(super) prev: usize,
    pub(super) playpause: usize,
    pub(super) next: usize,
    pub(super) curtime: usize,
    pub(super) bar: usize,
    pub(super) totaltime: usize,
    pub(super) bpm: usize,
    pub(super) shuffle: usize,
}

/// Column layout for the status line.
pub(super) fn status_line_layout(total_w: usize, w: &StatusLineWidths) -> (usize, StatusLineLayout) {
    let gap = 2;
    let prev = (0, w.prev);
    let playpause = (w.prev + 1, w.playpause);
    let next = (w.prev + 1 + w.playpause + 1, w.next);
    let cluster_w = w.prev + 1 + w.playpause + 1 + w.next;
    let title_start = cluster_w + gap;
    let reserved =
        gap + w.curtime + 1 + w.bar + 1 + w.totaltime + gap + w.bpm + 1 + w.shuffle;
    let name_w = name_field_width(total_w, title_start, reserved);

    let mut x = title_start + name_w + gap;
    x += w.curtime + 1;
    let scrubber = (x, w.bar);
    x += w.bar + 1 + w.totaltime + gap;
    let bpm = (x, w.bpm);
    x += w.bpm + 1;
    let shuffle = (x, w.shuffle);
    (name_w, StatusLineLayout { prev, playpause, next, scrubber, bpm, shuffle })
}

pub(super) fn progress_bar(pos: u32, dur: u32, width: usize) -> String {
    if dur == 0 {
        return "-".repeat(width);
    }
    let filled = ((pos as f64 / dur as f64) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "━".repeat(filled), "╍".repeat(width - filled))
}

/// Bracketed BPM-scan status tag shown next to the status line's scrubber.
pub(super) fn bpm_status_tag(s: &Session, track: Option<&core::Track>) -> String {
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
