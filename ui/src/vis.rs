//! `:vis` pane. The color field is computed off the UI thread by a small
//! background worker, so scrolling/typing/etc. never blocks on it — `draw`
//! just blits whatever frame the worker last published. The view is
//! responsible for calling `siv.set_fps(_)` while the pane is open, so
//! cursive's own event loop redraws it periodically.
//!
//! A real 5-band equalizer — `Session::audio_levels` (`Player::levels`, real
//! Goertzel magnitude on the live audio tap, not a volume proxy) drives both
//! how tall each bar swings and, via its own loudness, how fast the
//! left-to-right rainbow gradient drifts. Runs best-effort [`FPS`].
//!
//! Frame-dropping is inherent, not a queue we prune: the worker is one
//! thread computing one frame at a time in a loop — if a tick takes longer
//! than its budget, the next tick just starts late, it never backs up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cursive::{
    Printer,
    theme::{Color, ColorStyle},
};

use crate::SessionHandle;

/// Frames per second cursive redraws at (and the worker recomputes at) —
/// "30fps or best effort" per this visualizer's simplicity.
pub const FPS: u32 = 30;
const N_BARS: usize = 5;

#[derive(Default)]
struct Frame {
    w: usize,
    h: usize,
    /// Row-major, `w * h` long: (r, g, b, ascii char).
    cells: Vec<(u8, u8, u8, u8)>,
}

pub struct Vis {
    enabled: AtomicBool,
    size: Mutex<(usize, usize)>,
    frame: Mutex<Frame>,
}

impl Vis {
    /// Spawns the worker and returns the shared handle. The worker sleeps
    /// (barely polling) while `enabled` is false, so an unopened pane costs
    /// nothing.
    pub fn spawn(session: SessionHandle) -> Arc<Self> {
        let shared = Arc::new(Vis {
            enabled: AtomicBool::new(false),
            size: Mutex::new((0, 0)),
            frame: Mutex::new(Frame::default()),
        });
        let worker = shared.clone();
        thread::spawn(move || worker.run(session));
        shared
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Called from `draw`: publishes the pane's current size for the worker
    /// to compute against, and paints the last frame it produced (a no-op
    /// blit — no allocation beyond one 1-byte str per cell).
    pub fn draw(&self, printer: &Printer, focused: bool) {
        let title = if focused { "[Vis]" } else { "Vis" };
        printer.with_color(ColorStyle::title_secondary(), |p| {
            p.print((0, 0), &crate::view::pad(title, p.size.x));
        });
        let w = printer.size.x;
        let h = printer.size.y.saturating_sub(1);
        if w == 0 || h == 0 {
            return;
        }
        *self.size.lock().unwrap() = (w, h);
        let frame = self.frame.lock().unwrap();
        if frame.w != w || frame.h != h {
            return; // worker hasn't caught up to a resize yet
        }
        for y in 0..h {
            for x in 0..w {
                let (r, g, b, ch) = frame.cells[y * w + x];
                let style = ColorStyle::new(Color::Rgb(r, g, b), Color::TerminalDefault);
                printer.with_color(style, |p| {
                    p.print((x, y + 1), std::str::from_utf8(&[ch]).unwrap());
                });
            }
        }
    }

    fn run(&self, session: SessionHandle) {
        // Per-band envelope followers (0.0..=1.0) — the real signal
        // (`audio_levels`) is jumpy sample to sample, this is what turns it
        // into something that reads as bars moving rather than flickering.
        // Rises fast (transients pop), falls slower (a natural decay
        // instead of an instant drop).
        let mut smoothed = [0.0f32; N_BARS];
        // Hue's own clock: drifts faster when the (smoothed) signal is
        // louder overall, so "the gradient shifts as the music changes" is
        // literally true, not just a fixed animation.
        let mut hue_phase: f32 = 0.0;
        let mut last_bars_tick = Instant::now();
        loop {
            if !self.enabled.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(1000 / FPS as u64) * 3);
                last_bars_tick = Instant::now(); // don't count idle time as motion
                continue;
            }
            let (w, h) = *self.size.lock().unwrap();
            if w > 0 && h > 0 {
                // No dedup: even silence needs the envelope to keep decaying
                // toward zero, not freeze on the last frame.
                let now = Instant::now();
                // Cap dt so a long gap (pane just reopened, a slow tick)
                // doesn't jump the envelopes/hue into a sudden lurch.
                let dt = now.duration_since(last_bars_tick).as_secs_f32().min(0.25);
                last_bars_tick = now;
                let levels = session.lock().unwrap().audio_levels();
                for (i, s) in smoothed.iter_mut().enumerate() {
                    let target = level_to_height(levels[i]);
                    let rate = if target > *s { 18.0 } else { 4.0 };
                    *s += (target - *s) * (1.0 - (-rate * dt).exp());
                }
                let overall = smoothed.iter().sum::<f32>() / N_BARS as f32;
                hue_phase += dt * (0.015 + overall * 0.25);
                *self.frame.lock().unwrap() = compute_bars(w, h, &smoothed, hue_phase);
            } else {
                last_bars_tick = Instant::now();
            }
            thread::sleep(Duration::from_millis(1000 / FPS as u64));
        }
    }
}

/// Goertzel magnitude -> a visible bar height, dBFS-meter style: real
/// program material sits nowhere near a full-scale single-bin tone (whose
/// magnitude would be ~0.5), so a linear mapping would look almost
/// motionless. -50dB..-6dB maps to 0.0..1.0 (clamped outside that).
fn level_to_height(magnitude: f32) -> f32 {
    let db = 20.0 * magnitude.max(1e-6).log10();
    ((db + 50.0) / 44.0).clamp(0.0, 1.0)
}

/// Renders the 5-bar EQ from already-smoothed, already-scaled `heights`
/// (0.0..=1.0 per band — the real audio analysis and envelope smoothing
/// happen in `run`, via `Session::audio_levels`) and a hue that sweeps
/// left-to-right across the whole strip, drifting with `hue_phase`.
fn compute_bars(w: usize, h: usize, heights: &[f32; N_BARS], hue_phase: f32) -> Frame {
    if w == 0 || h == 0 {
        return Frame { w, h, cells: Vec::new() };
    }
    let hue_base = hue_phase.rem_euclid(1.0);

    let mut cells = vec![(0u8, 0u8, 0u8, b' '); w * h];
    let bar_w = (w / N_BARS).max(1);
    for (i, &height) in heights.iter().enumerate() {
        let filled = (height * h as f32).round() as usize;
        let x0 = i * bar_w;
        // leave a 1-column gap before the next bar, except the last (which
        // just runs to the edge, absorbing any width%N_BARS remainder).
        let x1 = if i + 1 == N_BARS { w } else { ((i + 1) * bar_w).saturating_sub(1).min(w) };
        for x in x0..x1 {
            let hue = (hue_base + x as f32 / w as f32).rem_euclid(1.0);
            let (r, g, b) = hsv_to_rgb(hue, 0.85, 0.9);
            for row in 0..filled.min(h) {
                let y = h - 1 - row;
                cells[y * w + x] = (r, g, b, b'#');
            }
        }
    }
    Frame { w, h, cells }
}

/// `h`/`s`/`v` in `0.0..=1.0` to 8-bit RGB.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i64).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}
