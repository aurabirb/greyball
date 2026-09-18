use std::collections::HashSet;

use cursive::event::EventResult;

use core::{Command, Session, TrackId};

use super::{Focus, HIST, MedleyView, NOW_PLAYING, PLAYLISTS, QUEUE, SEARCH};
use super::input::Editing;
use super::panes::list_screen_for_pane;
use super::playlists::TopRow;
use super::rows::{Cell, LIST_TITLE_ROWS, Row, plain_row, tracks_to_rows};

use super::tab_bar::screen_name;

impl MedleyView {
    /// Track ids visible on `screen`, in display order.
    pub(super) fn visible_track_ids(&self, s: &Session, screen: usize) -> Vec<TrackId> {
        if let Some(tracks) = self.filtered_tracks(s, screen) {
            return tracks.iter().map(|t| t.id).collect();
        }
        match screen {
            NOW_PLAYING => s.playing_context_ids(),
            SEARCH => s.results_ids(),
            QUEUE => s.queue_ids(),
            HIST => s.history_ids(),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlist_track_ids(id)
                } else if let Some((sid, _, node)) = &self.open_remote {
                    s.remote_playlist_track_ids(sid, node)
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// The track selected on `screen`.
    pub(super) fn selected_track(&self, s: &Session, screen: usize) -> Option<TrackId> {
        let ids = self.visible_track_ids(s, screen);
        ids.get(self.lists[screen].cursor).copied()
    }

    /// Which screen index keyboard nav/selection currently targets.
    pub(super) fn active_screen(&self) -> usize {
        match self.focus {
            Focus::Pane(p) => list_screen_for_pane(p).unwrap_or(self.screen),
            Focus::Main | Focus::Warnings => self.screen,
        }
    }

    /// Plays row `idx` of `screen`'s track list, same as pressing Enter on it while selected.
    pub(super) fn play_track_at(&mut self, screen: usize, idx: usize) -> EventResult {
        let (tracks, sel, name) = self.with_session(|s| {
            let tracks = self.visible_track_ids(s, screen);
            let sel = tracks.get(idx).copied();
            (tracks, sel, self.context_name(s, screen))
        });
        let Some(id) = sel else {
            return EventResult::consumed();
        };
        let index = tracks.iter().position(|t| *t == id).unwrap_or(0);
        // Only the Playlists screen's own remote-browse state is ever meaningful here.
        let remote = if screen == PLAYLISTS {
            self.open_remote.as_ref().map(|(sid, _, node)| (sid.clone(), node.clone()))
        } else {
            None
        };
        self.run(Command::PlayContext { tracks, index, remote, name })
    }

    /// `screen`'s track list's display name.
    fn context_name(&self, s: &Session, screen: usize) -> Option<String> {
        match screen {
            NOW_PLAYING => s.playing_context_name(),
            SEARCH => Some("Search results".to_string()),
            QUEUE => Some("Queue".to_string()),
            HIST => Some("History".to_string()),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlists().into_iter().find(|p| p.id == id).map(|p| p.name)
                } else {
                    self.open_remote.as_ref().map(|(_, name, _)| name.clone())
                }
            }
            _ => None,
        }
    }

    pub(super) fn row_unit(&self, screen: usize, count: usize) -> &'static str {
        let playlists = screen == PLAYLISTS
            && self.open_playlist.is_none()
            && self.open_remote.is_none();
        match (playlists, count == 1) {
            (true, true) => "playlist",
            (true, false) => "playlists",
            (false, true) => "track",
            (false, false) => "tracks",
        }
    }

    /// `screen`'s list title row: `<name>`, optionally followed by `  (<hint>)`.
    pub(super) fn list_title(&self, s: &Session, screen: usize) -> String {
        if let Some(query) = self.active_filter().filter(|q| !q.is_empty() && self.filterable_screen(screen)) {
            let total = self.visible_track_ids(s, screen).len();
            let plural = if total == 1 { "" } else { "es" };
            return format!("filter {query:?} ({total} match{plural})");
        }
        let (name, hint) = match screen {
            NOW_PLAYING => (s.playing_context_name(), None),
            SEARCH => (self.last_query.clone(), (self.editing == Editing::Search).then_some("Esc to cancel")),
            PLAYLISTS => {
                let name = match (self.open_playlist, &self.open_remote) {
                    (Some(id), _) => s.playlists().into_iter().find(|p| p.id == id).map(|p| p.name),
                    (None, Some((sid, name, _))) => Some(format!("[{sid}] {name}")),
                    (None, None) => None,
                };
                let hint = name.is_some().then_some("Esc to go back");
                (name, hint)
            }
            _ => (None, None),
        };
        let name = name.unwrap_or_else(|| {
            let total = self.list_len(s, screen);
            format!("{} ({total} {})", screen_name(screen), self.row_unit(screen, total))
        });
        match hint {
            Some(hint) => format!("{name}  ({hint})"),
            None => name,
        }
    }

    /// Resolves only the visible `offset`/`limit` window — a list can run into the thousands.
    pub(super) fn rows(&self, s: &Session, screen: usize, offset: usize, limit: usize) -> Vec<Row> {
        let pending: HashSet<TrackId> = match &self.open_remote {
            Some((sid, _, node)) if screen == PLAYLISTS => s.remote_pending_ids(sid, node).into_iter().collect(),
            _ => HashSet::new(),
        };
        let track_rows = |tracks| tracks_to_rows(s, tracks, &pending);
        if let Some(matched) = self.filtered_tracks(s, screen) {
            let query = self.active_filter().unwrap_or_default();
            if matched.is_empty() {
                return vec![plain_row(format!("no matches for {query:?}"))];
            }
            return track_rows(matched.into_iter().skip(offset).take(limit).collect());
        }
        match screen {
            NOW_PLAYING => {
                if s.playing_context_len() == 0 {
                    // Nothing has ever been played this session — nothing to show a tracklist of yet.
                    vec![plain_row("nothing played yet — press Enter on a track to start playing")]
                } else {
                    track_rows(s.playing_context_window(offset, limit))
                }
            }
            SEARCH => {
                if s.results_len() == 0 {
                    match &self.last_query {
                        // A search ran and came back empty — say so.
                        Some(q) => vec![plain_row(format!(
                            "no results for {q:?} — check the Log pane (:log) for source errors"
                        ))],
                        None => vec![],
                    }
                } else {
                    track_rows(s.results_window(offset, limit))
                }
            }
            QUEUE => track_rows(s.queue_window(offset, limit)),
            HIST => track_rows(s.history_window(offset, limit)),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    track_rows(s.playlist_window(id, offset, limit))
                } else if let Some((sid, _, node)) = &self.open_remote {
                    track_rows(s.remote_playlist_window(sid, node, offset, limit))
                } else {
                    let playlists = s.playlists();
                    self.top_rows(s)
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(|row| {
                            let key = s.playlist_hotkey(&row.target());
                            let mut r = match &row {
                                TopRow::Local(id) => {
                                    let p = playlists.iter().find(|p| p.id == *id);
                                    let name = p.map(|p| p.name.clone()).unwrap_or_default();
                                    let count = p.map(|p| p.items.len()).unwrap_or(0);
                                    plain_row(format!("{name}  ({count} tracks)"))
                                }
                                TopRow::Remote(sid, name, _) => plain_row(format!("[{sid}] {name}")),
                            };
                            r.hotkeys = Cell::plain(key.map(String::from).unwrap_or_default());
                            r
                        })
                        .collect()
                }
            }
            _ => vec![],
        }
    }

    /// The current screen's full list length.
    pub(super) fn list_len(&self, s: &Session, screen: usize) -> usize {
        if self.filterable_screen(screen) {
            return self.visible_track_ids(s, screen).len();
        }
        match screen {
            NOW_PLAYING => s.playing_context_len(),
            SEARCH => s.results_len(),
            QUEUE => s.queue_len(),
            HIST => s.queue.history_len(),
            PLAYLISTS => self.top_rows(s).len(),
            _ => 0,
        }
    }

    pub(super) fn clamp_cursor(&mut self, len: usize) {
        let c = &mut self.lists[self.screen].cursor;
        if len == 0 {
            *c = 0;
        } else if *c >= len {
            *c = len - 1;
        }
    }

    /// The screen index Shift-J/Shift-K and PageUp/PageDown's cursor-jump should act on.
    fn active_list_screen(&self) -> Option<usize> {
        match self.focus {
            Focus::Main => Some(self.screen),
            Focus::Pane(pane) => list_screen_for_pane(pane),
            Focus::Warnings => None,
        }
    }

    /// Shift-J/Shift-K and PageUp/PageDown on the main tracklist or a focused Queue/History pane.
    pub(super) fn jump_list(&mut self, up: bool, step: usize) -> EventResult {
        let Some(screen) = self.active_list_screen() else { return EventResult::Ignored };
        let len = self.with_session(|s| self.list_len(s, screen));
        let view_h = match self.focus {
            Focus::Pane(pane) => self
                .last_pane_rects
                .iter()
                .find(|(p, _)| *p == pane)
                .map(|&(_, rect)| rect.height().saturating_sub(1))
                .unwrap_or(0),
            _ => self.list_h(),
        };
        self.lists[screen].jump(up, step, len, view_h);
        EventResult::consumed()
    }

    /// `clamp_cursor`, generalized to an explicit `screen` and folding in the forward step + length lookup.
    pub(super) fn bump_pane_cursor(&mut self, screen: usize, step: usize) {
        self.lists[screen].cursor = self.lists[screen].cursor.saturating_add(step);
        let len = self.with_session(|s| self.visible_track_ids(s, screen).len());
        let c = &mut self.lists[screen].cursor;
        if len == 0 {
            *c = 0;
        } else if *c >= len {
            *c = len - 1;
        }
    }

    /// Visible list rows as of the last layout pass — `last_main_rect` minus its title row.
    pub(super) fn list_h(&self) -> usize {
        self.last_main_rect.height().saturating_sub(LIST_TITLE_ROWS)
    }

    /// Keep each visible list's window around its cursor.
    pub(super) fn clamp_scroll(&mut self) {
        self.lists[self.screen].follow(self.list_h());
        // A focused docked list-pane has its own cursor and scroll window, sized to its own rect.
        if let Focus::Pane(pane) = self.focus
            && let Some(screen) = list_screen_for_pane(pane)
            && let Some(&(_, rect)) = self.last_pane_rects.iter().find(|(p, _)| *p == pane)
        {
            self.lists[screen].follow(rect.height().saturating_sub(1));
        }
    }

    /// Layout-pass upkeep for `screen`'s list window, shown `list_h` rows tall.
    pub(super) fn relayout_list(&mut self, screen: usize, resized: bool, list_h: usize) {
        let len = self.with_session(|s| self.list_len(s, screen));
        self.lists[screen].relayout(resized, len, list_h);
    }
}
