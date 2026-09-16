//! Search fan-out.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::Catalog;
use crate::event::{Bus, CoreEvent};
use crate::traits::Source;
use crate::types::SearchQuery;

pub struct Search {
    sources: Vec<Arc<dyn Source>>,
    catalog: Arc<Catalog>,
    bus: Bus,
    generation: Arc<AtomicU64>,
}

impl Search {
    pub fn new(sources: Vec<Arc<dyn Source>>, catalog: Arc<Catalog>, bus: Bus) -> Self {
        Self {
            sources,
            catalog,
            bus,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Spawn one OS thread per source. Returns immediately. A generation counter
    /// cancels stale searches: results from a superseded generation are dropped.
    pub fn run(&self, q: SearchQuery) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        log::info!(
            "search gen {generation}: {:?} across {} source(s)",
            q.text,
            self.sources.len()
        );
        for source in &self.sources {
            let source = source.clone();
            let catalog = self.catalog.clone();
            let bus = self.bus.clone();
            let gen_counter = self.generation.clone();
            let q = q.clone();
            std::thread::spawn(move || {
                let sid = source.id();
                let mut hits = 0usize;
                let mut sink = |hit: crate::types::SearchHit| {
                    if gen_counter.load(Ordering::SeqCst) != generation {
                        return; // superseded
                    }
                    match catalog.ingest(hit) {
                        Ok(tid) => {
                            hits += 1;
                            bus.send(CoreEvent::SearchHit(tid))
                        }
                        Err(e) => bus.send(CoreEvent::SourceError {
                            source: sid.clone(),
                            message: e.to_string(),
                        }),
                    }
                };
                match source.search(&q, &mut sink) {
                    Ok(()) => {
                        let stale = gen_counter.load(Ordering::SeqCst) != generation;
                        log::info!(
                            "search gen {generation} [{}]: done, {hits} hit(s){}",
                            sid,
                            if stale { " (superseded)" } else { "" }
                        );
                        if !stale {
                            bus.send(CoreEvent::SearchDone { source: sid.clone() });
                        }
                    }
                    Err(e) => {
                        log::warn!("search gen {generation} [{}]: {e}", sid);
                        bus.send(CoreEvent::SourceError {
                            source: sid.clone(),
                            message: e.to_string(),
                        })
                    }
                }
            });
        }
    }
}
