use cursive::Printer;
use cursive::event::EventResult;
use cursive::theme::ColorStyle;

use core::{Command, TrackId};

use super::MedleyView;
use super::scroll::{CursorWindow, modal_list_h};
use super::text::pad;

/// Row the "Add to Playlist" picker's list starts on — same shape as `HOTKEY_LIST_TOP`.
pub(super) const PLAYLIST_PICKER_LIST_TOP: usize = 2;

impl MedleyView {
    /// Fullscreen "Add to Playlist" picker (`+` with a track selected).
    pub(super) fn draw_playlist_picker(&self, printer: &Printer) {
        let playlists = self.with_session(|s| s.playlists());
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Add to Playlist", p.size.x));
        });

        if playlists.is_empty() {
            printer.print(
                (0, PLAYLIST_PICKER_LIST_TOP),
                "(no playlists — :newplaylist <name> to make one)",
            );
        }
        let lines: Vec<String> = playlists.iter().map(|p| p.name.clone()).collect();
        self.draw_rows(
            printer,
            &lines,
            self.playlist_picker_cursor,
            self.playlist_picker_offset,
            PLAYLIST_PICKER_LIST_TOP,
        );

        let bottom = printer.size.y.saturating_sub(1);
        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad("  [Enter] add to selected playlist   [Esc] cancel", p.size.x));
        });
    }

    /// Same idea as `jump_warnings`, for the "Add to Playlist" picker.
    pub(super) fn jump_playlist_picker(&mut self, up: bool, step: usize) {
        let n = self.with_session(|s| s.playlists().len());
        let h = modal_list_h(self.last_screen_size.y, PLAYLIST_PICKER_LIST_TOP);
        CursorWindow { cursor: &mut self.playlist_picker_cursor, offset: &mut self.playlist_picker_offset }
            .jump(up, step, n, h);
    }

    /// Same idea as `follow_warnings_offset`, for the "Add to Playlist" picker.
    pub(super) fn follow_playlist_picker_offset(&mut self) {
        let h = modal_list_h(self.last_screen_size.y, PLAYLIST_PICKER_LIST_TOP);
        CursorWindow { cursor: &mut self.playlist_picker_cursor, offset: &mut self.playlist_picker_offset }
            .follow(h);
    }

    pub(super) fn open_playlist_picker(&mut self, track: TrackId) {
        self.playlist_picker_open = true;
        self.playlist_picker_cursor = 0;
        self.playlist_picker_offset = 0;
        self.playlist_picker_track = Some(track);
    }

    /// Enter (or a click) on the selected picker row.
    pub(super) fn commit_playlist_picker(&mut self) -> EventResult {
        let track = self.playlist_picker_track.take();
        let playlist =
            self.with_session(|s| s.playlists().into_iter().nth(self.playlist_picker_cursor).map(|p| p.id));
        self.playlist_picker_open = false;
        self.focus = self.fallback_focus();
        match (track, playlist) {
            (Some(track), Some(playlist)) => self.run(Command::AddToPlaylist { track, playlist }),
            _ => EventResult::consumed(),
        }
    }
}
