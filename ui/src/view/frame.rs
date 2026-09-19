use std::sync::Arc;

use core::{BuiltinAction, HotkeyTarget};

use super::MedleyView;
use super::status_line::{StatusCore, StatusLine, bpm_status_tag};
use super::window::{WindowFrame, WindowId};

/// The shell's own session-derived draw data, rebuilt only when `Session::revision` moves.
pub(super) struct Chrome {
    status: StatusCore,
    pub(super) warn_count: usize,
    pub(super) help_key: Option<char>,
}

/// One frame's render data: each visible window's frame, the chrome, and this tick's live status line.
pub(super) struct Frame {
    pub(super) windows: Vec<WindowFrame>,
    pub(super) chrome: Arc<Chrome>,
    pub(super) status: StatusLine,
}

impl MedleyView {
    /// The frame's one session lock; every memo under it rebuilds only on its own key's miss.
    pub(super) fn frame(&self, visible: &[WindowId]) -> Frame {
        self.with_session(|s| {
            let ctx = self.ctx(s);
            let windows = visible.iter().map(|&id| self.windows[id].frame(&ctx)).collect();
            let chrome = self.chrome.get_or_build(s.revision(), || {
                Arc::new(Chrome {
                    status: StatusCore::snapshot(s),
                    warn_count: s.plugin_warning_count(),
                    help_key: s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::OpenHelp)),
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
