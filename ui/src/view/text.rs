use std::time::Duration;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Left-aligns `s` in a `width`-column field by display width, so wide codepoints don't shift what follows.
pub(crate) fn pad(s: &str, width: usize) -> String {
    let mut s = truncate(s, width);
    let w = s.width();
    if w < width {
        s.extend(std::iter::repeat_n(' ', width - w));
    }
    s
}

/// Right-aligns `s` in a `width`-column field (display-width-aware, see `pad`).
pub(super) fn pad_right_aligned(s: &str, width: usize) -> String {
    let s = truncate(s, width);
    let w = s.width();
    if w < width { " ".repeat(width - w) + &s } else { s }
}

/// Strips U+FE0E/U+FE0F variation selectors.
pub(super) fn strip_variation_selectors(s: &str) -> String {
    s.chars().filter(|&c| c != '\u{FE0E}' && c != '\u{FE0F}').collect()
}

/// `truncate`, but marks a cut with a trailing `…` instead of silently dropping the rest.
pub(super) fn truncate_ellipsis(s: &str, width: usize) -> String {
    let s = strip_variation_selectors(s);
    if s.width() <= width {
        return s;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = truncate(&s, width - 1);
    out.push('…');
    out
}

/// Truncates `s` to at most `width` terminal display columns (not chars).
pub(super) fn truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in strip_variation_selectors(s).chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Greedy word-wrap into `<= width`-column segments, hard-breaking a word longer than `width`.
pub(super) fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let mut chars: Vec<char> = word.chars().collect();
        loop {
            let sep = usize::from(!cur.is_empty());
            if cur.chars().count() + sep + chars.len() <= width {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.extend(chars.iter());
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                continue; // retry the same word against a fresh line
            }
            // the word alone is longer than `width` — hard-break it.
            let take = width.min(chars.len());
            let rest = chars.split_off(take);
            lines.push(chars.into_iter().collect());
            chars = rest;
            if chars.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Separator inserted between loop repeats by [`scroll_title`].
pub const SCROLL_GAP: &str = "   ";

/// Offset into a `cycle_len`-long looping [`scroll_title`] after `elapsed`, one column per second.
pub fn marquee_offset(elapsed: Duration, cycle_len: usize) -> usize {
    if cycle_len == 0 {
        return 0;
    }
    (elapsed.as_secs() as usize) % cycle_len
}

/// A marquee-style `width`-character window over `full`, sliding by one character per unit of `offset`.
pub fn scroll_title(full: &str, width: usize, offset: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = full.chars().collect();
    let len = chars.len();
    if len <= width {
        return full.to_string();
    }

    let gap: Vec<char> = SCROLL_GAP.chars().collect();
    let cycle_len = len + gap.len();
    let start = offset % cycle_len;

    let mut window: Vec<char> = Vec::with_capacity(width);
    for i in 0..width {
        let pos = (start + i) % cycle_len;
        window.push(if pos < len { chars[pos] } else { gap[pos - len] });
    }

    // Ellipsis at an edge only when that edge sits strictly inside `full` itself.
    if start < len && start > 0 {
        window[0] = '…';
    }
    let end = (start + width - 1) % cycle_len;
    if end < len.saturating_sub(1) {
        window[width - 1] = '…';
    }
    window.into_iter().collect()
}

/// `true` if column `x` falls inside `(start, width)`.
pub(super) fn in_span(x: usize, (start, width): (usize, usize)) -> bool {
    x >= start && x < start + width
}

pub(super) fn ms(ms: u32) -> String {
    let total = ms / 1000;
    format!("{}:{:02}", total / 60, total % 60)
}
