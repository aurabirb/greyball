//! Play/Pause gating logic shared by the Linux MPRIS backend ([`crate::mpris`])
//! and the macOS `MPRemoteCommandCenter` backend ([`crate::media_keys_macos`]).
//! Not itself platform-gated (pure, no OS API calls) so both cfg'd-out
//! modules can depend on the same tested logic without duplicating it.

use medley_core::{Command, PlayerState};

/// A "Play" request only makes sense resuming a paused track — there's no
/// `Command` to start playback from a cold stop without picking a track.
/// `None` means: no-op.
pub(crate) fn play_command(state: PlayerState) -> Option<Command> {
    (state == PlayerState::Paused).then_some(Command::PlayPause)
}

/// A "Pause" request -> toggle, but only while actually playing.
pub(crate) fn pause_command(state: PlayerState) -> Option<Command> {
    (state == PlayerState::Playing).then_some(Command::PlayPause)
}

