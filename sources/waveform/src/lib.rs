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
        let mut levels: Vec<f32> = Vec::new();
        let mut partial = vec![0.0_f32; BUCKETS];
        let mut filled = 0;
        let duration_ms = track.known_duration_ms() as u64;
        let (mut sum, mut n) = (0.0_f32, 0);
        let decoded = core::audio_decode::decode_blocks(&stream, |block, rate| {
            let total_windows = (duration_ms * rate as u64 / 1000 / WINDOW as u64).max(1) as usize;
            for [l, r] in block {
                sum += (l * l + r * r) * 0.5;
                n += 1;
                if n == WINDOW {
                    let level = (sum / WINDOW as f32).sqrt();
                    let bucket = (levels.len() * BUCKETS / total_windows).min(BUCKETS - 1);
                    partial[bucket] = partial[bucket].max(level);
                    levels.push(level);
                    (sum, n) = (0.0, 0);
                    if duration_ms > 0 && bucket > filled {
                        filled = bucket;
                        if let Some(prefix) = normalise(&partial[..filled]) {
                            waveform::publish_live(track.id, &prefix);
                        }
                    }
                }
            }
            wanted()
        });
        waveform::clear_live(track.id);
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
        Outcome::Done(TrackMeta { attrs: [(waveform::ATTR.to_string(), waveform::encode(&buckets))].into() })
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

/// `levels` max-reduced to `BUCKETS`, then `normalise`d.
fn envelope(levels: &[f32]) -> Option<Vec<u8>> {
    let buckets: Vec<f32> = (0..BUCKETS)
        .map(|i| {
            let lo = i * levels.len() / BUCKETS;
            let hi = ((i + 1) * levels.len() / BUCKETS).max(lo + 1).min(levels.len());
            levels[lo..hi].iter().copied().fold(0.0, f32::max)
        })
        .collect();
    normalise(&buckets)
}

/// `buckets` scaled so the loudest is 255; `None` for silence or no audio.
fn normalise(buckets: &[f32]) -> Option<Vec<u8>> {
    let peak = buckets.iter().copied().fold(0.0, f32::max);
    (peak > 0.0).then(|| buckets.iter().map(|v| (v / peak * 255.0).round() as u8).collect())
}
