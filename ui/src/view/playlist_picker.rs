use cursive::{Printer, Vec2};
use cursive::event::{Event, EventResult};
use cursive::theme::ColorStyle;

use core::{Command, Playlist, TrackId};

use super::MedleyView;
use super::scroll::{ListEvent, ListState, modal_list_rect};
use super::text::pad;

/// Row the picker's list starts on (row 0 = title, row 1 = blank spacer).
const LIST_TOP: usize = 2;

/// The "Add to Playlist" picker (`+` with a track selected); exists only while open.
pub(super) struct PlaylistPicker {
    list: ListState,
    /// Captured when the picker opens, so it stays fixed if the underlying list scrolls.
    track: TrackId,
}

impl PlaylistPicker {
    pub(super) fn new(track: TrackId) -> Self {
        Self { list: ListState::default(), track }
    }

    pub(super) fn relayout(&mut self, resized: bool, size: Vec2, len: usize) {
        self.list.relayout(resized, len, modal_list_rect(size, LIST_TOP).height());
    }

    pub(super) fn on_event(&mut self, event: &Event, size: Vec2, len: usize) -> ListEvent {
        self.list.on_event(event, len, modal_list_rect(size, LIST_TOP))
    }

    pub(super) fn draw(&self, printer: &Printer, playlists: &[Playlist]) {
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Add to Playlist", p.size.x));
        });

        if playlists.is_empty() {
            printer.print((0, LIST_TOP), "(no playlists — :newplaylist <name> to make one)");
        }
        let lines: Vec<String> = playlists.iter().map(|p| p.name.clone()).collect();
        self.list.draw(&printer.windowed(modal_list_rect(printer.size, LIST_TOP)), &lines);

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad("  [Enter] add to selected playlist   [Esc] cancel", p.size.x));
        });
    }
}

impl MedleyView {
    pub(super) fn on_playlist_picker_event(&mut self, event: &Event) -> EventResult {
        let playlists = self.with_session(|s| s.playlists());
        let size = self.last_screen_size;
        let Some(picker) = &mut self.playlist_picker else { return EventResult::Ignored };
        match picker.on_event(event, size, playlists.len()) {
            ListEvent::Close => {
                self.playlist_picker = None;
                self.focus = self.fallback_focus();
                EventResult::consumed()
            }
            ListEvent::Activate | ListEvent::Clicked => {
                let (track, playlist) = (picker.track, playlists.get(picker.list.cursor).map(|p| p.id));
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
