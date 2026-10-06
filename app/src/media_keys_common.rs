//! Absolute-to-relative `Command` mapping shared by the Linux MPRIS backend ([`crate::mpris`]),
//! the macOS `MPRemoteCommandCenter` backend ([`crate::media_keys_macos`]) and the web interface.
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


/// `Command::Seek` is relative milliseconds; callers hold an absolute target.
pub(crate) fn seek_delta_ms(target_ms: i64, current_ms: u32) -> i64 {
    target_ms - i64::from(current_ms)
}

/// `Command::Volume` is a relative percent delta; callers hold an absolute 0.0..=1.0 target.
pub(crate) fn volume_delta_percent(target_volume: f64, current_volume: f32) -> i8 {
    let target_pct = (target_volume.clamp(0.0, 1.0) * 100.0).round();
    let current_pct = f64::from(current_volume.clamp(0.0, 1.0)) * 100.0;
    (target_pct - current_pct).clamp(f64::from(i8::MIN), f64::from(i8::MAX)) as i8
}
