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
    pub(super) prev_key: Option<char>,
    pub(super) next_key: Option<char>,
    pub(super) enqueue_key: Option<char>,
    pub(super) wedge_key: Option<char>,
    pub(super) clear_queue_key: Option<char>,
    pub(super) reveal_key: Option<char>,
    pub(super) layout_key: Option<char>,
    pub(super) kind_key: Option<char>,
    /// Every key bound to a playlist, sorted.
    pub(super) assigned: Vec<char>,
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
                let key = |action| s.effective_hotkey(&HotkeyTarget::Builtin(action));
                let mut assigned: Vec<char> =
                    s.hotkeys().into_iter().filter(|(_, target)| !matches!(target, HotkeyTarget::Builtin(_))).map(|(key, _)| key).collect();
                assigned.sort_unstable();
                Arc::new(Chrome {
                    status: StatusCore::snapshot(s),
                    warn_count: s.warning_count(),
                    help_key: key(BuiltinAction::OpenHelp),
                    keys_key: key(BuiltinAction::SwitchPlaylists),
                    place_key: key(BuiltinAction::CyclePlacement),
                    like_key: key(BuiltinAction::Like),
                    prev_key: key(BuiltinAction::Previous),
                    next_key: key(BuiltinAction::Next),
                    enqueue_key: key(BuiltinAction::Enqueue),
                    wedge_key: key(BuiltinAction::Wedge),
                    clear_queue_key: key(BuiltinAction::ClearQueue),
                    reveal_key: key(BuiltinAction::RevealPlaying),
                    layout_key: key(BuiltinAction::CyclePaneLayout),
                    kind_key: key(BuiltinAction::CycleKindFilter),
                    assigned,
                })
            });
            // Live per-tick data, never memoized.
            let ps = s.player_status();
            let bpm_tag = bpm_status_tag(s, chrome.status.now_playing_id());
            let liked = chrome.status.now_playing_id().and_then(|id| s.liked_mark(id));
            let status = StatusLine::assemble(&chrome.status, ps.position_ms, ps.duration_ms, bpm_tag, liked);
            Frame { windows, chrome, status }
        })
    }
}
