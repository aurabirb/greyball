//! macOS equivalent of [`crate::mpris`]: hardware media keys (Play/Pause,
//! Next, Previous) and Control Center / lock-screen "Now Playing" controls
//! don't go through D-Bus on macOS — the native mechanism is the
//! `MediaPlayer` framework's `MPRemoteCommandCenter` (registers handlers the
//! system routes hardware keys and Control Center taps to) and
//! `MPNowPlayingInfoCenter` (publishes the currently-playing track's
//! metadata/position for Control Center and the lock screen to display).
//!
//! **UNTESTED**: this module was written in a Linux sandbox with no macOS
//! SDK and no cross-compiler for `aarch64/x86_64-apple-darwin` — it has never
//! been compiled. The `objc2`/`objc2-media-player` API shapes below are
//! written from documented API knowledge (docs.rs), not verified against a
//! real toolchain. Before trusting this on a real Mac, at minimum check:
//! - The `addTargetWithHandler` block signature/return type against whatever
//!   `objc2-media-player` version actually resolves.
//! - Whether registering commands this early (during app startup, before
//!   cursive's main loop starts spinning) is sufficient, or whether macOS
//!   requires it to happen after the app has an active audio session/run
//!   loop tick.
//!
//! Both APIs are synchronous, main-thread-affine Cocoa APIs (unlike MPRIS's
//! async D-Bus connection) — no dedicated thread/runtime is spun up here.
//! `register` must be called from the main thread before cursive's run loop
//! starts, which is where `app/src/main.rs` calls it.
//!
//! Play/Pause state-gating logic is shared with `crate::mpris` via
//! `crate::media_keys_common` rather than duplicated. `Stop` and playback
//! position scrubbing (`MPChangePlaybackPositionCommand`) are deliberately
//! left unimplemented, for the same reasons `mpris.rs` left `Stop()` a
//! no-op and to keep the unverified Objective-C surface smaller.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use medley_core::{PlayerState, Session};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};

use crate::media_keys_common::{pause_command, play_command};

type CommandHandler = RcBlock<dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus>;

fn playback_state(state: PlayerState) -> MPNowPlayingPlaybackState {
    match state {
        PlayerState::Playing => MPNowPlayingPlaybackState::Playing,
        PlayerState::Paused => MPNowPlayingPlaybackState::Paused,
        PlayerState::Stopped => MPNowPlayingPlaybackState::Stopped,
    }
}

/// Handle owning the registered `MPRemoteCommandCenter` handlers. Must be
/// kept alive for the app's lifetime — dropping it would (we believe) let
/// the handler blocks and their opaque target tokens be deallocated, which
/// likely tears down the registration. UNVERIFIED, see module docs.
pub struct MediaKeysManager {
    session: Arc<Mutex<Session>>,
    _handler_tokens: Vec<Retained<AnyObject>>,
    _handler_blocks: Vec<CommandHandler>,
}

impl MediaKeysManager {
    /// Registers Play/Pause/Toggle/Next/Previous handlers on the shared
    /// `MPRemoteCommandCenter`. Must be called from the main thread, once,
    /// before the app's run loop starts. Unlike `MprisManager::spawn`, this
    /// can't fail soft at runtime the same way (no "session bus unreachable"
    /// equivalent) — these APIs are always present on macOS.
    pub fn register(session: Arc<Mutex<Session>>) -> Self {
        let mut tokens = Vec::new();
        let mut blocks = Vec::new();

        // SAFETY: called once from the main thread during startup, before
        // cursive's run loop starts, per Apple's threading requirements for
        // Cocoa singletons like `MPRemoteCommandCenter`.
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();

            let s = session.clone();
            let block: CommandHandler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                let mut sess = s.lock().unwrap();
                match play_command(sess.player_status().state) {
                    Some(cmd) => {
                        let _ = sess.dispatch(cmd);
                        MPRemoteCommandHandlerStatus::Success
                    }
                    None => MPRemoteCommandHandlerStatus::CommandFailed,
                }
            });
            tokens.push(center.playCommand().addTargetWithHandler(&block));
            blocks.push(block);

            let s = session.clone();
            let block: CommandHandler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                let mut sess = s.lock().unwrap();
                match pause_command(sess.player_status().state) {
                    Some(cmd) => {
                        let _ = sess.dispatch(cmd);
                        MPRemoteCommandHandlerStatus::Success
                    }
                    None => MPRemoteCommandHandlerStatus::CommandFailed,
                }
            });
            tokens.push(center.pauseCommand().addTargetWithHandler(&block));
            blocks.push(block);

            let s = session.clone();
            let block: CommandHandler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                let _ = s.lock().unwrap().dispatch(medley_core::Command::PlayPause);
                MPRemoteCommandHandlerStatus::Success
            });
            tokens.push(center.togglePlayPauseCommand().addTargetWithHandler(&block));
            blocks.push(block);

            let s = session.clone();
            let block: CommandHandler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                let _ = s.lock().unwrap().dispatch(medley_core::Command::Next);
                MPRemoteCommandHandlerStatus::Success
            });
            tokens.push(center.nextTrackCommand().addTargetWithHandler(&block));
            blocks.push(block);

            let s = session.clone();
            let block: CommandHandler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                let _ = s.lock().unwrap().dispatch(medley_core::Command::Previous);
                MPRemoteCommandHandlerStatus::Success
            });
            tokens.push(center.previousTrackCommand().addTargetWithHandler(&block));
            blocks.push(block);
        }

        log::info!("media_keys_macos: registered {} command handlers", tokens.len());
        Self { session, _handler_tokens: tokens, _handler_blocks: blocks }
    }

    /// Publishes the current track/position/state to `MPNowPlayingInfoCenter`
    /// — call this from the same spots `mpris.rs`'s `notify_playback_status`
    /// and `notify_metadata` are called (playback state transitions and
    /// track changes), since this single call covers both.
    pub fn update_now_playing(&self) {
        let s = self.session.lock().unwrap();
        let status = s.player_status();
        let track = s.now_playing();
        drop(s);

        log::debug!(
            "media_keys_macos: update_now_playing state={:?} position_ms={} track={:?}",
            status.state,
            status.position_ms,
            track.as_ref().map(|t| &t.title)
        );

        // SAFETY: main-thread-affine Cocoa singleton, see module docs.
        unsafe {
            let info_center = MPNowPlayingInfoCenter::defaultCenter();
            log::debug!("media_keys_macos: calling setPlaybackState for {:?}", status.state);
            info_center.setPlaybackState(playback_state(status.state.clone()));

            let Some(track) = track else {
                log::debug!("media_keys_macos: no current track, clearing NowPlayingInfo");
                info_center.setNowPlayingInfo(None);
                return;
            };

            let dict = NSMutableDictionary::<NSString, AnyObject>::new();
            dict.insert(MPMediaItemPropertyTitle, &*NSString::from_str(&track.title));
            dict.insert(
                MPMediaItemPropertyArtist,
                &*NSString::from_str(&track.display_artist()),
            );
            if let Some(album) = &track.album {
                dict.insert(MPMediaItemPropertyAlbumTitle, &*NSString::from_str(album));
            }
            dict.insert(
                MPMediaItemPropertyPlaybackDuration,
                &*NSNumber::new_f64(f64::from(track.duration_ms) / 1000.0),
            );
            dict.insert(
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                &*NSNumber::new_f64(f64::from(status.position_ms) / 1000.0),
            );
            let rate = if status.state == PlayerState::Playing { 1.0 } else { 0.0 };
            dict.insert(MPNowPlayingInfoPropertyPlaybackRate, &*NSNumber::new_f64(rate));

            log::debug!(
                "media_keys_macos: setNowPlayingInfo title={:?} rate={rate}",
                track.title
            );
            info_center.setNowPlayingInfo(Some(&dict));
        }
    }
}
