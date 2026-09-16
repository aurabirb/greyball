//! Shared audio-file heuristics. Lifted out of `sources/http` so `core`'s
//! playlist import (`playlist_m3u` synthesis) and the http source agree on
//! what counts as an audio file and how lossy/lossless it is. Pure string
//! work — no I/O, no source knowledge.

use crate::types::Quality;

/// Case-insensitive audio extensions we recognise.
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "m4a", "aac", "wav", "webm", "aiff", "wv",
];
/// The subset that is (typically) lossless.
pub const LOSSLESS_EXTS: &[&str] = &["flac", "wav", "aiff", "wv"];

/// The lower-cased extension of a file name / last URL path segment, if any.
fn ext_of(name: &str) -> Option<String> {
    let seg = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (_, ext) = seg.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// True if `name` ends in a known audio extension.
pub fn audio_ext(name: &str) -> bool {
    ext_of(name).is_some_and(|e| AUDIO_EXTS.contains(&e.as_str()))
}

/// Lossless/lossy classifier by extension (bits/hz unknown here). Non-audio or
/// unknown extensions classify as lossy — same rule the http source used.
pub fn audio_quality(name: &str) -> Quality {
    match ext_of(name) {
        Some(e) if LOSSLESS_EXTS.contains(&e.as_str()) => Quality::Lossless {
            bits: None,
            hz: None,
        },
        _ => Quality::Lossy { kbps: None },
    }
}
