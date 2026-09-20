//! Search fan-out.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::Catalog;
use crate::event::{Bus, CoreEvent};
use crate::traits::{BrowseNode, Source};
use crate::types::{ItemKind, SearchQuery};

pub struct Search {
    sources: Vec<Arc<dyn Source>>,
    catalog: Arc<Catalog>,
    bus: Bus,
    next: AtomicU64,
    /// The searches still wanted; a source thread drops what it finds for any other.
    live: Arc<Mutex<HashSet<u64>>>,
}

impl Search {
    pub fn new(sources: Vec<Arc<dyn Source>>, catalog: Arc<Catalog>, bus: Bus) -> Self {
        Self { sources, catalog, bus, next: AtomicU64::new(0), live: Arc::new(Mutex::new(HashSet::new())) }
    }

    /// Cancels `id`: nothing more is reported for it.
    pub fn forget(&self, id: u64) {
        self.live.lock().unwrap().remove(&id);
    }

    /// Spawns one OS thread per source and returns the search's id at once; every event it reports carries that id.
    pub fn run(&self, q: SearchQuery) -> u64 {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.live.lock().unwrap().insert(id);
        log::info!("search {id}: {:?} across {} source(s)", q.text, self.sources.len());
        for source in &self.sources {
            let source = source.clone();
            let catalog = self.catalog.clone();
            let bus = self.bus.clone();
            let live = self.live.clone();
            let q = q.clone();
            std::thread::spawn(move || {
                let sid = source.id();
                let wanted = || live.lock().unwrap().contains(&id);
                let mut hits = 0usize;
                let mut sink = |hit: crate::types::Track| {
                    if !wanted() {
                        return;
                    }
                    match catalog.ingest(hit) {
                        Ok(tid) => {
                            hits += 1;
                            bus.send(CoreEvent::SearchHit { search: id, track: tid })
                        }
                        Err(e) => bus.send(CoreEvent::BackgroundFailure {
                            context: sid.to_string(),
                            message: e.to_string(),
                        }),
                    }
                };
                let mut result = source.search(&q, &mut sink);
                for kind in [ItemKind::Album, ItemKind::Playlist] {
                    if result.is_err() || !q.kinds.contains(&kind) {
                        continue;
                    }
                    let mut collection_sink = |name: String, node: BrowseNode| {
                        if !wanted() {
                            return;
                        }
                        bus.send(CoreEvent::SearchCollection { search: id, source: sid.clone(), kind, name, node })
                    };
                    result = source.search_collections(&q, kind, &mut collection_sink);
                }
                match result {
                    Ok(()) => {
                        let stale = !wanted();
                        log::info!("search {id} [{sid}]: done, {hits} hit(s){}", if stale { " (cancelled)" } else { "" });
                        if !stale {
                            bus.send(CoreEvent::SearchDone { search: id, source: sid.clone() });
                        }
                    }
                    Err(e) => {
                        log::warn!("search {id} [{sid}]: {e}");
                        bus.send(CoreEvent::BackgroundFailure { context: sid.to_string(), message: format!("search: {e}") })
                    }
                }
            });
        }
        id
    }
}
