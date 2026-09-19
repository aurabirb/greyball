use cursive::{Printer, Vec2};
use cursive::event::{Event, EventResult};
use cursive::theme::ColorStyle;

use core::{Command, Playlist, TrackId};

use super::MedleyView;
use super::scroll::{ListEvent, ListState, modal_list_rect, stale};
use super::text::pad;

/// Row the picker's list starts on (row 0 = title, row 1 = blank spacer).
const LIST_TOP: usize = 2;

/// The "Add to Playlist" picker (`+` with a track selected); exists only while open.
pub(super) struct PlaylistPicker {
    list: ListState,
    /// Captured when the picker opens, so it stays fixed if the underlying list scrolls.
    track: TrackId,
    /// Rebuilt whenever `built_at` drifts from `Session::revision` (e.g. a
    /// playlist is created while the picker is open) — see `MedleyView::refresh_playlist_picker`.
    playlists: Vec<Playlist>,
    built_at: u64,
}

impl PlaylistPicker {
    pub(super) fn new(track: TrackId, playlists: Vec<Playlist>, revision: u64) -> Self {
        Self { list: ListState::default(), track, playlists, built_at: revision }
    }

    pub(super) fn built_at(&self) -> u64 {
        self.built_at
    }

    /// Replaces `playlists`, keeping the cursor on the same playlist if it still exists.
    pub(super) fn refresh(&mut self, playlists: Vec<Playlist>, revision: u64) {
        let selected = self.playlists.get(self.list.cursor).map(|p| p.id);
        self.playlists = playlists;
        self.built_at = revision;
        self.list.cursor = selected
            .and_then(|id| self.playlists.iter().position(|p| p.id == id))
            .unwrap_or_else(|| self.list.cursor.min(self.playlists.len().saturating_sub(1)));
    }

    pub(super) fn relayout(&mut self, resized: bool, size: Vec2) {
        self.list.relayout(resized, self.playlists.len(), modal_list_rect(size, LIST_TOP).height());
    }

    pub(super) fn on_event(&mut self, event: &Event, size: Vec2) -> ListEvent {
        self.list.on_event(event, self.playlists.len(), modal_list_rect(size, LIST_TOP))
    }

    pub(super) fn draw(&self, printer: &Printer) {
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Add to Playlist", p.size.x));
        });

        if self.playlists.is_empty() {
            printer.print((0, LIST_TOP), "(no playlists — :newplaylist <name> to make one)");
        }
        let lines: Vec<String> = self.playlists.iter().map(|p| p.name.clone()).collect();
        self.list.draw(&printer.windowed(modal_list_rect(printer.size, LIST_TOP)), &lines);

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad("  [Enter] add to selected playlist   [Esc] cancel", p.size.x));
        });
    }
}

impl MedleyView {
    /// Rebuilds the picker's playlist list if the session moved on since it
    /// was (re)built — called from `required_size`, mirroring `refresh_help`.
    pub(super) fn refresh_playlist_picker(&mut self, revision: u64) {
        if !self.playlist_picker.as_ref().is_some_and(|p| stale(p.built_at(), revision)) {
            return;
        }
        let playlists = self.with_session(|s| s.playlists());
        if let Some(picker) = &mut self.playlist_picker {
            picker.refresh(playlists, revision);
        }
    }

    pub(super) fn on_playlist_picker_event(&mut self, event: &Event) -> EventResult {
        let size = self.last_screen_size;
        let Some(picker) = &mut self.playlist_picker else { return EventResult::Ignored };
        match picker.on_event(event, size) {
            ListEvent::Close => {
                self.playlist_picker = None;
                self.focus = self.fallback_focus();
                EventResult::consumed()
            }
            ListEvent::Activate | ListEvent::Clicked => {
                let (track, playlist) = (picker.track, picker.playlists.get(picker.list.cursor).map(|p| p.id));
                self.playlist_picker = None;
                self.focus = self.fallback_focus();
                match playlist {
                    Some(playlist) => self.run(Command::AddToPlaylist { track, playlist }),
                    None => EventResult::consumed(),
                }
            }
            ListEvent::Moved | ListEvent::Unhandled => EventResult::consumed(),
        }
    }
}
