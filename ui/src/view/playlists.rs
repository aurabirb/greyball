use cursive::event::EventResult;

use core::{BrowseNode, Command, HotkeyTarget, Playlist, PlaylistId, Session, SourceId};

use crate::command;

use super::{MedleyView, PLAYLISTS};
use super::input::popup;

/// One row of the Playlists screen's top-level list.
pub(super) enum TopRow {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode),
}

impl TopRow {
    /// This row's `HotkeyTarget` — what a playlist hotkey binds to.
    pub(super) fn target(&self) -> HotkeyTarget {
        match self {
            TopRow::Local(id) => HotkeyTarget::Local(*id),
            TopRow::Remote(sid, _, node) => HotkeyTarget::Remote(sid.clone(), node.clone()),
        }
    }
}

/// Display name for a `TopRow`.
pub(super) fn top_row_name(row: &TopRow, playlists: &[Playlist]) -> String {
    match row {
        TopRow::Local(id) => playlists
            .iter()
            .find(|p| p.id == *id)
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        TopRow::Remote(sid, name, _) => format!("[{sid}] {name}"),
    }
}

/// A remote browse folder/playlist as tracked by `open_remote`/`RememberedPlaylist::Remote`.
type RemoteOpen = (SourceId, String, BrowseNode);

/// Which kind of playlist view was open on the Playlists screen when it was left for another screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RememberedPlaylist {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode),
}

/// Pure state transition backing `leave_playlists`.
fn playlists_left(
    open_playlist: Option<PlaylistId>,
    open_remote: Option<RemoteOpen>,
    remembered: Option<RememberedPlaylist>,
) -> (Option<PlaylistId>, Option<RemoteOpen>, Option<RememberedPlaylist>) {
    let remembered = match (open_playlist, open_remote) {
        (Some(id), _) => Some(RememberedPlaylist::Local(id)),
        (None, Some((sid, name, node))) => Some(RememberedPlaylist::Remote(sid, name, node)),
        (None, None) => remembered,
    };
    (None, None, remembered)
}

/// What to reopen on switching back to Playlists; a remembered local playlist that no longer exists is dropped.
pub(super) fn resolve_remembered_playlist(
    remembered: Option<RememberedPlaylist>,
    playlists: &[Playlist],
) -> Option<RememberedPlaylist> {
    remembered.filter(|r| match r {
        RememberedPlaylist::Local(id) => playlists.iter().any(|p| p.id == *id),
        RememberedPlaylist::Remote(..) => true,
    })
}

impl MedleyView {
    pub(super) fn leave_playlists(&mut self) {
        let (open, remote, remembered) =
            playlists_left(self.open_playlist, self.open_remote.clone(), self.remembered_playlist.clone());
        self.open_playlist = open;
        self.open_remote = remote;
        self.remembered_playlist = remembered;
    }

    /// The Playlists screen's combined top-level list.
    pub(super) fn top_rows(&self, s: &Session) -> Vec<TopRow> {
        let mut rows: Vec<TopRow> = s.playlists().into_iter().map(|p| TopRow::Local(p.id)).collect();
        for sid in s.source_ids() {
            for (name, node) in s.remote_playlists(&sid) {
                rows.push(TopRow::Remote(sid.clone(), name, node));
            }
        }
        rows
    }

    /// The local playlist selected/open on the Playlists screen.
    pub(super) fn selected_playlist(&self, s: &Session) -> Option<PlaylistId> {
        if self.screen != PLAYLISTS {
            return None;
        }
        if let Some(id) = self.open_playlist {
            return Some(id);
        }
        if self.open_remote.is_some() {
            return None;
        }
        match self.top_rows(s).get(self.lists[PLAYLISTS].cursor) {
            Some(TopRow::Local(id)) => Some(*id),
            _ => None,
        }
    }

    /// The playlist (local *or* remote) selected/open on the Playlists screen.
    pub(super) fn selected_hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        if self.screen != PLAYLISTS {
            return None;
        }
        if let Some(id) = self.open_playlist {
            return Some(HotkeyTarget::Local(id));
        }
        if let Some((sid, _, node)) = &self.open_remote {
            return Some(HotkeyTarget::Remote(sid.clone(), node.clone()));
        }
        self.top_rows(s).into_iter().nth(self.lists[PLAYLISTS].cursor).map(|r| r.target())
    }

    /// `:open <url-or-path>`'s remote-link case.
    fn open_playlist_uri(&mut self, uri: String) -> EventResult {
        let found = self.with_session(|s| {
            let source = s.source_for_uri(&uri)?;
            let node = source.browse_uri(&uri)?;
            Some((source.id(), node))
        });
        match found {
            Some((sid, node)) => {
                let name = match &node {
                    core::BrowseNode::Path(id) => id.clone(),
                    core::BrowseNode::Root => String::new(),
                };
                self.screen = PLAYLISTS;
                self.open_playlist = None;
                self.open_remote = Some((sid, name, node));
                self.lists[PLAYLISTS].cursor = 0;
                self.filter_query = None; // a different list now — stale filter would be confusing
                self.clamp_scroll(); // new list under an old (now meaningless) cursor
                EventResult::consumed()
            }
            None => popup(format!("no source can open this as a playlist: {uri:?}")),
        }
    }

    /// `:open <url-or-path>`'s argument case.
    pub(super) fn open_arg(&mut self, arg: String, open: Option<PlaylistId>) -> EventResult {
        if self.with_session(|s| s.source_for_uri(&arg).is_some()) {
            return self.open_playlist_uri(arg);
        }
        let lower = arg.to_ascii_lowercase();
        if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
            return self.run(Command::ImportM3u(std::path::PathBuf::from(arg.trim())));
        }
        let paths = command::split_paths(&arg);
        self.run(Command::AddFilesToPlaylist { playlist: open, paths })
    }

    pub(super) fn activate(&mut self) -> EventResult {
        if self.screen == PLAYLISTS && self.open_playlist.is_none() && self.open_remote.is_none() {
            let row = self.with_session(|s| {
                self.top_rows(s).into_iter().nth(self.lists[PLAYLISTS].cursor)
            });
            match row {
                Some(TopRow::Local(id)) => {
                    self.open_playlist = Some(id);
                    self.lists[PLAYLISTS].cursor = 0;
                    self.filter_query = None;
                    self.clamp_scroll(); // opened a new list — old window is meaningless
                    return EventResult::consumed();
                }
                Some(TopRow::Remote(sid, name, node)) => {
                    self.open_remote = Some((sid, name, node));
                    self.lists[PLAYLISTS].cursor = 0;
                    self.filter_query = None;
                    self.clamp_scroll();
                    return EventResult::consumed();
                }
                None => {}
            }
        }
        EventResult::Ignored
    }
}
