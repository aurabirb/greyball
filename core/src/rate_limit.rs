//! Per-instance outgoing-request pacing: a minimum gap plus jitter between
//! successive calls to [`RateLimiter::throttle`] on the same instance, so a
//! source's own pagination/search fan-out doesn't hammer its API back-to-
//! back. Not a cross-source singleton — each source holds its own instance
//! (e.g. a field on `WebApi`/`SoundcloudSource`, alongside its
//! `reqwest::blocking::Client`), since each API has its own limits.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;

pub struct RateLimiter {
    last: Mutex<Instant>,
    min_interval: Duration,
    jitter_max: Duration,
}

impl RateLimiter {
    /// `min_interval`: the floor gap between two calls. `jitter_max`: a
    /// random extra delay in `[0, jitter_max)` added on top, so a burst of
    /// callers desynchronizes instead of staying in lockstep.
    pub fn new(min_interval: Duration, jitter_max: Duration) -> Self {
        Self {
            last: Mutex::new(Instant::now() - min_interval),
            min_interval,
            jitter_max,
        }
    }

    /// Blocks the calling thread until `min_interval` (+ jitter) has passed
    /// since the last call through this instance.
    pub fn throttle(&self) {
        let mut last = self.last.lock().unwrap();
        let jitter = if self.jitter_max.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_millis(rand::rng().random_range(0..self.jitter_max.as_millis() as u64))
        };
        let gap = self.min_interval + jitter;
        let elapsed = last.elapsed();
        if elapsed < gap {
            std::thread::sleep(gap - elapsed);
        }
        *last = Instant::now();
    }
}
