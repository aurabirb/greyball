//! Clean-room windowed-sinc resampler (no code taken from any GPL library), matching the
//! clean-room DSP ethos already documented in `sources/bpm/src/lib.rs`. Correctness matters here
//! more than in most of this codebase's DSP: a wrong sample rate silently corrupts every
//! downstream embedding, so this includes an anti-aliasing low-pass built into the kernel rather
//! than plain linear interpolation.

/// Half-width, in input samples, of the truncated sinc kernel. Higher = better stopband
/// rejection at the cost of more multiplies per output sample.
const HALF_TAPS: i64 = 16;

/// Resample `input` (mono) from `from_rate` to `to_rate` Hz via windowed-sinc interpolation.
/// The sinc cutoff is set at the lower of the two Nyquist frequencies, so downsampling
/// (the common case here: mixdown rate -> 16kHz) filters out content that would otherwise alias.
pub fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    // Cutoff in cycles/input-sample: 0.5 is input Nyquist, `ratio * 0.5` is output Nyquist.
    let cutoff = 0.5_f64.min(ratio * 0.5);
    let out_len = ((input.len() as f64) * ratio).round() as usize;

    (0..out_len)
        .map(|i| {
            let src_pos = i as f64 / ratio;
            let center = src_pos.floor() as i64;
            let mut acc = 0.0_f64;
            let mut gain = 0.0_f64;
            for k in -HALF_TAPS..=HALF_TAPS {
                let idx = center + k;
                if idx < 0 || idx as usize >= input.len() {
                    continue;
                }
                let n = src_pos - idx as f64;
                let h = sinc_lowpass(n, cutoff);
                let w = hann(k as f64, HALF_TAPS as f64);
                let tap = h * w;
                acc += input[idx as usize] as f64 * tap;
                gain += tap;
            }
            (if gain.abs() > 1e-12 { acc / gain } else { 0.0 }) as f32
        })
        .collect()
}

/// Ideal low-pass impulse response at cutoff `fc` (cycles/sample), evaluated at offset `n`.
fn sinc_lowpass(n: f64, fc: f64) -> f64 {
    if n.abs() < 1e-9 {
        2.0 * fc
    } else {
        (2.0 * std::f64::consts::PI * fc * n).sin() / (std::f64::consts::PI * n)
    }
}

/// Hann window over `[-half, half]`, evaluated at `k`.
fn hann(k: f64, half: f64) -> f64 {
    0.5 + 0.5 * (std::f64::consts::PI * k / half).cos()
}
