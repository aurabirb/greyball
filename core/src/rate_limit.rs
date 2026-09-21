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

/// A [`RateLimiter`] plus a shared cool-down: once any caller sets one, every
/// subsequent caller (a source's search and browse threads share one) waits it out.
pub struct RateGate {
    limiter: RateLimiter,
    cooldown_until: Mutex<Option<Instant>>,
}

impl RateGate {
    pub fn new(min_interval: Duration, jitter_max: Duration) -> Self {
        Self { limiter: RateLimiter::new(min_interval, jitter_max), cooldown_until: Mutex::new(None) }
    }

    pub fn wait_turn(&self) {
        self.wait_turn_while(&|| true);
    }

    /// Like `wait_turn`, but gives up (returning false) once `wanted` turns false while waiting out a cool-down.
    pub fn wait_turn_while(&self, wanted: &dyn Fn() -> bool) -> bool {
        loop {
            if !wanted() {
                return false;
            }
            let until = *self.cooldown_until.lock().unwrap();
            match until {
                Some(t) => match t.checked_duration_since(Instant::now()) {
                    Some(remaining) => std::thread::sleep(remaining.min(Duration::from_millis(250))),
                    None => {
                        *self.cooldown_until.lock().unwrap() = None;
                        break;
                    }
                },
                None => break,
            }
        }
        self.limiter.throttle();
        true
    }

    /// Make every caller back off for at least `wait`.
    pub fn set_cooldown(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut slot = self.cooldown_until.lock().unwrap();
        if slot.is_none_or(|current| current < until) {
            *slot = Some(until);
        }
    }
}
