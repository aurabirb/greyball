use cursive::event::EventResult;

use core::{BrowseNode, Command, HotkeyTarget, Playlist, PlaylistId, Session, SourceId};

use crate::command;
use crate::screen::Screen;

use super::MedleyView;
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

/// An open remote browse folder/playlist: source, display name, node.
type RemoteOpen = (SourceId, String, BrowseNode);

/// Which kind of playlist view was open on the Playlists screen when it was left for another screen.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RememberedPlaylist {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode),
}

/// Where the Playlists screen is: its top-level list, or inside one local/remote playlist.
#[derive(Default)]
pub(super) struct PlaylistNav {
    /// The local playlist whose tracks are shown; mutually exclusive with `remote`.
    pub(super) open: Option<PlaylistId>,
    pub(super) remote: Option<RemoteOpen>,
    /// What was open when the screen was last left, reopened on switching back.
    remembered: Option<RememberedPlaylist>,
}

impl PlaylistNav {
    pub(super) fn at_top_level(&self) -> bool {
        self.open.is_none() && self.remote.is_none()
    }

    /// Identity of the open list, for caches keyed on it.
    pub(super) fn list_id(&self) -> (Option<PlaylistId>, Option<(SourceId, BrowseNode)>) {
        (self.open, self.remote.clone().map(|(sid, _, node)| (sid, node)))
    }

    /// Leaving the screen closes whatever is open but remembers it.
    pub(super) fn leave(&mut self) {
        self.remembered = match (self.open.take(), self.remote.take()) {
            (Some(id), _) => Some(RememberedPlaylist::Local(id)),
            (None, Some((sid, name, node))) => Some(RememberedPlaylist::Remote(sid, name, node)),
            (None, None) => self.remembered.take(),
        };
    }

    /// Esc back to the top level: unlike `leave`, this forgets what was open.
    pub(super) fn back_out(&mut self) {
        *self = Self::default();
    }

    /// Reopens what `leave` remembered, unless it's a local playlist that no longer exists in `playlists`.
    pub(super) fn restore(&mut self, playlists: &[Playlist]) {
        match self.remembered.clone() {
            Some(RememberedPlaylist::Local(id)) if playlists.iter().any(|p| p.id == id) => self.open = Some(id),
            Some(RememberedPlaylist::Remote(sid, name, node)) => self.remote = Some((sid, name, node)),
            _ => {}
        }
    }

    pub(super) fn open_local(&mut self, id: PlaylistId) {
        self.open = Some(id);
        self.remote = None;
    }

    pub(super) fn open_remote(&mut self, sid: SourceId, name: String, node: BrowseNode) {
        self.open = None;
        self.remote = Some((sid, name, node));
    }
}

impl MedleyView {
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
        if self.screen != Screen::Playlists {
            return None;
        }
        if let Some(id) = self.playlists.open {
            return Some(id);
        }
        if self.playlists.remote.is_some() {
            return None;
        }
        match self.top_rows(s).get(self.lists[Screen::Playlists].cursor) {
            Some(TopRow::Local(id)) => Some(*id),
            _ => None,
        }
    }

    /// The playlist (local *or* remote) selected/open on the Playlists screen.
    pub(super) fn selected_hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        if self.screen != Screen::Playlists {
            return None;
        }
        if let Some(id) = self.playlists.open {
            return Some(HotkeyTarget::Local(id));
        }
        if let Some((sid, _, node)) = &self.playlists.remote {
            return Some(HotkeyTarget::Remote(sid.clone(), node.clone()));
        }
        self.top_rows(s).into_iter().nth(self.lists[Screen::Playlists].cursor).map(|r| r.target())
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
                self.screen = Screen::Playlists;
                self.playlists.open_remote(sid, name, node);
                self.lists[Screen::Playlists].cursor = 0;
                self.filter.query = None; // a different list now — stale filter would be confusing
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
        if self.screen == Screen::Playlists && self.playlists.at_top_level() {
            let row = self.with_session(|s| {
                self.top_rows(s).into_iter().nth(self.lists[Screen::Playlists].cursor)
            });
            match row {
                Some(TopRow::Local(id)) => {
                    self.playlists.open_local(id);
                    self.lists[Screen::Playlists].cursor = 0;
                    self.filter.query = None;
                    self.clamp_scroll(); // opened a new list — old window is meaningless
                    return EventResult::consumed();
                }
                Some(TopRow::Remote(sid, name, node)) => {
                    self.playlists.open_remote(sid, name, node);
                    self.lists[Screen::Playlists].cursor = 0;
                    self.filter.query = None;
                    self.clamp_scroll();
                    return EventResult::consumed();
                }
                None => {}
            }
        }
        EventResult::Ignored
    }
}
