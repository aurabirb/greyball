use std::sync::{Arc, Mutex};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use core::{BrowseNode, PlaylistId, Session, SourceId};

use crate::row::RowItem;

use super::{HIST, MedleyView, NOW_PLAYING, PLAYLISTS, QUEUE};
use super::input::Editing;

/// The screen-local `/`-filter: fuzzy-narrows the viewed list without touching `Session`.
#[derive(Default)]
pub(super) struct LocalFilter {
    /// Committed query (Enter); while still typing, `active_filter` reads the edit buffer instead.
    pub(super) query: Option<String>,
    matcher: SkimMatcherV2,
    /// Filtering and ranking a whole list is too slow to redo per redraw; a `Mutex` only because `draw` takes `&self`.
    cache: Mutex<Option<FilterCache>>,
}

/// The last `filtered_tracks` result, plus the key it was computed under.
/// Keyed on `revision` rather than a source length, so a same-length content
/// swap (e.g. a second search returning as many results as the first) still
/// recomputes instead of serving stale matches.
struct FilterCache {
    revision: u64,
    screen: usize,
    /// `PlaylistNav::list_id`, so two same-length playlists never share a cache entry.
    list_id: (Option<PlaylistId>, Option<(SourceId, BrowseNode)>),
    query: String,
    /// `Arc` so callers can hand out the whole matched list without cloning every `Track` in it.
    result: Arc<[core::Track]>,
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
                if let Some(id) = self.playlists.open {
                    s.playlist_window(id, 0, s.playlist_len(id))
                } else if let Some((sid, _, node)) = &self.playlists.remote {
                    s.remote_playlist_window(sid, node, 0, s.remote_playlist_len(sid, node))
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// Whether `/` on `screen` should filter it locally rather than jump to Search.
    pub(super) fn filterable_screen(&self, screen: usize) -> bool {
        match screen {
            NOW_PLAYING | QUEUE | HIST => true,
            PLAYLISTS => !self.playlists.at_top_level(),
            _ => false,
        }
    }

    /// The active local filter query.
    pub(super) fn active_filter(&self) -> Option<&str> {
        match &self.editing {
            Editing::Filter => Some(self.buffer.as_str()),
            _ => self.filter.query.as_deref(),
        }
    }

    /// `screen`'s tracks narrowed and ranked by the active local filter; `Arc` clone only.
    pub(super) fn filtered_tracks(&self, s: &Session, screen: usize) -> Option<Arc<[core::Track]>> {
        let query = self.active_filter()?;
        if query.is_empty() || !self.filterable_screen(screen) {
            return None;
        }
        let revision = s.revision();
        let list_id = self.playlists.list_id();

        if let Some(cache) = self.filter.cache.lock().unwrap().as_ref()
            && cache.revision == revision
            && cache.screen == screen
            && cache.list_id == list_id
            && cache.query == query
        {
            return Some(cache.result.clone());
        }

        let tracks = self.all_tracks_for_screen(s, screen);
        let mut ranked: Vec<(core::Track, FilterRank)> = tracks
            .into_iter()
            .filter_map(|t| rank_filter(&self.filter.matcher, &t.main(), query).map(|r| (t, r)))
            .collect();
        ranked.sort_by(|a, b| a.1.cmp(&b.1));
        let result: Arc<[core::Track]> = ranked.into_iter().map(|(t, _)| t).collect();

        *self.filter.cache.lock().unwrap() = Some(FilterCache {
            revision,
            screen,
            list_id,
            query: query.to_string(),
            result: result.clone(),
        });
        Some(result)
    }

    /// Reset the current screen's cursor/scroll to the top.
    pub(super) fn reset_filter_selection(&mut self) {
        let screen = self.screen;
        self.lists[screen].cursor = 0;
        self.lists[screen].offset = 0;
    }
}
