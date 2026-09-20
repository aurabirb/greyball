//! `player` — audio playback for medley.
//!
//! Two `core::Player` implementations:
//! - [`RodioPlayer`]: real playback via `rodio`, fed by `core::StreamEngine` streams.
//! - [`NullPlayer`]: headless, no device, no decode — emits a fixed event
//!   sequence carrying exactly the loaded `(source, uri)`. Used by the M1c
//!   integration test.

mod fmp4;
mod null;
mod rodio_player;
pub mod spectrum;
mod tap;

pub use null::NullPlayer;
pub use rodio_player::RodioPlayer;
pub use tap::{AudioTap, AudioWindow, Tapped, WINDOW};

use core::{PlayerState, PlayerStatus};

/// Shared, audio-type-free status snapshot. Both players keep one behind a
/// `Mutex` so `status()` is cheap and the trait stays `Send + Sync`.
#[derive(Clone, Debug)]
struct Snapshot {
    state: PlayerState,
    position_ms: u32,
    duration_ms: u32,
    volume: f32,
    source: Option<core::SourceId>,
    uri: Option<String>,
    /// The stream being played or loaded, for the live download status.
    stream: Option<core::StreamHandle>,
    /// Playback is paused waiting for the download.
    buffering: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            state: PlayerState::Stopped,
            position_ms: 0,
            duration_ms: 0,
            volume: 1.0,
            source: None,
            uri: None,
            stream: None,
            buffering: false,
        }
    }
}

impl Snapshot {
    fn status(&self) -> PlayerStatus {
        let info = self.stream.as_ref().map(|h| h.info());
        let waiting = info.as_ref().is_some_and(|i| matches!(i.state, core::StreamState::Connecting | core::StreamState::Buffering));
        let downloading = info.as_ref().is_some_and(|i| {
            matches!(i.state, core::StreamState::Connecting | core::StreamState::Fetching | core::StreamState::Buffering | core::StreamState::Committing)
        });
        PlayerStatus {
            state: self.state,
            position_ms: self.position_ms,
            duration_ms: self.duration_ms,
            volume: self.volume,
            buffering: self.buffering || waiting,
            download_pct: info.and_then(|i| i.percent).filter(|_| downloading),
        }
    }
}

fn clamp_volume(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}
