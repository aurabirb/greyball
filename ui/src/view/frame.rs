use std::sync::Arc;

use core::{BuiltinAction, HotkeyTarget};

use super::{MedleyView, Placed};
use super::status_line::{StatusCore, StatusLine, bpm_status_tag};
use super::window::WindowFrame;

/// The shell's own session-derived draw data, rebuilt only when `Session::revision` moves.
pub(super) struct Chrome {
    status: StatusCore,
    pub(super) warn_count: usize,
    pub(super) help_key: Option<char>,
    pub(super) keys_key: Option<char>,
    pub(super) place_key: Option<char>,
    pub(super) like_key: Option<char>,
}

/// One frame's render data: each visible window's frame, the chrome, and this tick's live status line.
pub(super) struct Frame {
    pub(super) windows: Vec<WindowFrame>,
    pub(super) chrome: Arc<Chrome>,
    pub(super) status: StatusLine,
}

impl MedleyView {
    /// The frame's one session lock; every memo under it rebuilds only on its own key's miss.
    pub(super) fn frame(&self, placed: &[Placed]) -> Frame {
        self.with_session(|s| {
            let ctx = self.ctx(s);
            let windows = placed.iter().map(|placed| self.windows[placed.id].frame(&ctx)).collect();
            let chrome = self.chrome.get_or_build((s.revision(), s.warnings_revision()), || {
                Arc::new(Chrome {
                    status: StatusCore::snapshot(s),
                    warn_count: s.warning_count(),
                    help_key: s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::OpenHelp)),
                    keys_key: s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::SwitchPlaylists)),
                    place_key: s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::CyclePlacement)),
                    like_key: s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::Like)),
                })
            });
            // Live per-tick data, never memoized.
            let ps = s.player_status();
            let bpm_tag = bpm_status_tag(s, chrome.status.now_playing_id());
            let status = StatusLine::assemble(&chrome.status, ps.position_ms, ps.duration_ms, bpm_tag);
            Frame { windows, chrome, status }
        })
    }
}
