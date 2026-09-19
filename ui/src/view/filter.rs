use std::sync::Arc;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use core::{BrowseNode, PlaylistId, Session, SourceId, TrackId};

use crate::row::RowItem;
use crate::screen::Screen;

use super::MedleyView;
use super::input::Editing;
use super::memo::Memo;

/// The screen-local `/`-filter: fuzzy-narrows the viewed list without touching `Session`.
#[derive(Default)]
pub(super) struct LocalFilter {
    /// Committed query (Enter); while still typing, `active_filter` reads the edit buffer instead.
    pub(super) query: Option<String>,
    matcher: SkimMatcherV2,
    /// Filtering and ranking a whole list is too slow to redo per redraw; ids (not `Track`s) so an attrs patch can't go stale in it.
    cache: Memo<FilterKey, Arc<[TrackId]>>,
}

#[derive(PartialEq)]
struct FilterKey {
    list_revision: u64,
    screen: Screen,
    /// `PlaylistNav::list_id`, so two same-length playlists never share a cache entry.
    list_id: (Option<PlaylistId>, Option<(SourceId, BrowseNode)>),
    query: String,
}

/// The `/`-filter's rank for one row against `query`, low-to-high, `None` if it doesn't match at all.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct FilterRank(u8, std::cmp::Reverse<i64>);

fn rank_filter(matcher: &SkimMatcherV2, text: &str, query: &str) -> Option<FilterRank> {
    let text = text.to_lowercase();
    let query = query.to_lowercase();
    let score = matcher.fuzzy_match(&text, &query)?;
    let tier = if text.contains(&query) { 0 } else { 1 };
    Some(FilterRank(tier, std::cmp::Reverse(score)))
}

impl MedleyView {
    /// Every track already loaded for `screen`'s list, unwindowed.
    fn all_tracks_for_screen(&self, s: &Session, screen: Screen) -> Vec<core::Track> {
        match screen {
            Screen::NowPlaying => s.playing_context_window(0, s.playing_context_len()),
            Screen::Queue => s.queue_window(0, s.queue_len()),
            Screen::History => s.history_window(0, s.queue.history_len()),
            Screen::Playlists => {
                if let Some(id) = self.playlists.open {
                    s.playlist_window(id, 0, s.playlist_len(id))
                } else if let Some((sid, _, node)) = &self.playlists.remote {
                    s.remote_playlist_window(sid, node, 0, s.remote_playlist_len(sid, node))
                } else {
                    vec![]
                }
            }
            Screen::Search => vec![],
        }
    }

    /// Whether `/` on `screen` should filter it locally rather than jump to Search.
    pub(super) fn filterable_screen(&self, screen: Screen) -> bool {
        match screen {
            Screen::NowPlaying | Screen::Queue | Screen::History => true,
            Screen::Playlists => !self.playlists.at_top_level(),
            Screen::Search => false,
        }
    }

    /// The active local filter query.
    pub(super) fn active_filter(&self) -> Option<&str> {
        match &self.editing {
            Editing::Filter => Some(self.buffer.as_str()),
            _ => self.filter.query.as_deref(),
        }
    }

    /// `screen`'s track ids narrowed/ranked by the local filter; callers resolve rows via `Session::tracks_for`.
    pub(super) fn filtered_ids(&self, s: &Session, screen: Screen) -> Option<Arc<[TrackId]>> {
        let query = self.active_filter()?;
        if query.is_empty() || !self.filterable_screen(screen) {
            return None;
        }
        let key = FilterKey {
            list_revision: s.list_revision(),
            screen,
            list_id: self.playlists.list_id(),
            query: query.to_string(),
        };
        Some(self.filter.cache.get_or_build(key, || {
            let mut ranked: Vec<(TrackId, FilterRank)> = self
                .all_tracks_for_screen(s, screen)
                .into_iter()
                .filter_map(|t| rank_filter(&self.filter.matcher, &t.main(), query).map(|r| (t.id, r)))
                .collect();
            ranked.sort_by(|a, b| a.1.cmp(&b.1));
            ranked.into_iter().map(|(id, _)| id).collect()
        }))
    }

    /// Reset the current screen's cursor/scroll to the top.
    pub(super) fn reset_filter_selection(&mut self) {
        let screen = self.screen;
        self.lists[screen].cursor = 0;
        self.lists[screen].offset = 0;
    }
}
