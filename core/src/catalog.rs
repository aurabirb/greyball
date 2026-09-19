//! Ingest, link/unlink.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::{Bus, CoreEvent};
use crate::matcher::Matcher;
use crate::traits::{Result, Store};
use crate::types::{LinkReason, Playlist, Rendition, SearchHit, SourceId, Track, TrackId};

/// Push `r` unless a rendition with the same `(source, uri)` already exists.
fn merge_rendition(renditions: &mut Vec<Rendition>, r: Rendition) {
    if !renditions.iter().any(|x| x.source == r.source && x.uri == r.uri) {
        renditions.push(r);
    }
}

/// Fill in `t`'s isrc/duration/album from another track's, but only where
/// `t` doesn't already have one — never overwrite existing metadata.
fn merge_metadata(t: &mut Track, isrc: Option<String>, duration_ms: u32, album: Option<String>) {
    if t.isrc.is_none() {
        t.isrc = isrc;
    }
    if t.duration_ms == 0 {
        t.duration_ms = duration_ms;
    }
    if t.album.is_none() {
        t.album = album;
    }
}

pub struct Catalog {
    store: Arc<dyn Store>,
    bus: Bus,
    /// Every method here is a read-then-write across two separate `Store`
    /// calls (`get_track`/`track_by_*` followed by `upsert_track`) — with
    /// `ScanDriver` now writing `attrs` from a background thread
    /// continuously, and the UI/session thread able to `ingest`/`patch` the
    /// same track at any time (M3U import, `link`/`unlink`, ...), two
    /// interleaved read-modify-writes would otherwise silently lose
    /// whichever one committed first. One coarse lock serializes all of
    /// them; contention is negligible since these are occasional writes,
    /// not a hot read path (reads bypass `Catalog` entirely and go straight
    /// through `Store`, unaffected by this lock).
    lock: Mutex<()>,
    /// Bumped by `save_playlist`, the one write path for a user playlist.
    playlists_gen: AtomicU64,
    /// Bumped whenever a track id stops existing, since any list may still hold it.
    removed_gen: AtomicU64,
}

impl Catalog {
    pub fn new(store: Arc<dyn Store>, bus: Bus) -> Self {
        Self { store, bus, lock: Mutex::new(()), playlists_gen: AtomicU64::new(0), removed_gen: AtomicU64::new(0) }
    }

    pub fn save_playlist(&self, playlist: &Playlist) -> Result<()> {
        self.store.upsert_playlist(playlist)?;
        self.playlists_gen.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn delete_track(&self, id: TrackId) -> Result<()> {
        self.store.delete_track(id)?;
        self.removed_gen.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn removed_gen(&self) -> u64 {
        self.removed_gen.load(Ordering::Relaxed)
    }

    pub fn playlists_gen(&self) -> u64 {
        self.playlists_gen.load(Ordering::Relaxed)
    }

    /// Fold a hit into the library. Returns the logical track it belongs to.
    pub fn ingest(&self, hit: SearchHit) -> Result<TrackId> {
        let _guard = self.lock.lock().unwrap();
        // 1. exact rendition dedupe
        if let Some(t) = self.store.track_by_rendition(&hit.source, &hit.uri)? {
            return Ok(t.id);
        }

        // 2. ISRC match
        if let Some(isrc) = &hit.isrc
            && let Some(mut t) = self.store.track_by_isrc(isrc)?
        {
            self.append_rendition(&mut t, &hit, LinkReason::Isrc);
            self.store.upsert_track(&t)?;
            self.bus.send(CoreEvent::TrackUpdated(t.id));
            return Ok(t.id);
        }

        // 3. fuzzy match — candidates are indexed by normalized title
        // (`Store::tracks_by_title_norm`), not a scan over the whole
        // library: `Matcher::matches`'s fuzzy branch requires
        // `Matcher::norm(title)` equality anyway, so nothing outside that
        // bucket could ever match.
        let candidates = self.store.tracks_by_title_norm(&Matcher::norm(&hit.title))?;
        log::trace!("ingest: {} title-matched candidate(s)", candidates.len());
        for mut t in candidates {
            if let Some(reason) = Matcher::matches(&t, &hit) {
                self.append_rendition(&mut t, &hit, reason);
                self.store.upsert_track(&t)?;
                self.bus.send(CoreEvent::TrackUpdated(t.id));
                return Ok(t.id);
            }
        }

        // 4. new track
        let id = TrackId::new();
        let track = Track {
            id,
            title: hit.title.clone(),
            artists: hit.artists.clone(),
            duration_ms: hit.duration_ms,
            isrc: hit.isrc.clone(),
            album: hit.album.clone(),
            year: None,
            attrs: std::collections::BTreeMap::new(),
            tags: vec![],
            renditions: vec![hit.to_rendition(LinkReason::Manual)],
        };
        self.store.upsert_track(&track)?;
        self.bus.send(CoreEvent::TrackUpdated(id));
        Ok(id)
    }

    fn append_rendition(&self, t: &mut Track, hit: &SearchHit, reason: LinkReason) {
        merge_rendition(&mut t.renditions, hit.to_rendition(reason));
        merge_metadata(t, hit.isrc.clone(), hit.duration_ms, hit.album.clone());
    }

    /// Merge b into a: move all b.renditions into a (skip dup uris), delete b,
    /// fix up any playlist references b->a. Emits TrackUpdated(a).
    pub fn link(&self, a: TrackId, b: TrackId) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let _guard = self.lock.lock().unwrap();
        let mut ta = self
            .store
            .get_track(a)?
            .ok_or(crate::traits::Error::NotFound)?;
        let tb = self
            .store
            .get_track(b)?
            .ok_or(crate::traits::Error::NotFound)?;

        for r in tb.renditions {
            merge_rendition(&mut ta.renditions, r);
        }
        merge_metadata(&mut ta, tb.isrc, tb.duration_ms, tb.album);

        self.delete_track(b)?;
        self.store.upsert_track(&ta)?;

        // fix up playlist references
        for mut p in self.store.all_playlists()? {
            if p.items.contains(&b) {
                for item in p.items.iter_mut() {
                    if *item == b {
                        *item = a;
                    }
                }
                self.save_playlist(&p)?;
            }
        }

        self.bus.send(CoreEvent::TrackUpdated(a));
        Ok(())
    }

    /// Remove the rendition(s) from this source. If it was the last rendition,
    /// delete the track.
    pub fn unlink(&self, t: TrackId, source: SourceId) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut track = self
            .store
            .get_track(t)?
            .ok_or(crate::traits::Error::NotFound)?;
        track.renditions.retain(|r| r.source != source);
        if track.renditions.is_empty() {
            self.delete_track(t)?;
            self.bus.send(CoreEvent::TrackUpdated(t));
        } else {
            self.store.upsert_track(&track)?;
            self.bus.send(CoreEvent::TrackUpdated(t));
        }
        Ok(())
    }

    /// `TrackUpdated` with no attrs write — for a derived-only change (e.g. a cache fill with no result).
    pub fn announce(&self, id: TrackId) {
        self.bus.send(CoreEvent::TrackUpdated(id));
    }

    /// Lists a background failure of work that holds only the catalog under the warnings.
    pub fn warn(&self, context: &str, message: String) {
        self.bus.send(CoreEvent::BackgroundFailure { context: context.to_string(), message });
    }

    /// Set attrs/tags/etc. from analysis or user edits.
    pub fn patch(&self, t: TrackId, f: impl FnOnce(&mut Track)) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut track = self
            .store
            .get_track(t)?
            .ok_or(crate::traits::Error::NotFound)?;
        f(&mut track);
        self.store.upsert_track(&track)?;
        self.bus.send(CoreEvent::TrackUpdated(t));
        Ok(())
    }
}

