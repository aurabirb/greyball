use std::collections::HashMap;

use cursive::Rect;

use core::{BuiltinAction, HotkeyTarget, Session};

use crate::command::Pane;

use super::MedleyView;
use super::rows::Row;
use super::settings::{SettingsEntry, settings_entries};
use super::status_line::StatusLine;

/// One docked list-pane's rendered rows, keyed by `Pane` in `Frame::pane_rows`.
pub(super) struct PaneFrame {
    pub(super) title: String,
    pub(super) rows: Vec<Row>,
    pub(super) total: usize,
}

/// Everything `draw`'s no-modal path reads from the session, snapshotted under one lock.
pub(super) struct Frame {
    pub(super) rows: Vec<Row>,
    pub(super) total: usize,
    pub(super) status: StatusLine,
    pub(super) settings: Vec<SettingsEntry>,
    pub(super) warn_count: usize,
    pub(super) pane_rows: HashMap<Pane, PaneFrame>,
    pub(super) membership_feedback: Option<String>,
    pub(super) main_title: String,
    pub(super) list_loading: bool,
    /// The effective Help hotkey, for the hint line's fallback text.
    pub(super) help_key: Option<char>,
}

impl MedleyView {
    /// One session lock for the whole frame: every session-derived value `draw` needs, then unlocked.
    pub(super) fn frame(
        &self,
        list_h: usize,
        offset: usize,
        list_panes: &[(Pane, Rect, usize, usize, usize)],
        want_settings: bool,
    ) -> Frame {
        self.with_session(|s| self.snapshot(s, list_h, offset, list_panes, want_settings))
    }

    fn snapshot(
        &self,
        s: &Session,
        list_h: usize,
        offset: usize,
        list_panes: &[(Pane, Rect, usize, usize, usize)],
        want_settings: bool,
    ) -> Frame {
        let rows = self.rows(s, self.screen, offset, list_h);
        let total = self.list_len(s, self.screen);
        let main_title = self.list_title(s, self.screen);
        // A paginated remote list only knows what it has loaded so far.
        let list_loading = self.screen == super::PLAYLISTS
            && match &self.playlists.remote {
                Some((sid, _, node)) => s.remote_playlist_loading(sid, node),
                None => {
                    self.playlists.open.is_none()
                        && s.source_ids().iter().any(|sid| s.remote_playlists_loading(sid))
                }
            };
        let settings = if want_settings { settings_entries(s, self.panes.cfg) } else { Vec::new() };
        let warn_count = s.plugin_warning_count();
        // Feed the scan walk the visible list so it's prioritized over store order.
        if let Some(scan) = &s.scan {
            self.follow_scan(s, scan, self.active_screen());
        }
        let pane_rows = list_panes
            .iter()
            .map(|&(pane, _, screen, offset, pane_h)| {
                let rows = self.rows(s, screen, offset, pane_h);
                let total = self.list_len(s, screen);
                (pane, PaneFrame { title: self.list_title(s, screen), rows, total })
            })
            .collect();
        let help_key = s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::OpenHelp));
        Frame {
            rows,
            total,
            status: StatusLine::snapshot(s),
            settings,
            warn_count,
            pane_rows,
            membership_feedback: s.membership_feedback(),
            main_title,
            list_loading,
            help_key,
        }
    }
}
