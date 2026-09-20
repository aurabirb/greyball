//! Identity / dedupe.

use unicode_normalization::UnicodeNormalization;

use crate::types::{LinkReason, Track};

pub struct Matcher;

impl Matcher {
    /// Normalize for comparison: lowercase, NFKD, drop diacritics, remove
    /// content in (), [], strip "feat."/"ft." tails, collapse non-alphanumerics
    /// to single spaces, trim.
    pub fn norm(s: &str) -> String {
        // lowercase + NFKD + drop combining marks (diacritics)
        let decomposed: String = s
            .to_lowercase()
            .nfkd()
            .filter(|c| !is_combining_mark(*c))
            .collect();

        // remove bracketed content
        let mut out = String::with_capacity(decomposed.len());
        let mut depth_paren = 0i32;
        let mut depth_brack = 0i32;
        for c in decomposed.chars() {
            match c {
                '(' => depth_paren += 1,
                ')' => {
                    if depth_paren > 0 {
                        depth_paren -= 1
                    }
                }
                '[' => depth_brack += 1,
                ']' => {
                    if depth_brack > 0 {
                        depth_brack -= 1
                    }
                }
                _ if depth_paren > 0 || depth_brack > 0 => {}
                _ => out.push(c),
            }
        }

        // strip "feat." / "ft." tails
        for marker in [" feat.", " feat ", " ft.", " ft ", " featuring "] {
            if let Some(idx) = out.find(marker) {
                out.truncate(idx);
            }
        }

        // collapse non-alphanumerics to single spaces
        let mut collapsed = String::with_capacity(out.len());
        let mut prev_space = false;
        for c in out.chars() {
            if c.is_alphanumeric() {
                collapsed.push(c);
                prev_space = false;
            } else if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        }

        collapsed.trim().to_string()
    }

    /// Does `hit` refer to the same recording as `track`?
    pub fn matches(track: &Track, hit: &Track) -> Option<LinkReason> {
        // 1. ISRC exact match
        if let (Some(a), Some(b)) = (&track.isrc, &hit.isrc) {
            if a.eq_ignore_ascii_case(b) {
                return Some(LinkReason::Isrc);
            }
            return None;
        }

        // 2. fuzzy title + artist + duration
        if Self::norm(&track.title) != Self::norm(&hit.title) {
            return None;
        }

        let artist_ok = match (track.artists.first(), hit.artists.first()) {
            (Some(a), Some(b)) => Self::norm(a) == Self::norm(b),
            _ => true, // either side has no artist -> wildcard
        };
        if !artist_ok {
            return None;
        }

        let dur_ok = track.duration_ms == 0
            || hit.duration_ms == 0
            || track.duration_ms.abs_diff(hit.duration_ms) <= 3000;
        if !dur_ok {
            return None;
        }

        Some(LinkReason::Fuzzy)
    }
}

fn is_combining_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}
