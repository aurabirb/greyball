//! `WaveformPlugin`: reduces a whole track to a fixed-length amplitude envelope; implements `core::ScanPlugin`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use core::audio_decode::DecodeError;
use core::waveform::{BUCKETS, bucket_of, envelope, normalise};
use core::{Outcome, ScanPlugin, StreamHandle, StreamState, Track, waveform};

/// Frames per RMS window before the windows are max-reduced into buckets.
const WINDOW: usize = 1024;

/// Buckets between live publishes, so the bar advances in 20 chunks.
const LIVE_STEP: usize = BUCKETS / 20;

/// Fragments sampled across a fully-cached track instead of one full linear decode — fixed count
/// so longer tracks (where the savings matter most) skip proportionally more of themselves.
const FRAGMENTS: usize = 60;

/// Decoded window length per fragment.
const FRAGMENT_WINDOW: Duration = Duration::from_millis(1500);

pub struct WaveformPlugin {
    min_interval: Duration,
    /// Settings-toggled: gates this decode-based plugin only. SoundCloud's
    /// own API-provided waveform plugin doesn't decode, so it never checks this.
    enabled: Arc<AtomicBool>,
}

impl WaveformPlugin {
    pub fn new(min_interval_secs: u64, enabled: Arc<AtomicBool>) -> Self {
        Self { min_interval: Duration::from_secs(min_interval_secs), enabled }
    }
}

impl ScanPlugin for WaveformPlugin {
    fn id(&self) -> &'static str {
        "waveform"
    }

    fn name(&self) -> &'static str {
        "Waveform"
    }

    fn needs(&self, track: &Track) -> bool {
        self.enabled.load(Ordering::Relaxed) && !track.attrs.contains_key(waveform::ATTR)
    }

    fn analyze(&self, track: &Track, audio: &dyn Fn() -> Result<StreamHandle, Outcome>, wanted: &dyn Fn() -> bool) -> Outcome {
        let outcome = self.decode(track, audio, wanted);
        if !matches!(outcome, Outcome::Done(_)) {
            waveform::clear_live(track.id);
        }
        outcome
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

impl WaveformPlugin {
    fn decode(&self, track: &Track, audio: &dyn Fn() -> Result<StreamHandle, Outcome>, wanted: &dyn Fn() -> bool) -> Outcome {
        let stream = match audio() {
            Ok(s) => s,
            Err(outcome) => return outcome,
        };
        let duration_ms = track.known_duration_ms() as u64;
        let live = duration_ms > 0;

        // Seeking is only cheap once the whole file is local; a still-downloading stream falls back
        // to the full linear decode it always used.
        let (levels, decoded) = if live && stream.info().state == StreamState::Done {
            self.decode_fragments(track, &stream, duration_ms, wanted)
        } else {
            self.decode_linear(track, &stream, duration_ms, live, wanted)
        };

        if decoded == Err(DecodeError::Interrupted) || !wanted() {
            return Outcome::Retry;
        }
        let Some(buckets) = envelope(&levels) else {
            log::debug!("waveform: \"{}\" — nothing to draw, skipping", track.title);
            return Outcome::Skip;
        };
        if live {
            // The status line clears it once the persisted attr lands, so the bar never blanks between the two.
            waveform::publish_live(track.id, &buckets);
        }
        Outcome::Done(waveform::meta(&buckets))
    }

    /// Full linear decode: RMS per `WINDOW` frames, max-reduced into buckets as they're decoded,
    /// with a left-to-right live preview every `LIVE_STEP` buckets.
    fn decode_linear(
        &self,
        track: &Track,
        stream: &StreamHandle,
        duration_ms: u64,
        live: bool,
        wanted: &dyn Fn() -> bool,
    ) -> (Vec<f32>, Result<u32, DecodeError>) {
        let mut levels: Vec<f32> = Vec::new();
        let mut partial = vec![0.0_f32; BUCKETS];
        let mut filled = 0;
        let mut total_windows = 1;
        let (mut sum, mut n) = (0.0_f32, 0);
        let decoded = core::audio_decode::decode_blocks(stream, |block, rate| {
            if levels.is_empty() {
                total_windows = (duration_ms * rate as u64 / 1000 / WINDOW as u64).max(1) as usize;
            }
            for [l, r] in block {
                sum += (l * l + r * r) * 0.5;
                n += 1;
                if n == WINDOW {
                    let level = (sum / WINDOW as f32).sqrt();
                    let bucket = bucket_of(levels.len(), total_windows);
                    partial[bucket] = partial[bucket].max(level);
                    levels.push(level);
                    (sum, n) = (0.0, 0);
                    if live && bucket >= filled + LIVE_STEP {
                        filled = bucket;
                        if let Some(prefix) = normalise(&partial[..filled]) {
                            waveform::publish_live(track.id, &prefix);
                        }
                    }
                }
            }
            wanted()
        });
        if n > 0 {
            levels.push((sum / n as f32).sqrt());
        }
        (levels, decoded)
    }

    /// Sparse-fragment decode: seeks to `FRAGMENTS` positions spread across the track and decodes a
    /// short window at each, using that window's RMS to stand in for its whole bucket range — the
    /// gap between fragments is left at the nearest sampled fragment's value (`envelope`'s stretch
    /// path, the same one used for tracks shorter than `BUCKETS` windows). Only reached once the
    /// stream is fully cached, so seeking is cheap (measured: tens of microseconds per seek on real
    /// VBR MP3s, fragment decode totalling ~10-20% of a full linear decode).
    fn decode_fragments(&self, track: &Track, stream: &StreamHandle, duration_ms: u64, wanted: &dyn Fn() -> bool) -> (Vec<f32>, Result<u32, DecodeError>) {
        let total_secs = duration_ms as f64 / 1000.0;
        let window_secs = FRAGMENT_WINDOW.as_secs_f64();
        let n = FRAGMENTS.min((total_secs / window_secs).floor().max(1.0) as usize).max(1);
        let span = (total_secs - window_secs).max(0.0);
        let positions: Vec<Duration> = (0..n).map(|i| Duration::from_secs_f64(span * i as f64 / n as f64)).collect();

        // `None` until `on_fragment` actually runs for that index — a failed seek (see
        // `core::audio_decode::decode_fragments`) skips the callback, so this stays `None` rather
        // than silently reading as measured silence.
        let mut levels: Vec<Option<f32>> = vec![None; n];
        // Each fragment's own bucket range, filled as fragments land, so the live preview grows
        // left-to-right like `decode_linear`'s instead of re-stretching the whole width every publish.
        // Kept as `Option` too (not `f32`) so a failed seek's still-empty range doesn't publish as
        // fake silence before the final gap-fill runs — see `fill_gaps`.
        let mut partial: Vec<Option<f32>> = vec![None; BUCKETS];
        let live_chunk = (n / 20).max(1);
        let decoded = core::audio_decode::decode_fragments(stream, &positions, FRAGMENT_WINDOW, |i, block, _rate| {
            let (sum, count) = block.iter().fold((0.0_f32, 0usize), |(s, c), [l, r]| (s + (l * l + r * r) * 0.5, c + 1));
            let level = if count > 0 { (sum / count as f32).sqrt() } else { 0.0 };
            levels[i] = Some(level);

            let start = bucket_of(i, n);
            let end = if i + 1 == n { BUCKETS } else { bucket_of(i + 1, n) };
            partial[start..end].fill(Some(level));

            if (i + 1) % live_chunk == 0 || i + 1 == n {
                // Gap-fill restricted to the reached prefix: the unreached tail must stay `None`/excluded
                // from `normalise`'s scaling, only a within-prefix gap (a failed seek) gets filled.
                let filled_prefix = fill_gaps(&partial[..end]);
                if let Some(prefix) = normalise(&filled_prefix) {
                    waveform::publish_live(track.id, &prefix);
                }
            }
            wanted()
        });
        (fill_gaps(&levels), decoded)
    }
}

/// Fills seek-failure gaps (`None`, where `on_fragment` was never called for that index) from the
/// nearest successfully-decoded neighbour within the given slice, so a failed seek never renders as
/// fake silence (`0.0`) — indistinguishable from genuinely quiet audio. Shared by the final persisted
/// result and each live-preview prefix during the scan.
fn fill_gaps(levels: &[Option<f32>]) -> Vec<f32> {
    levels
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.or_else(|| levels[..i].iter().rev().find_map(|v| *v)).or_else(|| levels[i + 1..].iter().find_map(|v| *v)).unwrap_or(0.0)
        })
        .collect()
}
