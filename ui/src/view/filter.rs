use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use core::{BrowseNode, PlaylistId, Session, SourceId};

use crate::row::RowItem;

use super::{Editing, HIST, MedleyView, NOW_PLAYING, PLAYLISTS, QUEUE};

/// `MedleyView::filter_cache`'s contents.
pub(super) struct FilterCache {
    screen: usize,
    /// `(open_playlist, open_remote)` identity, so two same-length playlists never share a cache entry.
    list_id: (Option<PlaylistId>, Option<(SourceId, BrowseNode)>),
    query: String,
    source_len: usize,
    result: Vec<core::Track>,
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
    fn all_tracks_for_screen(&self, s: &Session, screen: usize) -> Vec<core::Track> {
        match screen {
            NOW_PLAYING => s.playing_context_window(0, s.playing_context_len()),
            QUEUE => s.queue_window(0, s.queue_len()),
            HIST => s.history_window(0, s.queue.history_len()),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlist_window(id, 0, s.playlist_len(id))
                } else if let Some((sid, _, node)) = &self.open_remote {
                    s.remote_playlist_window(sid, node, 0, s.remote_playlist_len(sid, node))
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// Cheap count of `all_tracks_for_screen`'s source list.
    fn filterable_source_len(&self, s: &Session, screen: usize) -> usize {
        match screen {
            NOW_PLAYING => s.playing_context_len(),
            QUEUE => s.queue_len(),
            HIST => s.queue.history_len(),
            PLAYLISTS => {
                if let Some(id) = self.open_playlist {
                    s.playlist_len(id)
                } else if let Some((sid, _, node)) = &self.open_remote {
                    s.remote_playlist_len(sid, node)
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Whether `/` on `screen` should filter it locally rather than jump to Search.
    pub(super) fn filterable_screen(&self, screen: usize) -> bool {
        match screen {
            NOW_PLAYING | QUEUE | HIST => true,
            PLAYLISTS => self.open_playlist.is_some() || self.open_remote.is_some(),
            _ => false,
        }
    }

    /// The active local filter query.
    pub(super) fn active_filter(&self) -> Option<&str> {
        match &self.editing {
            Editing::Filter => Some(self.buffer.as_str()),
            _ => self.filter_query.as_deref(),
        }
    }

    /// `screen`'s tracks narrowed and ranked by the active local filter.
    pub(super) fn filtered_tracks(&self, s: &Session, screen: usize) -> Option<Vec<core::Track>> {
        let query = self.active_filter()?;
        if query.is_empty() || !self.filterable_screen(screen) {
            return None;
        }
        let source_len = self.filterable_source_len(s, screen);
        let list_id = (self.open_playlist, self.open_remote.clone().map(|(sid, _, node)| (sid, node)));

        if let Some(cache) = self.filter_cache.lock().unwrap().as_ref()
            && cache.screen == screen
            && cache.list_id == list_id
            && cache.query == query
            && cache.source_len == source_len
        {
            return Some(cache.result.clone());
        }

        let tracks = self.all_tracks_for_screen(s, screen);
        let mut ranked: Vec<(core::Track, FilterRank)> = tracks
            .into_iter()
            .filter_map(|t| rank_filter(&self.filter_matcher, &t.main(), query).map(|r| (t, r)))
            .collect();
        ranked.sort_by(|a, b| a.1.cmp(&b.1));
        let result: Vec<core::Track> = ranked.into_iter().map(|(t, _)| t).collect();

        *self.filter_cache.lock().unwrap() = Some(FilterCache {
            screen,
            list_id,
            query: query.to_string(),
            source_len,
            result: result.clone(),
        });
        Some(result)
    }

    /// Reset the current screen's cursor/scroll to the top.
    pub(super) fn reset_filter_selection(&mut self) {
        let screen = self.screen;
        self.cursor[screen] = 0;
        self.list_offset[screen] = 0;
    }
}
