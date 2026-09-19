use cursive::{Printer, Rect};
use cursive::event::Event;

use core::{Command, Playlist, TrackId};

use super::MedleyView;
use super::memo::Memo;
use super::modal::{Modal, ModalOutcome, draw_modal_frame, modal_list};
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
    fn new(track: TrackId, playlists: Vec<Playlist>, list_revision: u64) -> Self {
        let built = Memo::default();
        built.changed(list_revision);
        Self { list: ListState::default(), track, playlists, built }
    }

    pub(super) fn stale(&self, list_revision: u64) -> bool {
        self.built.changed(list_revision)
    }

    /// Replaces `playlists`, keeps the cursor on the same playlist, and keeps it in view.
    pub(super) fn refresh(&mut self, playlists: Vec<Playlist>, rect: Rect) {
        let selected = self.playlists.get(self.list.cursor).map(|p| p.id);
        self.playlists = playlists;
        self.list.cursor = selected
            .and_then(|id| self.playlists.iter().position(|p| p.id == id))
            .unwrap_or_else(|| self.list.cursor.min(self.playlists.len().saturating_sub(1)));
        self.list.follow(modal_list(rect).height());
    }

    pub(super) fn relayout(&mut self, resized: bool, rect: Rect) {
        self.list.relayout(resized, self.playlists.len(), modal_list(rect).height());
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

impl MedleyView {
    pub(super) fn open_playlist_picker(&mut self, track: TrackId) {
        let (playlists, list_revision) = self.with_session(|s| (s.playlists(), s.list_revision()));
        self.modal = Some(Modal::Picker(PlaylistPicker::new(track, playlists, list_revision)));
    }
}
