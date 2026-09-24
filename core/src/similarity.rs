//! "Similar tracks" ranking: how close a candidate track is to a reference one.

use crate::types::Track;

/// BPM within this many beats counts as a close match.
const BPM_TOLERANCE: f32 = 6.0;

/// Subtracted from the BPM distance when both tracks have a `key` attr and it matches —
/// a tie-breaker only, never enough to outrank a track with a meaningfully closer BPM.
const KEY_MATCH_BONUS: f32 = 1.0;

/// How far `candidate` is from `reference`, lower is more similar; `None` when they aren't
/// comparable (no `bpm` on either side) or `candidate` is outside `BPM_TOLERANCE`. BPM is the
/// only signal with real data today; `key` (when both tracks happen to have one) only nudges the
/// ranking among close-BPM matches. A later vector-embedding signal replaces this function's body,
/// not its call sites.
pub fn similarity(reference: &Track, candidate: &Track) -> Option<f32> {
    let ref_bpm: f32 = reference.attrs.get("bpm")?.parse().ok()?;
    let cand_bpm: f32 = candidate.attrs.get("bpm")?.parse().ok()?;
    if !ref_bpm.is_finite() || !cand_bpm.is_finite() {
        return None;
    }
    let bpm_distance = (ref_bpm - cand_bpm).abs();
    if bpm_distance > BPM_TOLERANCE {
        return None;
    }
    let key_bonus = match (reference.attrs.get("key"), candidate.attrs.get("key")) {
        (Some(a), Some(b)) if a == b => KEY_MATCH_BONUS,
        _ => 0.0,
    };
    Some((bpm_distance - key_bonus).max(0.0))
}
