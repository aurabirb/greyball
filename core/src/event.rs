//! Event bus, with the cursive dependency replaced by an opaque `wake`
//! closure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::types::{SourceId, TrackId};

#[derive(Clone, Debug)]
pub enum CoreEvent {
    SearchHit(TrackId),
    SearchDone { source: SourceId },
    TrackUpdated(TrackId),
    QueueChanged,
    /// Playlists were added / replaced (e.g. by an M3U import). The front-end
    /// refreshes its Playlists screen.
    PlaylistsChanged,
    /// Queue moved `current` to this track and wants it played. `Session`
    /// resolves + routes to a `Player`; nothing else listens.
    PlayRequested(TrackId),
    Player(PlayerEvent),
    /// `Session::play_from_cache` decoded-and-cached this track's audio on a
    /// background thread (no `MediaCache` entry existed yet when the
    /// fallback was needed) and it's now ready — `Session` retries playing
    /// `TrackId` if it's still what's wanted.
    CacheFallbackReady(TrackId),
    SourceError {
        source: SourceId,
        message: String,
    },
    /// A plugin's health changed (e.g. `setup()` finished, on a background
    /// thread) — the front-end's cue to redraw the warnings panel. Doesn't
    /// carry the new status itself: `Session::plugin_statuses` reads it
    /// fresh (`Plugin::probe` is cheap by contract), same pattern as
    /// `PlaylistsChanged` re-`browse`ing rather than caching.
    PluginStatusChanged,
    /// A plugin's `setup()` (interactive login) just finished with
    /// `PluginHealth::Ok` — sent alongside `PluginStatusChanged`, never
    /// instead of it. Some plugins' login flow (e.g. spotify's
    /// `librespot-oauth` dependency) prints straight to the terminal outside
    /// cursive's own diffed redraw, so the front-end forces a full repaint
    /// on this instead of its usual incremental one.
    PluginLoginSucceeded,
    /// A plugin-registered `:`-command (see `crate::Plugin::run_command`)
    /// just finished on a background thread — the front-end's cue to show
    /// the result (`Session::take_plugin_command_result`) as a modal.
    PluginCommandResult,
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    Loading { source: SourceId, uri: String },
    Playing { source: SourceId, uri: String },
    Paused,
    Stopped,
    Progress { position_ms: u32, duration_ms: u32 },
    Finished { source: SourceId, uri: String },
    /// `uri` is close to its end — time to `Player::preload` what follows it.
    PreloadHint { source: SourceId, uri: String },
    /// A `Load` failed after playback already committed to it (a resolve or
    /// stream error, not a normal stop) — `Session` tries the local-cache
    /// playback fallback for this rendition's track before giving up.
    LoadFailed { source: SourceId, uri: String },
    /// A source finished materializing `uri`'s audio locally on its own
    /// terms — RodioPlayer's own fetch completing, or (for Spotify, which
    /// has no fetch step scan.rs can observe directly) its worker noticing
    /// the track landed in librespot's own on-disk cache. `Session` reacts
    /// by prioritizing this track's scan (`ScanDriver::prioritize`) instead
    /// of scanning having to poll for it.
    Materialized { source: SourceId, uri: String },
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Cross-thread event channel. `Clone`; anything may `send`, exactly one
/// consumer calls `drain`.
#[derive(Clone)]
pub struct Bus {
    tx: Sender<CoreEvent>,
    rx: Receiver<CoreEvent>,
    wake: Option<Wake>,
    /// Wake coalescing: `send` calls `wake` only
    /// on the `false -> true` transition. The front-end clears it once per
    /// event-loop iteration via [`Bus::clear_wake`], so a burst of `send`s
    /// during a crawl posts a single callback, not hundreds.
    wake_pending: Arc<AtomicBool>,
}

impl Bus {
    /// Channel only — used by tests and headless front-ends.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            tx,
            rx,
            wake: None,
            wake_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `wake` is invoked after a `send` that finds no wake already pending (the
    /// `app` crate pokes cursive). Coalesced — see [`Bus::clear_wake`].
    pub fn with_sink<F: Fn() + Send + Sync + 'static>(wake: F) -> Self {
        let (tx, rx) = unbounded();
        Self {
            tx,
            rx,
            wake: Some(Arc::new(wake)),
            wake_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn send(&self, ev: CoreEvent) {
        // MVP: a closed channel just means the front-end went away; ignore.
        let _ = self.tx.send(ev);
        if let Some(w) = &self.wake
            && !self.wake_pending.swap(true, Ordering::SeqCst)
        {
            w();
        }
    }

    /// Re-arm wake coalescing: the next `send` will invoke the sink again. The
    /// front-end calls this once per event-loop iteration (before `drain`).
    /// No-op for a [`Bus::new`] bus (no sink).
    pub fn clear_wake(&self) {
        self.wake_pending.store(false, Ordering::SeqCst);
    }

    /// All currently-queued events, without blocking.
    pub fn drain(&self) -> Vec<CoreEvent> {
        self.rx.try_iter().collect()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

