//! A real tap into the live audio stream (not a volume-slider proxy) — for
//! anything that wants to look at what's actually playing, e.g. the `:vis 2`
//! equalizer.
//!
//! `Tapped` wraps a `rodio::Source`, forwarding every sample through
//! unchanged (playback is never affected) while also mixing multi-channel
//! frames down to mono and, once it's collected a window's worth, handing
//! that batch to `AudioTap`. The lock is only taken once per window (not per
//! sample), so it stays cheap on the audio thread's real-time deadline.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::{ChannelCount, Sample, SampleRate, Source};

/// Samples per published window — small enough to stay responsive (~23ms at
/// 44.1kHz), large enough for `player::spectrum::bands` to resolve down to
/// its lowest band.
pub const WINDOW: usize = 1024;

/// A window of mono samples plus the rate they were captured at — the
/// latter is needed to map `spectrum::bands`' target frequencies to bins.
pub struct AudioWindow {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// The most recent window of mono samples from whatever's currently
/// playing, continuously overwritten by the audio thread. Empty until
/// something has actually played at least one window's worth of audio.
#[derive(Default)]
pub struct AudioTap {
    latest: Mutex<Vec<f32>>,
    sample_rate: AtomicU32,
}

impl AudioTap {
    pub fn snapshot(&self) -> AudioWindow {
        AudioWindow {
            samples: self.latest.lock().unwrap().clone(),
            sample_rate: self.sample_rate.load(Ordering::Relaxed),
        }
    }

    /// Publish a window of mono samples — normally called only from
    /// [`Tapped`]'s own per-sample forwarding, but public so a player with no
    /// `rodio::Source` to wrap (e.g. `SpotifyPlayer`, which taps librespot's
    /// `Sink` layer directly instead) can still feed the same `AudioTap`.
    pub fn publish(&self, window: &[f32], sample_rate: u32) {
        let mut buf = self.latest.lock().unwrap();
        buf.clear();
        buf.extend_from_slice(window);
        drop(buf);
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
    }
}

pub struct Tapped<S: Source> {
    inner: S,
    tap: Arc<AudioTap>,
    channels: ChannelCount,
    sample_rate: SampleRate,
    /// Accumulates one interleaved frame before mixing it down to mono.
    frame: Vec<f32>,
    /// Accumulates mono samples until there's a full `WINDOW` to publish.
    window: Vec<f32>,
}

impl<S: Source> Tapped<S> {
    pub fn new(inner: S, tap: Arc<AudioTap>) -> Self {
        let channels = inner.channels().max(1);
        let sample_rate = inner.sample_rate();
        Self {
            inner,
            tap,
            channels,
            sample_rate,
            frame: Vec::with_capacity(channels as usize),
            window: Vec::with_capacity(WINDOW),
        }
    }
}

impl<S: Source> Iterator for Tapped<S> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let sample = self.inner.next()?;
        self.frame.push(sample);
        if self.frame.len() >= self.channels as usize {
            let mono = self.frame.iter().sum::<f32>() / self.frame.len() as f32;
            self.frame.clear();
            self.window.push(mono);
            if self.window.len() >= WINDOW {
                self.tap.publish(&self.window, self.sample_rate);
                self.window.clear();
            }
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Tapped<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        // Forward, or `Sink::try_seek` (used by `Command::Seek`) would
        // regress to always failing — the default impl says "unsupported".
        self.inner.try_seek(pos)
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}
