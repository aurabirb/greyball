use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cursive::{Printer, Rect};
use cursive::event::{Event, Key, MouseButton, MouseEvent};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use core::{BrowseNode, Command, HotkeyTarget, ItemKind, ListRef, PendingRows, Playlist, PlaylistId, Session, SourceId, TrackId};

use crate::keybindings;
use crate::row::RowItem;
use crate::screen::{ListKind, Placement};

use super::memo::Memo;
use super::kind_bar::{self, KindFilter};
use super::text::scroll_title;
use super::rows::{BACK_LABEL, main_col_start, Cell, LIST_TITLE_ROWS, Row, back_button_fits, draw_row_list, plain_row, tracks_to_rows};
use super::scroll::{ListEvent, ListState, Nav, WHEEL_STEP};
use super::window::{Ctx, StatusCtx, WindowOutcome, hint};

/// Two clicks on the same row within this long count as a double-click.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// One row of a Playlists window's top-level list.
#[derive(Clone)]
pub(super) enum TopRow {
    Local(PlaylistId),
    Remote(SourceId, String, BrowseNode, ItemKind),
}

impl TopRow {
    /// This row's `HotkeyTarget` — what a playlist hotkey binds to.
    pub(super) fn target(&self) -> HotkeyTarget {
        match self {
            TopRow::Local(id) => HotkeyTarget::Local(*id),
            TopRow::Remote(sid, _, node, _) => HotkeyTarget::Remote(sid.clone(), node.clone()),
        }
    }

    fn kind(&self) -> ItemKind {
        match self {
            TopRow::Local(_) => ItemKind::Playlist,
            TopRow::Remote(.., kind) => *kind,
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
        TopRow::Remote(sid, name, ..) => format!("[{sid}] {name}"),
    }
}

/// A bound playlist's display name; a remote one not in its source's landed list shows its id.
pub(super) fn hotkey_target_name(s: &Session, target: &HotkeyTarget) -> String {
    let playlists = s.playlists();
    match top_rows(s).into_iter().find(|r| &r.target() == target) {
        Some(row) => top_row_name(&row, &playlists),
        None => match target {
            HotkeyTarget::Remote(sid, BrowseNode::Path(id)) => format!("[{sid}] {id}"),
            _ => String::new(),
        },
    }
}

/// Every local playlist, then every source's landed remote playlists and saved albums.
pub(super) fn top_rows(s: &Session) -> Vec<TopRow> {
    let mut rows: Vec<TopRow> = s.playlists().into_iter().map(|p| TopRow::Local(p.id)).collect();
    for sid in s.source_ids() {
        for (name, node) in s.remote_playlists(&sid) {
            rows.push(TopRow::Remote(sid.clone(), name, node, ItemKind::Playlist));
        }
        for (name, node) in s.remote_albums(&sid) {
            rows.push(TopRow::Remote(sid.clone(), name, node, ItemKind::Album));
        }
    }
    rows
}

/// Where a window is: a Playlists window at its top level or in a playlist, a Search window in an opened collection; every other kind stays at `TopLevel`.
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

/// Revision, view generation, scroll offset, cursor, body height, whether searching.
type FrameKey = (u64, u64, usize, usize, usize, bool);

/// One list window's rendered rows plus what the shell's readout shows about it.
pub(super) struct ListFrame {
    title: String,
    rows: Vec<Row>,
    total: usize,
    /// A paginated remote list only knows what it has loaded so far.
    loading: bool,
    unit: &'static str,
    /// A keypress binds the playlist row under the cursor.
    pub(super) assignable: bool,
    /// The row under the cursor already has a key.
    keyed: bool,
    /// Items per kind, parallel to the window's `segments`.
    kind_counts: Option<Vec<usize>>,
}

/// A track-list window of one kind: its cursor, which list it is in, its `/`-filter and its memos.
pub(super) struct TrackList {
    kind: ListKind,
    state: ListState,
    open: Open,
    /// The `/`-filter, live while typed; never empty.
    query: Option<String>,
    /// Search only: which result kinds are listed.
    kinds: KindFilter,
    /// Bumped whenever `open`, `query` or `kinds` changes — the list-identity part of every memo key.
    view_gen: u64,
    /// The top level lists the playlists that have a key first.
    keyed_first: bool,
    /// The playlist whose top-level row the next layout pass puts the cursor on.
    select: Option<HotkeyTarget>,
    /// Search only: the collection under the cursor, the cursor row and list identity it was read at; results streaming in shift its row.
    anchor: Option<(u64, usize, HotkeyTarget)>,
    /// Keyed-first only: the unkeyed playlist a key was just pressed for, and the playlist listed after it.
    assigned: Option<(HotkeyTarget, HotkeyTarget)>,
    /// When and on which row the last left click landed.
    last_click: Option<(Instant, usize)>,
    /// Ranked ids, not `Track`s, so an attrs patch can't go stale in it.
    matches: Memo<(u64, u64), Arc<[TrackId]>>,
    frame: Memo<FrameKey, Arc<ListFrame>>,
    /// The top-level rows in this window's order, keyed on the playlists, remote playlists and hotkeys generations and the view generation (kind filter).
    top: Memo<(u64, u64, u64, u64), Arc<[TopRow]>>,
    /// A Playlists top level's kind-bar counts, all rows unfiltered.
    top_counts: Memo<(u64, u64, u64), Vec<usize>>,
    /// Search only: the collection rows after the tracks, albums then playlists, keyed on the results and view generations.
    collections: Memo<(u64, u64), Arc<[TopRow]>>,
}

impl TrackList {
    pub(super) fn new(kind: ListKind, keyed_first: bool) -> Self {
        Self {
            kind,
            keyed_first,
            select: None,
            anchor: None,
            assigned: None,
            top: Memo::default(),
            top_counts: Memo::default(),
            collections: Memo::default(),
            kinds: KindFilter::default(),
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

    pub(super) fn cycle_kinds(&mut self) {
        self.set_kinds(self.kinds.next(self.segments()));
    }

    /// The kinds the kind bar offers this window.
    fn segments(&self) -> &'static [KindFilter] {
        if self.kind == ListKind::Search { &KindFilter::ALL } else { &KindFilter::COLLECTIONS }
    }

    fn set_kinds(&mut self, kinds: KindFilter) {
        if self.has_kind_bar() && kinds != self.kinds {
            self.kinds = kinds;
            self.state = ListState::default();
            self.view_gen += 1;
        }
    }

    fn kind_counts(&self, s: &Session) -> Option<Vec<usize>> {
        if !self.has_kind_bar() {
            return None;
        }
        if self.kind == ListKind::Playlists {
            return Some(self.top_counts.get_or_build((s.playlists_gen(), s.remote_playlists_gen(), 0), || {
                let rows = top_rows(s);
                let of = |kind| rows.iter().filter(|row| row.kind() == kind).count();
                vec![rows.len(), of(ItemKind::Album), of(ItemKind::Playlist)]
            }));
        }
        let hits = s.search_collections();
        let of = |kind| hits.iter().filter(|hit| hit.1 == kind).count();
        let (songs, albums, playlists) = (s.results_len(), of(ItemKind::Album), of(ItemKind::Playlist));
        Some(vec![songs + albums + playlists, songs, albums, playlists])
    }

    fn bar(&self, counts: Option<&[usize]>, content_w: usize) -> Vec<kind_bar::Segment> {
        counts.map(|counts| kind_bar::layout(content_w, self.segments(), counts, self.kinds)).unwrap_or_default()
    }

    /// A Search window at its top level: the list mixes tracks and collections.
    pub(super) fn is_results(&self) -> bool {
        self.kind == ListKind::Search && matches!(self.open, Open::TopLevel)
    }

    /// A Search or Playlists window at its top level: the title row carries the kind bar.
    fn has_kind_bar(&self) -> bool {
        matches!(self.kind, ListKind::Search | ListKind::Playlists) && matches!(self.open, Open::TopLevel)
    }

    /// Whether the `/`-filter narrows this list.
    fn filterable(&self) -> bool {
        match self.kind {
            ListKind::NowPlaying | ListKind::Queue | ListKind::History => true,
            ListKind::Playlists => !matches!(self.open, Open::TopLevel),
            ListKind::Search => !matches!(self.open, Open::TopLevel),
        }
    }

    /// The generation, held by its owner, of the list on screen, plus that of track ids vanishing from any list.
    pub(super) fn list_gen(&self, s: &Session) -> u64 {
        s.removed_tracks_gen() + match (self.kind, &self.open) {
            (ListKind::NowPlaying, _) => s.context_gen(),
            (_, Open::Remote(sid, _, node)) => s.remote_playlist_gen(sid, node),
            (ListKind::Search, _) => s.results_gen(),
            (ListKind::Queue, _) => s.queue.queue_gen(),
            (ListKind::History, _) => s.queue.history_gen(),
            (ListKind::Playlists, Open::Local(_)) => s.playlists_gen(),
            (ListKind::Playlists, Open::TopLevel) => 0,
        }
    }

    /// Every track already loaded for the list, unwindowed.
    fn all_tracks(&self, s: &Session) -> Vec<core::Track> {
        match (self.kind, &self.open) {
            (_, Open::Remote(sid, _, node)) => s.remote_playlist_window(sid, node, 0, s.remote_playlist_len(sid, node)),
            (ListKind::NowPlaying, _) => s.playing_context_window(0, s.playing_context_len()),
            (ListKind::Queue, _) => s.queue_window(0, s.queue_len()),
            (ListKind::History, _) => s.history_window(0, s.queue.history_len()),
            (ListKind::Playlists, Open::Local(id)) => s.playlist_window(*id, 0, s.playlist_len(*id)),
            (ListKind::Playlists, Open::TopLevel) | (ListKind::Search, _) => vec![],
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
            (_, Open::Remote(sid, _, node)) => s.remote_playlist_track_ids(sid, node),
            (ListKind::NowPlaying, _) => s.playing_context_ids(),
            (ListKind::Search, _) if self.kinds.shows_tracks() => s.results_ids(),
            (ListKind::Search, _) => vec![],
            (ListKind::Queue, _) => s.queue_ids(),
            (ListKind::History, _) => s.history_ids(),
            (ListKind::Playlists, Open::Local(id)) => s.playlist_track_ids(*id),
            (ListKind::Playlists, Open::TopLevel) => vec![],
        }
    }

    /// The row of this list that is the playing one; the single predicate marks and reveal share.
    fn playing_index(&self, s: &Session) -> Option<usize> {
        let own = match (self.kind, self.filtered_ids(s)) {
            (_, Some(_)) => ListRef::Other,
            (ListKind::NowPlaying, None) => ListRef::Context,
            (_, None) => self.open_target().map_or(ListRef::Other, ListRef::Playlist),
        };
        s.playing_row(&self.visible_track_ids(s), &own)
    }

    pub(super) fn open_row_target(&mut self, row: TopRow) {
        match row {
            TopRow::Local(id) => self.reset_for_new_list(Open::Local(id)),
            TopRow::Remote(sid, name, node, _) => self.reset_for_new_list(Open::Remote(sid, name, node)),
        }
    }

    /// Puts the cursor on `track`; false when the list does not hold it.
    pub(super) fn select_track(&mut self, s: &Session, track: TrackId) -> bool {
        let row = self.visible_track_ids(s).iter().position(|&id| id == track);
        if let Some(i) = row {
            self.state.cursor = i;
        }
        row.is_some()
    }

    /// Moves the cursor to the playing row; false when the list does not hold it.
    pub(super) fn reveal(&mut self, s: &Session) -> bool {
        let row = self.playing_index(s);
        if let Some(i) = row {
            self.state.cursor = i;
        }
        row.is_some()
    }

    /// The open playlist and the cursor row in it, while the list shows it whole (no filter): a row's position there.
    pub(super) fn open_row(&self, s: &Session) -> Option<(HotkeyTarget, usize)> {
        self.filtered_ids(s).is_none().then(|| self.open_target().map(|t| (t, self.state.cursor)))?
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

    /// The top-level rows as this window lists them: what the cursor, a click, Enter and a bound key all index.
    fn top(&self, s: &Session) -> Arc<[TopRow]> {
        self.top.get_or_build((s.playlists_gen(), s.remote_playlists_gen(), s.hotkeys_gen(), self.view_gen), || {
            let mut rows = top_rows(s);
            rows.retain(|row| self.kinds.admits(row.kind()));
            if self.keyed_first {
                rows.sort_by_key(|row| s.playlist_hotkey(&row.target()).is_none());
            }
            rows.into()
        })
    }

    /// A Search window's collection rows: albums, then playlists, as the kind filter admits.
    fn collections(&self, s: &Session) -> Arc<[TopRow]> {
        self.collections.get_or_build((s.results_gen(), self.view_gen), || {
            let hits = s.search_collections();
            [ItemKind::Album, ItemKind::Playlist]
                .into_iter()
                .filter(|&kind| self.kinds.admits(kind))
                .flat_map(|kind| hits.iter().filter(move |hit| hit.1 == kind))
                .map(|(sid, kind, name, node)| TopRow::Remote(sid.clone(), name.clone(), node.clone(), *kind))
                .collect()
        })
    }

    /// How many track rows a Search window lists before its collection rows.
    fn search_tracks(&self, s: &Session) -> usize {
        if self.kinds.shows_tracks() { s.results_len() } else { 0 }
    }

    fn top_row(&self, s: &Session) -> Option<TopRow> {
        if self.is_results() {
            let at = self.state.cursor.checked_sub(self.search_tracks(s))?;
            return self.collections(s).get(at).cloned();
        }
        self.top(s).get(self.state.cursor).cloned()
    }

    /// The open playlist.
    pub(super) fn open_target(&self) -> Option<HotkeyTarget> {
        match &self.open {
            Open::TopLevel => None,
            Open::Local(id) => Some(HotkeyTarget::Local(*id)),
            Open::Remote(sid, _, node) => Some(HotkeyTarget::Remote(sid.clone(), node.clone())),
        }
    }

    /// Puts the cursor on `target`'s row of the top level, if that is where this window is.
    pub(super) fn select_playlist(&mut self, target: HotkeyTarget) {
        if self.kind == ListKind::Playlists && matches!(self.open, Open::TopLevel) {
            self.select = Some(target);
        }
    }

    /// Back to the top level, the cursor on `target`, else on the playlist it leaves, else where it was.
    pub(super) fn show_top(&mut self, target: Option<HotkeyTarget>) {
        let left = self.open_target();
        if left.is_some() {
            self.reset_for_new_list(Open::TopLevel);
        }
        self.select = target.or(left);
    }

    /// The playlist (local or remote) selected or open, if this is a Playlists window.
    pub(super) fn selected_hotkey_target(&self, s: &Session) -> Option<HotkeyTarget> {
        match self.kind {
            ListKind::Playlists => self.open_target().or_else(|| self.top_row(s).as_ref().map(TopRow::target)),
            _ => None,
        }
    }

    /// The full list length.
    pub(super) fn len(&self, s: &Session) -> usize {
        if let Some(ids) = self.filtered_ids(s) {
            return ids.len();
        }
        match (self.kind, &self.open) {
            (_, Open::Remote(sid, _, node)) => s.remote_playlist_len(sid, node),
            (ListKind::NowPlaying, _) => s.playing_context_len(),
            (ListKind::Search, _) => self.search_tracks(s) + self.collections(s).len(),
            (ListKind::Queue, _) => s.queue_len(),
            (ListKind::History, _) => s.queue.history_len(),
            (ListKind::Playlists, Open::Local(id)) => s.playlist_len(*id),
            (ListKind::Playlists, Open::TopLevel) => self.top(s).len(),
        }
    }

    /// The list's display name as a playback context.
    fn context_name(&self, s: &Session) -> Option<String> {
        match (self.kind, &self.open) {
            (_, Open::Remote(_, name, _)) => Some(name.clone()),
            (ListKind::NowPlaying, _) => s.playing_context_name(),
            (ListKind::Search, _) => Some("Search results".to_string()),
            (ListKind::Queue, _) => Some("Queue".to_string()),
            (ListKind::History, _) => Some("History".to_string()),
            (ListKind::Playlists, Open::Local(id)) => s.playlists().into_iter().find(|p| p.id == *id).map(|p| p.name),
            (ListKind::Playlists, Open::TopLevel) => None,
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
            let (local, name) = (self.open_local(), self.context_name(s));
            return WindowOutcome::Run(Command::PlayContext { tracks, index, remote, local, name });
        }
        if !matches!(self.open, Open::TopLevel) || !matches!(self.kind, ListKind::Playlists | ListKind::Search) {
            return WindowOutcome::Ignored;
        }
        match self.top_row(s) {
            Some(row) => self.open_row_target(row),
            None => return WindowOutcome::Ignored,
        }
        WindowOutcome::Consumed
    }

    fn unit(&self, count: usize) -> &'static str {
        let playlists = self.kind == ListKind::Playlists && matches!(self.open, Open::TopLevel);
        let results = self.is_results();
        match (playlists, results, count == 1) {
            (true, _, true) => "playlist",
            (true, _, false) => "playlists",
            (_, true, true) => "result",
            (_, true, false) => "results",
            (_, _, true) => "track",
            (_, _, false) => "tracks",
        }
    }

    /// The title row: `<name>`, optionally followed by `  (<hint>)`.
    fn title(&self, s: &Session, total: usize, searching: bool) -> String {
        if let Some(query) = self.query.as_deref().filter(|_| self.filterable()) {
            let plural = if total == 1 { "" } else { "es" };
            return format!("filter {query:?} ({total} match{plural})");
        }
        let (name, hint) = match (self.kind, &self.open) {
            (ListKind::NowPlaying, _) => (s.playing_context_name(), None),
            (_, Open::Remote(sid, name, _)) => (Some(format!("[{sid}] {name}")), Some("Esc to go back")),
            (ListKind::Search, _) => (s.results_query().map(str::to_string), searching.then_some("Esc to cancel")),
            (ListKind::Playlists, Open::Local(_)) => (self.context_name(s), Some("Esc to go back")),
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
        let pending = match &self.open {
            Open::Remote(sid, _, node) => s.remote_pending_rows(sid, node),
            _ => PendingRows::default(),
        };
        // A row position means nothing in a filtered view.
        let pending = if self.filtered_ids(s).is_some() { PendingRows { rows: HashSet::new(), ..pending } } else { pending };
        let playing = self.playing_index(s);
        let track_rows = |tracks| tracks_to_rows(s, tracks, &pending, offset, playing);
        if let Some(matched) = self.filtered_ids(s) {
            if matched.is_empty() {
                return vec![plain_row(format!("no matches for {:?}", self.query.as_deref().unwrap_or_default()))];
            }
            let window: Vec<TrackId> = matched.iter().skip(offset).take(limit).copied().collect();
            return track_rows(s.tracks_for(&window));
        }
        match (self.kind, &self.open) {
            (ListKind::NowPlaying, _) if s.playing_context_len() == 0 => {
                vec![plain_row("nothing played yet — press Enter on a track to start playing")]
            }
            (ListKind::NowPlaying, _) => track_rows(s.playing_context_window(offset, limit)),
            (_, Open::Remote(sid, _, node)) => track_rows(s.remote_playlist_window(sid, node, offset, limit)),
            (ListKind::Search, _) if s.results_len() == 0 && s.search_collections().is_empty() => match s.results_query() {
                // A search ran and came back empty — say so.
                Some(q) => vec![plain_row(format!(
                    "no results for {q:?} — check the Log pane (:log) for source errors"
                ))],
                None => vec![],
            },
            (ListKind::Search, _) if self.len(s) == 0 => vec![plain_row(format!("no {} in the results", self.kinds.label()))],
            (ListKind::Search, _) => {
                let tracks = self.search_tracks(s);
                let mut rows = if offset < tracks { track_rows(s.results_window(offset, limit)) } else { vec![] };
                let room = limit.saturating_sub(rows.len());
                let collections = self.collections(s);
                rows.extend(collections.iter().skip(offset.saturating_sub(tracks)).take(room).map(|row| {
                    plain_row(format!("[{}] {}", kind_bar::noun(row.kind()), top_row_name(row, &[])))
                }));
                rows
            }
            (ListKind::Queue, _) => track_rows(s.queue_window(offset, limit)),
            (ListKind::History, _) => track_rows(s.history_window(offset, limit)),
            (ListKind::Playlists, Open::Local(id)) => track_rows(s.playlist_window(*id, offset, limit)),
            (ListKind::Playlists, Open::TopLevel) if self.kinds != KindFilter::All && self.top(s).is_empty() => {
                vec![plain_row(format!("no {} yet", self.kinds.label()))]
            }
            (ListKind::Playlists, Open::TopLevel) => {
                let playlists = s.playlists();
                self.top(s)
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|row| {
                        let mut r = match row {
                            TopRow::Local(id) => {
                                let p = playlists.iter().find(|p| p.id == *id);
                                let name = p.map(|p| p.name.clone()).unwrap_or_default();
                                let count = p.map(|p| p.items.len()).unwrap_or(0);
                                plain_row(format!("{name}  ({count} tracks)"))
                            }
                            TopRow::Remote(sid, name, _, ItemKind::Album) => plain_row(format!("[album] [{sid}] {name}")),
                            TopRow::Remote(sid, name, ..) => plain_row(format!("[{sid}] {name}")),
                        };
                        let key = s.playlist_hotkey(&row.target());
                        r.hotkeys = Cell::plain(key.map(String::from).unwrap_or_default());
                        r
                    })
                    .collect()
            }
        }
    }

    pub(super) fn loading(&self, s: &Session) -> bool {
        match (self.kind, &self.open) {
            (_, Open::Remote(sid, _, node)) => s.remote_playlist_loading(sid, node),
            (ListKind::Playlists, Open::TopLevel) => s.source_ids().iter().any(|sid| s.remote_playlists_loading(sid)),
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
        let key = (s.revision(), self.view_gen, self.state.offset, self.state.cursor, view_h, ctx.searching);
        self.frame.get_or_build(key, || {
            let total = self.len(s);
            let assignable = self.kind == ListKind::Playlists && matches!(self.open, Open::TopLevel) && total > 0;
            Arc::new(ListFrame {
                title: self.title(s, total, ctx.searching),
                rows: self.rows(s, self.state.offset, view_h),
                total,
                loading: self.loading(s),
                unit: self.unit(total),
                assignable,
                kind_counts: self.kind_counts(s),
                keyed: assignable && self.top_row(s).is_some_and(|row| s.playlist_hotkey(&row.target()).is_some()),
            })
        })
    }

    /// The status row's text when nothing was reported: what a key does here, in a row `fit` cells wide.
    pub(super) fn idle(&self, frame: &ListFrame, placement: Placement, status: &StatusCtx, fit: usize) -> String {
        let c = status.chrome;
        let tab = placement == Placement::Tabbed;
        let mut tail = status.tail(placement);
        tail.retain(|_| !(tab && self.kind == ListKind::Playlists));
        let hints: Vec<String> = match (self.kind, &self.open) {
            (ListKind::Playlists, Open::TopLevel) => {
                let assign = if frame.keyed { "[Bksp] clear" } else { "[any key] assign" };
                let mut hints: Vec<String> = frame.assignable.then(|| assign.to_string()).into_iter().collect();
                hints.extend(hint(&[c.keys_key], "playlist keys").filter(|_| tab));
                hints
            }
            (ListKind::Playlists, _) if tab => {
                let mut hints: Vec<String> = hint(&[c.like_key], "like").into_iter().chain(hint(&[c.enqueue_key, c.wedge_key], "queue")).collect();
                hints.extend(status.keys_run("send to playlist", &hints, fit));
                hints
            }
            (ListKind::Playlists, _) => status.keys_run("playlist", &tail, fit).into_iter().collect(),
            (ListKind::NowPlaying, _) => {
                let transport = match (c.prev_key, c.next_key) {
                    (Some(_), Some(_)) => hint(&[c.prev_key, c.next_key], "prev/next").into_iter().collect(),
                    _ => hint(&[c.prev_key], "prev").into_iter().chain(hint(&[c.next_key], "next")).collect::<Vec<_>>(),
                };
                let layout = hint(&[c.layout_key], "cycle layout").filter(|_| status.docked);
                let rest = [hint(&[c.help_key], "help")].into_iter().flatten().chain(transport);
                rest.chain(hint(&[c.enqueue_key, c.wedge_key], "queue")).chain(hint(&[c.reveal_key], "show playing")).chain(layout).collect()
            }
            (ListKind::Queue, _) => [hint(&[c.help_key], "help"), Some("[/] filter".into()), hint(&[c.clear_queue_key], "clear queue")].into_iter().flatten().collect(),
            (ListKind::History, _) => [hint(&[c.help_key], "help"), Some("[/] filter".into()), Some("[Enter] play".into())].into_iter().flatten().collect(),
            (ListKind::Search, _) => [hint(&[c.help_key], "help"), Some("[/] search".into()), Some("[Enter] play".into()), hint(&[c.kind_key], &format!("show: {}", self.kinds.label()))]
                .into_iter()
                .flatten()
                .collect(),
        };
        hints.into_iter().chain(tail).collect::<Vec<_>>().join("   ")
    }

    /// The cursor's place in the list, `cursor/total unit`; `None` when the list is empty.
    pub(super) fn count(&self, frame: &ListFrame) -> Option<String> {
        let more = if frame.loading { "+" } else { "" };
        (frame.total > 0).then(|| format!("{}/{}{more} {}", self.state.cursor.min(frame.total - 1) + 1, frame.total, frame.unit))
    }

    /// `marked` brackets the title, the focus marker docked windows use.
    pub(super) fn draw(&self, printer: &Printer, marked: bool, frame: &ListFrame) {
        let mut title = if marked { format!("[{}]", frame.title) } else { frame.title.clone() };
        let content_w = printer.size.x.saturating_sub(1);
        let bar = self.bar(frame.kind_counts.as_deref(), content_w);
        if let Some(start) = kind_bar::start(&bar) {
            title = scroll_title(&title, start.saturating_sub(main_col_start(content_w) + 1), 0);
        }
        draw_row_list(printer, &title, !matches!(self.open, Open::TopLevel), &frame.rows, self.state.offset, self.state.cursor, frame.total);
        kind_bar::draw(printer, &bar, self.kinds);
    }

    /// Re-follows the cursor when `resized`, else keeps cursor and scroll window inside the list.
    pub(super) fn relayout(&mut self, resized: bool, s: &Session, rect: Rect) {
        let len = self.len(s);
        // Its first key moves a row up into the keyed group; the cursor goes to the playlist that followed it.
        if let Some((_, next)) = self.assigned.take_if(|(bound, _)| s.playlist_hotkey(bound).is_some()) {
            self.select = Some(next);
        }
        let anchored = self.anchor.take().filter(|(generation, at, _)| *generation == self.view_gen && *at == self.state.cursor);
        let explicit = self.select.take();
        let follow = explicit.is_some();
        let target = explicit.or(anchored.map(|(.., target)| target));
        let selected = target.and_then(|target| match self.kind {
            ListKind::Search => self.collections(s).iter().position(|row| row.target() == target).map(|at| at + self.search_tracks(s)),
            _ => self.top(s).iter().position(|row| row.target() == target),
        });
        self.state.cursor = selected.unwrap_or(self.state.cursor).min(len.saturating_sub(1));
        self.state.relayout(resized || (selected.is_some() && follow), len, Self::body(rect).height());
        self.anchor = self.top_row(s)
            .filter(|_| self.is_results())
            .map(|row| (self.view_gen, self.state.cursor, row.target()));
    }

    /// Brings the scroll window back around the cursor.
    pub(super) fn follow(&mut self, rect: Rect) {
        self.state.follow(Self::body(rect).height());
    }

    /// Nav keys, Enter, Esc out of a filter then an open playlist, wheel and clicks inside `rect`; anything else is `Ignored`.
    pub(super) fn on_event(&mut self, event: &Event, ctx: &Ctx, rect: Rect) -> WindowOutcome {
        let (s, body) = (ctx.s, Self::body(rect));
        self.assigned = None;
        if let Event::Mouse { offset, position, event: mouse } = event {
            if !position.checked_sub(*offset).is_some_and(|pos| rect.contains(pos)) {
                return WindowOutcome::Ignored;
            }
            if matches!(mouse, MouseEvent::Hold(_) | MouseEvent::Release(_)) {
                return WindowOutcome::Ignored;
            }
            let local = *position - *offset - rect.top_left();
            if matches!(mouse, MouseEvent::Press(MouseButton::Left)) && local.y == 0 {
                let content_w = rect.width().saturating_sub(1);
                let frame = self.frame(ctx, rect);
                let bar = self.bar(frame.kind_counts.as_deref(), content_w);
                if let Some(kind) = kind_bar::hit(&bar, local.x) {
                    self.set_kinds(kind);
                    return WindowOutcome::Consumed;
                }
            }
            let back = !matches!(self.open, Open::TopLevel) && back_button_fits(rect.width().saturating_sub(1));
            if back && matches!(mouse, MouseEvent::Press(MouseButton::Left)) && local.y == 0 && local.x < BACK_LABEL.len() {
                self.show_top(None);
                return WindowOutcome::Consumed;
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
            ListEvent::Close if self.query.is_some() => {
                self.set_query(None);
                WindowOutcome::Consumed
            }
            ListEvent::Close if !matches!(self.open, Open::TopLevel) => {
                self.show_top(None);
                WindowOutcome::Consumed
            }
            ListEvent::Close => WindowOutcome::Ignored,
            ListEvent::Unhandled if matches!(event, Event::Mouse { .. }) => WindowOutcome::Consumed,
            ListEvent::Unhandled => self.assign(event, s),
        }
    }

    /// On a top-level playlist row a free key binds that playlist and Bksp clears its key.
    fn assign(&mut self, event: &Event, s: &Session) -> WindowOutcome {
        if self.kind != ListKind::Playlists || !matches!(self.open, Open::TopLevel) {
            return WindowOutcome::Ignored;
        }
        let Some(target) = self.top_row(s).as_ref().map(TopRow::target) else { return WindowOutcome::Ignored };
        let keyed = s.playlist_hotkey(&target).is_some();
        match event {
            Event::Key(Key::Backspace) if keyed => WindowOutcome::Unbind(target),
            Event::Char(key) if keybindings::taken(*key, &s.hotkeys().into_iter().collect()).is_none() => {
                let next = self.top(s).get(self.state.cursor + 1).map(TopRow::target);
                self.assigned = next.filter(|_| self.keyed_first && !keyed).map(|next| (target.clone(), next));
                WindowOutcome::Bind(target, *key)
            }
            _ => WindowOutcome::Ignored,
        }
    }
}
