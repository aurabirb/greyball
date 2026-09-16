//! `NullPlayer` — a `core::Player` with no audio device and no decoding.
//! It emits `Loading -> Playing -> Progress x2 -> Finished`
//! carrying **exactly** the `(source, uri)` it was loaded with, so the
//! `store.track_by_rendition` stale-check in `Session::on_event` matches.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::{Bus, CoreEvent, Player, PlayerEvent, PlayerState, PlayerStatus, Rendition};

use crate::{Snapshot, clamp_volume};

pub struct NullPlayer {
    inner: Arc<Mutex<Snapshot>>,
    bus: Bus,
    fake_len_ms: u32,
    /// Bumped on every `load`/`stop`; the fake-progress thread exits when its
    /// captured value no longer matches (cancels a superseded playback so no
    /// stale `Finished` is emitted).
    generation: Arc<AtomicU64>,
}

impl NullPlayer {
    /// `fake_len_ms` is the pretend track length; default is 200.
    pub fn new(bus: Bus, fake_len_ms: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Snapshot::default())),
            bus,
            fake_len_ms: fake_len_ms.max(3),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Player for NullPlayer {
    fn accepts(&self, _r: &Rendition) -> bool {
        true
    }

    fn load(&self, r: &Rendition, start_paused: bool, position_ms: u32, _cache: bool) {
        let my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (source, uri) = (r.source.clone(), r.uri.clone());

        {
            let mut s = self.inner.lock().unwrap();
            s.source = Some(source.clone());
            s.uri = Some(uri.clone());
            s.position_ms = position_ms;
            s.duration_ms = self.fake_len_ms;
            s.state = if start_paused {
                PlayerState::Paused
            } else {
                PlayerState::Playing
            };
        }

        self.bus.send(CoreEvent::Player(PlayerEvent::Loading {
            source: source.clone(),
            uri: uri.clone(),
        }));
        if !start_paused {
            self.bus.send(CoreEvent::Player(PlayerEvent::Playing {
                source: source.clone(),
                uri: uri.clone(),
            }));
        }

        let bus = self.bus.clone();
        let inner = self.inner.clone();
        let generation = self.generation.clone();
        let len = self.fake_len_ms;
        std::thread::spawn(move || {
            let step = Duration::from_millis((len / 3).max(1) as u64);
            let stale = |g: &AtomicU64| g.load(Ordering::SeqCst) != my_gen;

            for n in 1..=2u32 {
                std::thread::sleep(step);
                if stale(&generation) {
                    return;
                }
                let pos = len * n / 3;
                inner.lock().unwrap().position_ms = pos;
                bus.send(CoreEvent::Player(PlayerEvent::Progress {
                    position_ms: pos,
                    duration_ms: len,
                }));
            }

            std::thread::sleep(step);
            if stale(&generation) {
                return;
            }
            {
                let mut s = inner.lock().unwrap();
                s.state = PlayerState::Stopped;
                s.position_ms = 0;
            }
            bus.send(CoreEvent::Player(PlayerEvent::Finished { source, uri }));
        });
    }

    fn toggle(&self) {
        let mut s = self.inner.lock().unwrap();
        let (source, uri) = match (s.source.clone(), s.uri.clone()) {
            (Some(src), Some(u)) => (src, u),
            _ => return,
        };
        match s.state {
            PlayerState::Playing => {
                s.state = PlayerState::Paused;
                drop(s);
                self.bus.send(CoreEvent::Player(PlayerEvent::Paused));
            }
            PlayerState::Paused => {
                s.state = PlayerState::Playing;
                drop(s);
                self.bus
                    .send(CoreEvent::Player(PlayerEvent::Playing { source, uri }));
            }
            PlayerState::Stopped => {}
        }
    }

    fn seek(&self, position_ms: u32) {
        let duration_ms = {
            let mut s = self.inner.lock().unwrap();
            s.position_ms = position_ms;
            s.duration_ms
        };
        self.bus.send(CoreEvent::Player(PlayerEvent::Progress {
            position_ms,
            duration_ms,
        }));
    }

    fn set_volume(&self, v: f32) {
        self.inner.lock().unwrap().volume = clamp_volume(v);
    }

    fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut s = self.inner.lock().unwrap();
        s.state = PlayerState::Stopped;
        s.position_ms = 0;
        drop(s);
        self.bus.send(CoreEvent::Player(PlayerEvent::Stopped));
    }

    fn status(&self) -> PlayerStatus {
        self.inner.lock().unwrap().status()
    }
}
