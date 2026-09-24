//! `CachedSource` — a browsable, read-only view over every track already sitting in
//! `core::MediaCache`, regardless of which real source it came from. Never fetches
//! anything itself: it only enumerates `MediaCache`'s index and resolves each entry
//! back to its real `Track` (kept with its own, unmodified renditions) via `Store`.

use std::collections::HashSet;
use std::sync::Arc;

use core::{
    BrowseNode, BrowsePage, Error, MediaCache, Result, SearchQuery, Source, SourceId, Store, Track,
    matches_all_tokens,
};

/// The single synthetic folder this source offers at its root.
const ALL_PATH: &str = "all";

fn source_id() -> SourceId {
    SourceId::from("cached")
}

pub struct CachedSource {
    media_cache: Arc<MediaCache>,
    store: Arc<dyn Store>,
}

impl CachedSource {
    pub fn new(media_cache: Arc<MediaCache>, store: Arc<dyn Store>) -> Self {
        Self { media_cache, store }
    }

    /// Every locally cached track, alphabetical by display name. One bulk `Store::all_tracks()`
    /// call plus an in-memory filter against `MediaCache`'s index, rather than a store lookup per
    /// cache entry (see `core::MediaCache`'s module doc for why that scales badly).
    fn cached_tracks(&self) -> Result<Vec<Track>> {
        let cached: HashSet<(SourceId, String)> = self.media_cache.cached_entries().into_iter().collect();
        let mut out: Vec<Track> = self
            .store
            .all_tracks()?
            .into_iter()
            .filter(|track| track.renditions.iter().any(|r| cached.contains(&(r.source.clone(), r.uri.clone()))))
            .collect();
        out.sort_by_key(|t| t.display_name());
        Ok(out)
    }
}

impl Source for CachedSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn recognizes(&self, _uri: &str) -> bool {
        false
    }

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(Track)) -> Result<()> {
        let limit = if q.limit == 0 { usize::MAX } else { q.limit };
        let mut emitted = 0usize;
        for track in self.cached_tracks()? {
            if emitted >= limit {
                break;
            }
            if !matches_all_tokens(&q.text, &track.display_name()) {
                continue;
            }
            sink(track);
            emitted += 1;
        }
        Ok(())
    }

    fn resolve(&self, _uri: &str) -> Result<Track> {
        Err(Error::Unsupported("resolve"))
    }

    fn browse(&self, node: &BrowseNode, _want: usize) -> Result<BrowsePage> {
        match node {
            BrowseNode::Root => Ok(BrowsePage {
                title: "cached".to_string(),
                tracks: vec![],
                folders: vec![("All cached tracks".to_string(), BrowseNode::Path(ALL_PATH.to_string()))],
                node_meta: vec![],
                partial: false,
                errored: false,
            }),
            BrowseNode::Path(p) if p == ALL_PATH => Ok(BrowsePage {
                title: "All cached tracks".to_string(),
                tracks: self.cached_tracks()?,
                folders: vec![],
                node_meta: vec![],
                partial: false,
                errored: false,
            }),
            BrowseNode::Path(_) => Err(Error::NotFound),
        }
    }
}
