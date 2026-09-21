//! `WaveformPlugin`: reduces a whole track to a fixed-length amplitude envelope; implements `core::ScanPlugin`.

use std::time::Duration;

use core::audio_decode::DecodeError;
use core::waveform::BUCKETS;
use core::{Outcome, ScanPlugin, StreamHandle, Track, TrackMeta, waveform};

/// Frames per RMS window before the windows are max-reduced into buckets.
const WINDOW: usize = 1024;

pub struct WaveformPlugin {
    min_interval: Duration,
}

impl WaveformPlugin {
    pub fn new(min_interval_secs: u64) -> Self {
        Self { min_interval: Duration::from_secs(min_interval_secs) }
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
        !track.attrs.contains_key(waveform::ATTR)
    }

    fn analyze(&self, track: &Track, audio: &dyn Fn() -> Result<StreamHandle, Outcome>, wanted: &dyn Fn() -> bool) -> Outcome {
        let stream = match audio() {
            Ok(s) => s,
            Err(outcome) => return outcome,
        };
        let duration_ms = track.known_duration_ms() as u64;
        let mut live = (duration_ms > 0).then(|| LiveGuard(track.id));
        let mut levels: Vec<f32> = Vec::new();
        let mut partial = vec![0.0_f32; BUCKETS];
        let mut filled = 0;
        let mut total_windows = 1;
        let (mut sum, mut n) = (0.0_f32, 0);
        let decoded = core::audio_decode::decode_blocks(&stream, |block, rate| {
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
                    if live.is_some() && bucket > filled {
                        filled = bucket;
                        if let Some(prefix) = normalise(&partial[..filled]) {
                            waveform::publish_live(track.id, &prefix);
                        }
                    }
                }
            }
            wanted()
        });
        if decoded == Err(DecodeError::Interrupted) || !wanted() {
            return Outcome::Retry;
        }
        if n > 0 {
            levels.push((sum / n as f32).sqrt());
        }
        let Some(buckets) = envelope(&levels) else {
            log::debug!("waveform: \"{}\" — nothing to draw, skipping", track.title);
            return Outcome::Skip;
        };
        if let Some(guard) = live.take() {
            // Stays until the persisted attr supersedes it, so the bar never blanks between the two.
            std::mem::forget(guard);
            waveform::publish_live(track.id, &buckets);
        }
        Outcome::Done(TrackMeta { attrs: [(waveform::ATTR.to_string(), waveform::encode(&buckets))].into() })
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

/// Clears the track's live envelope when dropped.
struct LiveGuard(core::TrackId);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        waveform::clear_live(self.0);
    }
}

/// The bucket window `index` of `total` falls in.
fn bucket_of(index: usize, total: usize) -> usize {
    (index * BUCKETS / total).min(BUCKETS - 1)
}

/// `levels` max-reduced to `BUCKETS` with the live placement, empty buckets taking the nearest level, then `normalise`d.
fn envelope(levels: &[f32]) -> Option<Vec<u8>> {
    let mut buckets = vec![0.0_f32; BUCKETS];
    for (i, &level) in levels.iter().enumerate() {
        let b = bucket_of(i, levels.len());
        buckets[b] = buckets[b].max(level);
    }
    if levels.len() < BUCKETS {
        for (i, b) in buckets.iter_mut().enumerate() {
            *b = levels[(i * levels.len() / BUCKETS).min(levels.len().saturating_sub(1))..].first().copied().unwrap_or(0.0);
        }
    }
    normalise(&buckets)
}

/// `buckets` scaled so the loudest is 255; `None` for silence or no audio.
fn normalise(buckets: &[f32]) -> Option<Vec<u8>> {
    let peak = buckets.iter().copied().fold(0.0, f32::max);
    (peak > 0.0).then(|| buckets.iter().map(|v| (v / peak * 255.0).round() as u8).collect())
}
