//! Cheap 5-band magnitude estimate for a window of mono samples — real
//! signal, not a volume-slider proxy. No FFT crate: the Goertzel algorithm
//! evaluates a handful of target frequencies directly in one pass, which is
//! exactly what a 5-bar equalizer wants (a full spectrum would go mostly
//! unused). All 5 bands share the one pass over the window, each band's IIR
//! state in its own SIMD lane via `wide::f32x8`.
//!
//! Real program material's magnitude spectrum falls off with frequency
//! (pink-noise-like), so the raw Goertzel output is bass-dominated; a
//! per-band gain (see [`band_gain`]) compensates so the bands read as
//! balanced rather than the bass band swamping the rest.

use wide::f32x8;

/// Band center frequencies (Hz) — bass through treble.
const BAND_HZ: [f32; 5] = [60.0, 250.0, 1000.0, 4000.0, 12000.0];

/// Frequency-dependent compensation gain, relative to the bass band: a
/// gentle `(hz / BAND_HZ[0]).powf(0.3)` curve, monotonically increasing with
/// frequency to counter the natural bass-heavy falloff of real program
/// material, without boosting the highs so hard they read as noise flicker.
fn band_gain(hz: f32) -> f32 {
    (hz / BAND_HZ[0]).powf(0.3)
}

/// One magnitude per [`BAND_HZ`], `~0.0..=1.0` under typical program levels
/// (already includes [`band_gain`]'s per-band compensation; a loud transient
/// can still exceed 1.0 briefly — callers should clamp).
/// `samples` should be recent, mono, in `-1.0..=1.0`.
pub fn bands(samples: &[f32], sample_rate: u32) -> [f32; 5] {
    if samples.len() < 2 || sample_rate == 0 {
        return [0.0; 5];
    }
    let n = samples.len() as f32;
    let sr = sample_rate as f32;

    // Goertzel: pick the DFT bin nearest each target frequency, run its
    // recurrence over every sample, lanes 5..8 unused (left at 0).
    let mut omega_arr = [0.0f32; 8];
    for (slot, &hz) in omega_arr.iter_mut().zip(BAND_HZ.iter()) {
        let k = (0.5 + n * hz / sr).floor();
        *slot = 2.0 * std::f32::consts::PI * k / n;
    }
    let omega = f32x8::from(omega_arr);
    let coeff = f32x8::splat(2.0) * omega.cos();

    let mut s_prev = f32x8::splat(0.0);
    let mut s_prev2 = f32x8::splat(0.0);
    for &x in samples {
        let xv = f32x8::splat(x);
        let s = xv + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }

    let real = s_prev - s_prev2 * omega.cos();
    let imag = s_prev2 * omega.sin();
    let mag = (real * real + imag * imag).sqrt() / f32x8::splat(n);
    let mag_arr = mag.to_array();

    let mut out = [0.0f32; 5];
    for (o, (&mag, &hz)) in out.iter_mut().zip(mag_arr.iter().zip(BAND_HZ.iter())) {
        *o = mag * band_gain(hz);
    }
    out
}

