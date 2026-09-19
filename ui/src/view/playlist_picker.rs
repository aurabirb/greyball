use cursive::{Printer, Rect};
use cursive::event::Event;

use core::{Command, Playlist, Session, TrackId};

use super::memo::Memo;
use super::modal::{ModalOutcome, draw_modal_frame, modal_list};
use super::scroll::{ListEvent, ListState};

/// The "Add to Playlist" picker (`+` with a track selected); exists only while open.
pub(super) struct PlaylistPicker {
    list: ListState,
    /// Captured when the picker opens, so it stays fixed if the underlying list scrolls.
    track: TrackId,
    playlists: Vec<Playlist>,
    /// The `list_revision` that `playlists` was read under.
    built: Memo<u64>,
}

impl PlaylistPicker {
    pub(super) fn new(track: TrackId, s: &Session) -> Self {
        let built = Memo::default();
        built.changed(s.list_revision());
        Self { list: ListState::default(), track, playlists: s.playlists(), built }
    }

    /// Re-reads the playlists once they changed, keeping the cursor on the same playlist and in view.
    pub(super) fn relayout(&mut self, resized: bool, rect: Rect, s: &Session) {
        let view_h = modal_list(rect).height();
        if self.built.changed(s.list_revision()) {
            let selected = self.playlists.get(self.list.cursor).map(|p| p.id);
            self.playlists = s.playlists();
            self.list.cursor = selected
                .and_then(|id| self.playlists.iter().position(|p| p.id == id))
                .unwrap_or_else(|| self.list.cursor.min(self.playlists.len().saturating_sub(1)));
            self.list.follow(view_h);
        }
        self.list.relayout(resized, self.playlists.len(), view_h);
    }

    pub(super) fn on_event(&mut self, event: &Event, rect: Rect) -> ModalOutcome {
        match self.list.on_event(event, self.playlists.len(), modal_list(rect)) {
            ListEvent::Close => ModalOutcome::Close,
            ListEvent::Activate | ListEvent::Clicked => match self.playlists.get(self.list.cursor) {
                Some(p) => ModalOutcome::Run(Command::AddToPlaylist { track: self.track, playlist: p.id }),
                None => ModalOutcome::Close,
            },
            ListEvent::Moved | ListEvent::Unhandled => ModalOutcome::Stay,
        }
    }

    pub(super) fn draw(&self, printer: &Printer, rect: Rect) {
        let body = draw_modal_frame(printer, rect, Some("Add to Playlist"), "  [Enter] add to selected playlist   [Esc] cancel");
        if self.playlists.is_empty() {
            body.print((0, 1), "(no playlists — :newplaylist <name> to make one)");
        }
        let lines: Vec<String> = self.playlists.iter().map(|p| p.name.clone()).collect();
        self.list.draw(&printer.windowed(modal_list(rect)), &lines);
    }
}
