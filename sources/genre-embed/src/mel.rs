//! Mel-spectrogram front end matching Essentia's `TensorflowInputMusiCNN` preprocessing, which
//! discogs-effnet was trained on: 16kHz mono, 512-sample symmetric-Hann frames with a 256-sample
//! hop, magnitude spectrum, 96 Slaney-scale mel bands spanning 0-8kHz with unit-area triangles,
//! then `log10(1 + 10_000 * energy)` per band. Reimplemented from the model's own published specs
//! (clean-room, no GPL code) rather than any Python/native Essentia dependency.

use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex};

pub const RATE: u32 = 16_000;
pub const FRAME: usize = 512;
pub const HOP: usize = 256;
pub const BANDS: usize = 96;

/// Slaney mel-scale constants (librosa's `htk=False`): linear below `MIN_LOG_HZ`, logarithmic above.
const F_SP: f64 = 200.0 / 3.0;
const MIN_LOG_HZ: f64 = 1000.0;

fn min_log_mel() -> f64 {
    MIN_LOG_HZ / F_SP
}

fn logstep() -> f64 {
    6.4_f64.ln() / 27.0
}

/// Hz -> Slaney mel.
fn hz_to_slaney(f: f64) -> f64 {
    if f < MIN_LOG_HZ { f / F_SP } else { min_log_mel() + (f / MIN_LOG_HZ).ln() / logstep() }
}

/// Slaney mel -> Hz.
fn slaney_to_hz(m: f64) -> f64 {
    if m < min_log_mel() { m * F_SP } else { MIN_LOG_HZ * ((m - min_log_mel()) * logstep()).exp() }
}

/// One mel triangular filter as sparse `(fft_bin, weight)` pairs.
struct Filter(Vec<(usize, f32)>);

pub struct MelSpectrogram {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    filters: Vec<Filter>,
}

impl MelSpectrogram {
    pub fn new() -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(FRAME);
        let bins = FRAME / 2 + 1;
        let bin_hz = RATE as f64 / FRAME as f64;
        let (lo, hi) = (hz_to_slaney(0.0), hz_to_slaney(8000.0));
        let edges: Vec<f64> = (0..BANDS + 2).map(|i| slaney_to_hz(lo + (hi - lo) * i as f64 / (BANDS + 1) as f64)).collect();
        let filters = (0..BANDS)
            .map(|b| {
                let (l, c, r) = (edges[b], edges[b + 1], edges[b + 2]);
                // Slaney area normalization (matches `crate-digger`'s verified-working
                // reimplementation of this exact model's preprocessing), not unit-sum.
                let area_norm = 2.0 / (r - l);
                Filter(
                    (0..bins)
                        .filter_map(|k| {
                            let f = k as f64 * bin_hz;
                            let w = if f > l && f <= c {
                                (f - l) / (c - l)
                            } else if f > c && f < r {
                                (r - f) / (r - c)
                            } else {
                                0.0
                            };
                            (w > 0.0).then_some((k, (w * area_norm) as f32))
                        })
                        .collect(),
                )
            })
            .collect();
        // Symmetric Hann, matching Essentia's Windowing (not the periodic variant).
        let window = (0..FRAME).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (FRAME - 1) as f32).cos()).collect();
        Self { fft, window, filters }
    }

    /// Compute one `[BANDS]` row per `HOP`-spaced frame of `mono16k` (16kHz mono samples).
    pub fn frames(&self, mono16k: &[f32]) -> Vec<[f32; BANDS]> {
        let mut scratch = vec![Complex::<f32>::default(); FRAME];
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + FRAME <= mono16k.len() {
            for (i, s) in scratch.iter_mut().enumerate() {
                *s = Complex::new(mono16k[pos + i] * self.window[i], 0.0);
            }
            self.fft.process(&mut scratch);
            let mut row = [0.0_f32; BANDS];
            for (b, filt) in self.filters.iter().enumerate() {
                let e: f32 = filt.0.iter().map(|&(k, w)| scratch[k].norm() * w).sum();
                row[b] = (1.0 + 10_000.0 * e).log10();
            }
            out.push(row);
            pos += HOP;
        }
        out
    }
}

impl Default for MelSpectrogram {
    fn default() -> Self {
        Self::new()
    }
}
