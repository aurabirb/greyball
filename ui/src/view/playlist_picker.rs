use cursive::{Printer, Rect, Vec2};
use cursive::event::{Event, MouseButton, MouseEvent};
use unicode_width::UnicodeWidthStr;

use core::{Command, Playlist, Session, TrackId};

use super::memo::Memo;
use super::modal::{ModalOutcome, draw_modal_frame, modal_list};
use super::panes::{draw_float_frame, float_body};
use super::scroll::{ListEvent, ListState};

const TITLE: &str = "Add to Playlist";
const FOOTER: &str = "  [Enter] add to selected playlist   [Esc] cancel";
const EMPTY: &str = "(no playlists — :newplaylist <name> to make one)";
const MAX_LIST_ROWS: usize = 12;
const MAX_WIDTH: usize = 70;

/// The "Add to Playlist" picker (`+` with a track selected); exists only while open.
pub(super) struct PlaylistPicker {
    list: ListState,
    /// Captured when the picker opens, so it stays fixed if the underlying list scrolls.
    track: TrackId,
    playlists: Vec<Playlist>,
    /// The `playlists_gen` that `playlists` was read under.
    built: Memo<u64>,
}

impl PlaylistPicker {
    pub(super) fn new(track: TrackId, s: &Session) -> Self {
        let built = Memo::default();
        built.changed(s.playlists_gen());
        Self { list: ListState::default(), track, playlists: s.playlists(), built }
    }

    /// The border box, sized to the content and centred on `screen`.
    fn frame(&self, screen: Vec2) -> Rect {
        let widest = self.playlists.iter().map(|p| p.name.width()).max().unwrap_or(EMPTY.width());
        let inner_w = widest.max(FOOTER.width()).max(TITLE.width());
        let inner_h = 3 + self.playlists.len().clamp(1, MAX_LIST_ROWS);
        let size = Vec2::new((inner_w + 4).min(MAX_WIDTH), inner_h + 2).or_min(screen);
        Rect::from_size((screen - size) / 2, size)
    }

    fn rect(&self, screen: Vec2) -> Rect {
        float_body(self.frame(screen))
    }

    /// Re-reads the playlists once they changed, keeping the cursor on the same playlist and in view.
    pub(super) fn relayout(&mut self, resized: bool, screen: Vec2, s: &Session) {
        if self.built.changed(s.playlists_gen()) {
            let selected = self.playlists.get(self.list.cursor).map(|p| p.id);
            self.playlists = s.playlists();
            self.list.cursor = selected
                .and_then(|id| self.playlists.iter().position(|p| p.id == id))
                .unwrap_or_else(|| self.list.cursor.min(self.playlists.len().saturating_sub(1)));
        }
        let view_h = modal_list(self.rect(screen)).height();
        self.list.follow(view_h);
        self.list.relayout(resized, self.playlists.len(), view_h);
    }

    pub(super) fn on_event(&mut self, event: &Event, screen: Vec2) -> ModalOutcome {
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event
            && position.checked_sub(*offset).is_none_or(|pos| !self.frame(screen).contains(pos))
        {
            return ModalOutcome::Close;
        }
        match self.list.on_event(event, self.playlists.len(), modal_list(self.rect(screen))) {
            ListEvent::Close => ModalOutcome::Close,
            ListEvent::Activate | ListEvent::Clicked => match self.playlists.get(self.list.cursor) {
                Some(p) => ModalOutcome::Run(Command::AddToPlaylist { track: self.track, playlist: p.id }),
                None => ModalOutcome::Close,
            },
            ListEvent::Moved | ListEvent::Unhandled => ModalOutcome::Stay,
        }
    }

    pub(super) fn draw(&self, printer: &Printer, screen: Vec2) {
        draw_float_frame(printer, self.frame(screen));
        let rect = self.rect(screen);
        let body = draw_modal_frame(printer, rect, Some(TITLE), FOOTER);
        if self.playlists.is_empty() {
            body.print((0, 1), EMPTY);
        }
        let lines: Vec<String> = self.playlists.iter().map(|p| p.name.clone()).collect();
        self.list.draw(&printer.windowed(modal_list(rect)), &lines);
    }
}
