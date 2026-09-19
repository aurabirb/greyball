use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cursive::{Printer, Rect};
use cursive::event::{Event, MouseEvent};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use core::{BrowseNode, Command, HotkeyTarget, Playlist, PlaylistId, Session, SourceId, TrackId};

use crate::row::RowItem;
use crate::screen::Screen;

use super::memo::Memo;
use super::rows::{Cell, LIST_TITLE_ROWS, Row, draw_row_list, plain_row, tracks_to_rows};
use super::scroll::{ListEvent, ListState, Nav, WHEEL_STEP};
use super::window::{Ctx, WindowOutcome};

/// Two clicks on the same row within this long count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// One row of a Playlists window's top-level list.
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

/// Every local playlist, then every source's landed remote playlists.
pub(super) fn top_rows(s: &Session) -> Vec<TopRow> {
    let mut rows: Vec<TopRow> = s.playlists().into_iter().map(|p| TopRow::Local(p.id)).collect();
    for sid in s.source_ids() {
        for (name, node) in s.remote_playlists(&sid) {
            rows.push(TopRow::Remote(sid.clone(), name, node));
        }
    }
    rows
}

/// Where a Playlists window is; every other kind stays at `TopLevel`.
#[derive(Default)]
enum Open {
    #[default]
    TopLevel,
    Local(PlaylistId),
    /// Source, display name, node.
    Remote(SourceId, String, BrowseNode),
}

/// The `/`-filter's rank for one row, low-to-high.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct FilterRank(u8, std::cmp::Reverse<i64>);

fn rank_filter(matcher: &SkimMatcherV2, text: &str, query: &str) -> Option<FilterRank> {
    let text = text.to_lowercase();
    let query = query.to_lowercase();
    let score = matcher.fuzzy_match(&text, &query)?;
    let tier = if text.contains(&query) { 0 } else { 1 };
    Some(FilterRank(tier, std::cmp::Reverse(score)))
}

/// One list window's rendered rows plus what the shell's readout shows about it.
pub(super) struct ListFrame {
    title: String,
    rows: Vec<Row>,
    pub(super) total: usize,
    /// A paginated remote list only knows what it has loaded so far.
    pub(super) loading: bool,
    pub(super) unit: &'static str,
    /// Whether a playlist that can take a hotkey is selected or open.
    pub(super) hotkey_target: bool,
}

/// A track-list window of one kind: its cursor, which list it is in, its `/`-filter and its memos.
pub(super) struct TrackList {
    kind: Screen,
    state: ListState,
    open: Open,
    /// The `/`-filter, live while typed; never empty.
    query: Option<String>,
    /// Bumped whenever `open` or `query` changes — the list-identity part of every memo key.
    view_gen: u64,
    /// When and on which row the last left click landed.
    last_click: Option<(Instant, usize)>,
    /// Ranked ids, not `Track`s, so an attrs patch can't go stale in it.
    matches: Memo<(u64, u64), Arc<[TrackId]>>,
    frame: Memo<(u64, u64, usize, usize, bool), Arc<ListFrame>>,
}

impl TrackList {
    pub(super) fn new(kind: Screen) -> Self {
        Self {
            kind,
            state: ListState::default(),
            open: Open::default(),
            query: None,
            view_gen: 0,
            last_click: None,
            matches: Memo::default(),
            frame: Memo::default(),
        }
    }

    pub(super) fn cursor(&self) -> usize {
        self.state.cursor
    }

    /// `(view_gen, cursor)`: moves whenever what `visible_track_ids` or the selection means does.
    pub(super) fn follow_key(&self) -> (u64, usize) {
        (self.view_gen, self.state.cursor)
    }

    /// A different list under the old cursor and filter would be meaningless.
    fn reset_for_new_list(&mut self, open: Open) {
        self.open = open;
        self.query = None;
        self.state = ListState::default();
        self.view_gen += 1;
    }

    pub(super) fn open_remote(&mut self, sid: SourceId, name: String, node: BrowseNode) {
        self.reset_for_new_list(Open::Remote(sid, name, node));
    }

    /// Sets the `/`-filter and, when it changed, restarts the selection at the top.
    pub(super) fn set_query(&mut self, query: Option<&str>) {
        let query = query.filter(|q| !q.is_empty());
        if self.query.as_deref() != query {
            self.query = query.map(str::to_string);
            self.state = ListState::default();
            self.view_gen += 1;
        }
    }

    pub(super) fn is_search(&self) -> bool {
        self.kind == Screen::Search
    }

    /// Whether the `/`-filter narrows this list.
    fn filterable(&self) -> bool {
        match self.kind {
            Screen::NowPlaying | Screen::Queue | Screen::History => true,
            Screen::Playlists => !matches!(self.open, Open::TopLevel),
            Screen::Search => false,
        }
    }

    /// The generation, held by its owner, of the list on screen, plus that of track ids vanishing from any list.
    pub(super) fn list_gen(&self, s: &Session) -> u64 {
        s.removed_tracks_gen() + match (self.kind, &self.open) {
            (Screen::NowPlaying, _) => s.context_gen(),
            (Screen::Search, _) => s.results_gen(),
            (Screen::Queue, _) => s.queue.queue_gen(),
            (Screen::History, _) => s.queue.history_gen(),
            (Screen::Playlists, Open::Local(_)) => s.playlists_gen(),
            (Screen::Playlists, Open::Remote(sid, _, node)) => s.remote_playlist_gen(sid, node),
            (Screen::Playlists, Open::TopLevel) => 0,
        }
    }

    /// Every track already loaded for the list, unwindowed.
    fn all_tracks(&self, s: &Session) -> Vec<core::Track> {
        match (self.kind, &self.open) {
            (Screen::NowPlaying, _) => s.playing_context_window(0, s.playing_context_len()),
            (Screen::Queue, _) => s.queue_window(0, s.queue_len()),
            (Screen::History, _) => s.history_window(0, s.queue.history_len()),
            (Screen::Playlists, Open::Local(id)) => s.playlist_window(*id, 0, s.playlist_len(*id)),
            (Screen::Playlists, Open::Remote(sid, _, node)) => {
                s.remote_playlist_window(sid, node, 0, s.remote_playlist_len(sid, node))
            }
            (Screen::Playlists, Open::TopLevel) | (Screen::Search, _) => vec![],
        }
    }

    /// The list's ids narrowed and ranked by the filter; rows resolve them fresh via `Session::tracks_for`.
    fn filtered_ids(&self, s: &Session) -> Option<Arc<[TrackId]>> {
        let query = self.query.as_deref().filter(|_| self.filterable())?;
        Some(self.matches.get_or_build((self.list_gen(s), self.view_gen), || {
            let matcher = SkimMatcherV2::default();
            let mut ranked: Vec<(TrackId, FilterRank)> = self
                .all_tracks(s)
                .into_iter()
                .filter_map(|t| rank_filter(&matcher, &t.main(), query).map(|r| (t.id, r)))
                .collect();
            ranked.sort_by(|a, b| a.1.cmp(&b.1));
            ranked.into_iter().map(|(id, _)| id).collect()
        }))
    }

    /// Track ids on screen, in display order.
    pub(super) fn visible_track_ids(&self, s: &Session) -> Vec<TrackId> {
        if let Some(ids) = self.filtered_ids(s) {
            return ids.to_vec();
        }
        match (self.kind, &self.open) {
            (Screen::NowPlaying, _) => s.playing_context_ids(),
            (Screen::Search, _) => s.results_ids(),
            (Screen::Queue, _) => s.queue_ids(),
            (Screen::History, _) => s.history_ids(),
            (Screen::Playlists, Open::Local(id)) => s.playlist_track_ids(*id),
            (Screen::Playlists, Open::Remote(sid, _, node)) => s.remote_playlist_track_ids(sid, node),
            (Screen::Playlists, Open::TopLevel) => vec![],
        }
    }

    pub(super) fn selected_track(&self, s: &Session) -> Option<TrackId> {
        self.visible_track_ids(s).get(self.state.cursor).copied()
    }

    /// The open local playlist.
    pub(super) fn open_local(&self) -> Option<PlaylistId> {
        match self.open {
            Open::Local(id) => Some(id),
            _ => None,
        }
    }

    /// The playlist (local or remote) selected or open, if this is a Playlists window.
    pub(super) fn selected_hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        match (self.kind, &self.open) {
            (Screen::Playlists, Open::Local(id)) => Some(HotkeyTarget::Local(*id)),
            (Screen::Playlists, Open::Remote(sid, _, node)) => Some(HotkeyTarget::Remote(sid.clone(), node.clone())),
            (Screen::Playlists, Open::TopLevel) => top_rows(s).get(self.state.cursor).map(TopRow::target),
            _ => None,
        }
    }

    /// The full list length.
    pub(super) fn len(&self, s: &Session) -> usize {
        if let Some(ids) = self.filtered_ids(s) {
            return ids.len();
        }
        match (self.kind, &self.open) {
            (Screen::NowPlaying, _) => s.playing_context_len(),
            (Screen::Search, _) => s.results_len(),
            (Screen::Queue, _) => s.queue_len(),
            (Screen::History, _) => s.queue.history_len(),
            (Screen::Playlists, Open::Local(id)) => s.playlist_len(*id),
            (Screen::Playlists, Open::Remote(sid, _, node)) => s.remote_playlist_len(sid, node),
            (Screen::Playlists, Open::TopLevel) => top_rows(s).len(),
        }
    }

    /// The list's display name as a playback context.
    fn context_name(&self, s: &Session) -> Option<String> {
        match (self.kind, &self.open) {
            (Screen::NowPlaying, _) => s.playing_context_name(),
            (Screen::Search, _) => Some("Search results".to_string()),
            (Screen::Queue, _) => Some("Queue".to_string()),
            (Screen::History, _) => Some("History".to_string()),
            (Screen::Playlists, Open::Local(id)) => s.playlists().into_iter().find(|p| p.id == *id).map(|p| p.name),
            (Screen::Playlists, Open::Remote(_, name, _)) => Some(name.clone()),
            (Screen::Playlists, Open::TopLevel) => None,
        }
    }

    /// Plays the row under the cursor, else opens the top-level playlist under it.
    fn activate(&mut self, s: &Session) -> WindowOutcome {
        let tracks = self.visible_track_ids(s);
        let index = self.state.cursor;
        if index < tracks.len() {
            let remote = match &self.open {
                Open::Remote(sid, _, node) => Some((sid.clone(), node.clone())),
                _ => None,
            };
            return WindowOutcome::Run(Command::PlayContext { tracks, index, remote, name: self.context_name(s) });
        }
        if self.kind != Screen::Playlists || !matches!(self.open, Open::TopLevel) {
            return WindowOutcome::Ignored;
        }
        match top_rows(s).into_iter().nth(self.state.cursor) {
            Some(TopRow::Local(id)) => self.reset_for_new_list(Open::Local(id)),
            Some(TopRow::Remote(sid, name, node)) => self.reset_for_new_list(Open::Remote(sid, name, node)),
            None => return WindowOutcome::Ignored,
        }
        WindowOutcome::Consumed
    }

    fn unit(&self, count: usize) -> &'static str {
        let playlists = self.kind == Screen::Playlists && matches!(self.open, Open::TopLevel);
        match (playlists, count == 1) {
            (true, true) => "playlist",
            (true, false) => "playlists",
            (false, true) => "track",
            (false, false) => "tracks",
        }
    }

    /// The title row: `<name>`, optionally followed by `  (<hint>)`.
    fn title(&self, s: &Session, total: usize, searching: bool) -> String {
        if let Some(query) = self.query.as_deref().filter(|_| self.filterable()) {
            let plural = if total == 1 { "" } else { "es" };
            return format!("filter {query:?} ({total} match{plural})");
        }
        let (name, hint) = match (self.kind, &self.open) {
            (Screen::NowPlaying, _) => (s.playing_context_name(), None),
            (Screen::Search, _) => (s.results_query().map(str::to_string), searching.then_some("Esc to cancel")),
            (Screen::Playlists, Open::Local(_)) => (self.context_name(s), Some("Esc to go back")),
            (Screen::Playlists, Open::Remote(sid, name, _)) => (Some(format!("[{sid}] {name}")), Some("Esc to go back")),
            _ => (None, None),
        };
        let name = name.unwrap_or_else(|| format!("{} ({total} {})", self.kind.label(), self.unit(total)));
        match hint {
            Some(hint) => format!("{name}  ({hint})"),
            None => name,
        }
    }

    /// Resolves only the visible `offset`/`limit` window — a list can run into the thousands.
    fn rows(&self, s: &Session, offset: usize, limit: usize) -> Vec<Row> {
        let pending: HashSet<TrackId> = match &self.open {
            Open::Remote(sid, _, node) => s.remote_pending_ids(sid, node).into_iter().collect(),
            _ => HashSet::new(),
        };
        let track_rows = |tracks| tracks_to_rows(s, tracks, &pending);
        if let Some(matched) = self.filtered_ids(s) {
            if matched.is_empty() {
                return vec![plain_row(format!("no matches for {:?}", self.query.as_deref().unwrap_or_default()))];
            }
            let window: Vec<TrackId> = matched.iter().skip(offset).take(limit).copied().collect();
            return track_rows(s.tracks_for(&window));
        }
        match (self.kind, &self.open) {
            (Screen::NowPlaying, _) if s.playing_context_len() == 0 => {
                vec![plain_row("nothing played yet — press Enter on a track to start playing")]
            }
            (Screen::NowPlaying, _) => track_rows(s.playing_context_window(offset, limit)),
            (Screen::Search, _) if s.results_len() == 0 => match s.results_query() {
                // A search ran and came back empty — say so.
                Some(q) => vec![plain_row(format!(
                    "no results for {q:?} — check the Log pane (:log) for source errors"
                ))],
                None => vec![],
            },
            (Screen::Search, _) => track_rows(s.results_window(offset, limit)),
            (Screen::Queue, _) => track_rows(s.queue_window(offset, limit)),
            (Screen::History, _) => track_rows(s.history_window(offset, limit)),
            (Screen::Playlists, Open::Local(id)) => track_rows(s.playlist_window(*id, offset, limit)),
            (Screen::Playlists, Open::Remote(sid, _, node)) => {
                track_rows(s.remote_playlist_window(sid, node, offset, limit))
            }
            (Screen::Playlists, Open::TopLevel) => {
                let playlists = s.playlists();
                top_rows(s)
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|row| {
                        let mut r = match &row {
                            TopRow::Local(id) => {
                                let p = playlists.iter().find(|p| p.id == *id);
                                let name = p.map(|p| p.name.clone()).unwrap_or_default();
                                let count = p.map(|p| p.items.len()).unwrap_or(0);
                                plain_row(format!("{name}  ({count} tracks)"))
                            }
                            TopRow::Remote(sid, name, _) => plain_row(format!("[{sid}] {name}")),
                        };
                        let key = s.playlist_hotkey(&row.target());
                        r.hotkeys = Cell::plain(key.map(String::from).unwrap_or_default());
                        r
                    })
                    .collect()
            }
        }
    }

    fn loading(&self, s: &Session) -> bool {
        match (self.kind, &self.open) {
            (Screen::Playlists, Open::Remote(sid, _, node)) => s.remote_playlist_loading(sid, node),
            (Screen::Playlists, Open::TopLevel) => s.source_ids().iter().any(|sid| s.remote_playlists_loading(sid)),
            _ => false,
        }
    }

    /// The rows under the title row of `rect` — the one layout `frame`, `draw`, `on_event` and `relayout` share.
    fn body(rect: Rect) -> Rect {
        let top = LIST_TITLE_ROWS.min(rect.height());
        Rect::from_size((rect.left(), rect.top() + top), (rect.width(), rect.height() - top))
    }

    /// This window's rows for a `rect`-sized window, rebuilt only when something they read changed.
    pub(super) fn frame(&self, ctx: &Ctx, rect: Rect) -> Arc<ListFrame> {
        let (s, view_h) = (ctx.s, Self::body(rect).height());
        let key = (s.revision(), self.view_gen, self.state.offset, view_h, ctx.searching);
        self.frame.get_or_build(key, || {
            let total = self.len(s);
            Arc::new(ListFrame {
                title: self.title(s, total, ctx.searching),
                rows: self.rows(s, self.state.offset, view_h),
                total,
                loading: self.loading(s),
                unit: self.unit(total),
                hotkey_target: self.kind == Screen::Playlists && (total > 0 || !matches!(self.open, Open::TopLevel)),
            })
        })
    }

    /// `marked` brackets the title, the focus marker docked windows use.
    pub(super) fn draw(&self, printer: &Printer, marked: bool, frame: &ListFrame) {
        let title = if marked { format!("[{}]", frame.title) } else { frame.title.clone() };
        draw_row_list(printer, &title, &frame.rows, self.state.offset, self.state.cursor, frame.total);
    }

    /// Re-follows the cursor when `resized`, else keeps cursor and scroll window inside the list.
    pub(super) fn relayout(&mut self, resized: bool, s: &Session, rect: Rect) {
        let len = self.len(s);
        self.state.cursor = self.state.cursor.min(len.saturating_sub(1));
        self.state.relayout(resized, len, Self::body(rect).height());
    }

    /// Brings the scroll window back around the cursor.
    pub(super) fn follow(&mut self, rect: Rect) {
        self.state.follow(Self::body(rect).height());
    }

    /// Nav keys, Enter, Esc out of an open playlist, wheel and clicks inside `rect`; anything else is `Ignored`.
    pub(super) fn on_event(&mut self, event: &Event, ctx: &Ctx, rect: Rect) -> WindowOutcome {
        let (s, body) = (ctx.s, Self::body(rect));
        if let Event::Mouse { offset, position, event: mouse } = event {
            if !position.checked_sub(*offset).is_some_and(|pos| rect.contains(pos)) {
                return WindowOutcome::Ignored;
            }
            if matches!(mouse, MouseEvent::Hold(_) | MouseEvent::Release(_)) {
                return WindowOutcome::Ignored;
            }
        }
        // The wheel scrolls the window only; the next key press re-follows the cursor.
        if let Some(Nav::Wheel(up)) = Nav::of(event) {
            self.state.scroll(up, WHEEL_STEP, self.len(s), body.height());
            return WindowOutcome::Consumed;
        }
        match self.state.on_event(event, self.len(s), body) {
            ListEvent::Moved => WindowOutcome::Consumed,
            ListEvent::Activate => self.activate(s),
            ListEvent::Clicked => {
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(t, row)| row == self.state.cursor && now.duration_since(t) <= DOUBLE_CLICK_WINDOW);
                // A third click must not chain into another double.
                self.last_click = (!double).then_some((now, self.state.cursor));
                if double { self.activate(s) } else { WindowOutcome::Consumed }
            }
            ListEvent::Close if !matches!(self.open, Open::TopLevel) => {
                self.reset_for_new_list(Open::TopLevel);
                WindowOutcome::Consumed
            }
            ListEvent::Close => WindowOutcome::Ignored,
            ListEvent::Unhandled if matches!(event, Event::Mouse { .. }) => WindowOutcome::Consumed,
            ListEvent::Unhandled => WindowOutcome::Ignored,
        }
    }
}
