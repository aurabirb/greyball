use std::sync::Arc as Rc;

use cursive::Rect;

use core::{BrowseNode, BuiltinAction, HotkeyTarget, PlaylistId, Session, SourceId};

use crate::command::Pane;
use crate::screen::Screen;

use super::MedleyView;
use super::input::Editing;
use super::rows::Row;
use super::settings::{SettingsEntry, settings_entries};
use super::status_line::{StatusCore, StatusLine, bpm_status_tag};

/// One docked list-pane's rendered rows.
pub(super) struct PaneFrame {
    pub(super) title: String,
    pub(super) rows: Vec<Row>,
    pub(super) total: usize,
}

/// Every UI-side input shaping `CachedFrame`, plus `revision` — miss one here and it serves a stale frame.
#[derive(Clone, PartialEq)]
pub(super) struct FrameKey {
    revision: u64,
    screen: Screen,
    list_id: (Option<PlaylistId>, Option<(SourceId, BrowseNode)>),
    query: Option<String>,
    editing: Editing,
    last_query: Option<String>,
    cursor: usize,
    offset: usize,
    list_h: usize,
    /// `(pane, screen, offset, height)` per open list pane, in draw order.
    panes: Vec<(Pane, Screen, usize, usize)>,
    want_settings: bool,
    pane_cfg: core::PaneLayoutConfig,
}

/// Everything `draw`'s no-modal path reads, rebuilt only on a `FrameKey` miss and shared via `Rc`.
pub(super) struct CachedFrame {
    pub(super) rows: Vec<Row>,
    pub(super) total: usize,
    status: StatusCore,
    pub(super) settings: Vec<SettingsEntry>,
    pub(super) warn_count: usize,
    pub(super) panes: Vec<PaneFrame>,
    pub(super) membership_feedback: Option<String>,
    pub(super) main_title: String,
    pub(super) list_loading: bool,
    pub(super) help_key: Option<char>,
    pub(super) hotkey_target_selected: bool,
}

/// One frame's render data: the memoized `CachedFrame` plus this frame's live status line.
pub(super) struct Frame {
    pub(super) cached: Rc<CachedFrame>,
    pub(super) status: StatusLine,
}

impl MedleyView {
    fn frame_key(
        &self,
        revision: u64,
        list_h: usize,
        offset: usize,
        list_panes: &[(Pane, Rect, Screen, usize, usize)],
        want_settings: bool,
    ) -> FrameKey {
        FrameKey {
            revision,
            screen: self.screen,
            list_id: self.playlists.list_id(),
            query: self.active_filter().map(str::to_string),
            editing: self.editing.clone(),
            last_query: self.last_query.clone(),
            cursor: self.lists[self.screen].cursor,
            offset,
            list_h,
            panes: list_panes.iter().map(|&(pane, _, screen, offset, h)| (pane, screen, offset, h)).collect(),
            want_settings,
            pane_cfg: self.panes.cfg,
        }
    }

    /// One session lock on a miss; a hit only re-reads revision plus live per-tick data.
    pub(super) fn frame(
        &self,
        list_h: usize,
        offset: usize,
        list_panes: &[(Pane, Rect, Screen, usize, usize)],
        want_settings: bool,
    ) -> Frame {
        self.with_session(|s| {
            let key = self.frame_key(s.revision(), list_h, offset, list_panes, want_settings);
            let cached = self.frame_cache.get_or_build(key, || {
                Rc::new(self.build_cached_frame(s, list_h, offset, list_panes, want_settings))
            });
            // Live per-tick data: cheap, no extra I/O beyond what's already locked.
            let ps = s.player_status();
            let bpm_tag = bpm_status_tag(s, cached.status.now_playing_id());
            let status = StatusLine::assemble(&cached.status, ps.position_ms, ps.duration_ms, bpm_tag);
            Frame { cached, status }
        })
    }

    fn build_cached_frame(
        &self,
        s: &Session,
        list_h: usize,
        offset: usize,
        list_panes: &[(Pane, Rect, Screen, usize, usize)],
        want_settings: bool,
    ) -> CachedFrame {
        let rows = self.rows(s, self.screen, offset, list_h);
        let total = self.list_len(s, self.screen);
        let main_title = self.list_title(s, self.screen);
        // A paginated remote list only knows what it has loaded so far.
        let list_loading = self.screen == Screen::Playlists
            && match &self.playlists.remote {
                Some((sid, _, node)) => s.remote_playlist_loading(sid, node),
                None => {
                    self.playlists.open.is_none()
                        && s.source_ids().iter().any(|sid| s.remote_playlists_loading(sid))
                }
            };
        let settings = if want_settings { settings_entries(s, self.panes.cfg) } else { Vec::new() };
        let warn_count = s.plugin_warning_count();
        let panes = list_panes
            .iter()
            .map(|&(_pane, _, screen, offset, pane_h)| {
                let rows = self.rows(s, screen, offset, pane_h);
                let total = self.list_len(s, screen);
                PaneFrame { title: self.list_title(s, screen), rows, total }
            })
            .collect();
        let help_key = s.effective_hotkey(&HotkeyTarget::Builtin(BuiltinAction::OpenHelp));
        CachedFrame {
            rows,
            total,
            status: StatusCore::snapshot(s),
            settings,
            warn_count,
            panes,
            membership_feedback: s.membership_feedback(),
            main_title,
            list_loading,
            help_key,
            hotkey_target_selected: self.selected_hotkey_target(s).is_some(),
        }
    }
}
