//! Store implementations: `MemStore` (in-memory) and `RedbStore` (redb).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use redb::{Database, MultimapTableDefinition, ReadableTable, TableDefinition};

use crate::matcher::Matcher;
use crate::traits::{Error, NodeMeta, Result, Store, StoredFolders};
use crate::types::{Playlist, PlaylistId, Track, TrackId};

fn rendition_key(source: &str, uri: &str) -> String {
    format!("{source}\0{uri}")
}

fn store_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Store(e.to_string())
}

// --------------------------------------------------------------------------
// MemStore
// --------------------------------------------------------------------------

#[derive(Default)]
struct MemInner {
    tracks: HashMap<TrackId, Track>,
    playlists: HashMap<PlaylistId, Playlist>,
    by_isrc: HashMap<String, TrackId>,
    by_rendition: HashMap<String, TrackId>,
    by_title_norm: HashMap<String, Vec<TrackId>>,
    remote_playlist_ids: HashMap<String, Vec<TrackId>>,
    remote_playlist_folders: HashMap<String, StoredFolders>,
}

#[derive(Default)]
pub struct MemStore {
    inner: Mutex<MemInner>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemStore {
    fn upsert_track(&self, t: &Track) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        // drop stale secondary index entries for this track
        inner.by_isrc.retain(|_, v| *v != t.id);
        inner.by_rendition.retain(|_, v| *v != t.id);
        for ids in inner.by_title_norm.values_mut() {
            ids.retain(|id| *id != t.id);
        }
        if let Some(isrc) = &t.isrc {
            inner.by_isrc.insert(isrc.clone(), t.id);
        }
        for r in &t.renditions {
            inner
                .by_rendition
                .insert(rendition_key(r.source.as_str(), &r.uri), t.id);
        }
        inner.by_title_norm.entry(Matcher::norm(&t.title)).or_default().push(t.id);
        inner.tracks.insert(t.id, t.clone());
        Ok(())
    }

    fn get_track(&self, id: TrackId) -> Result<Option<Track>> {
        Ok(self.inner.lock().unwrap().tracks.get(&id).cloned())
    }

    fn all_tracks(&self) -> Result<Vec<Track>> {
        Ok(self.inner.lock().unwrap().tracks.values().cloned().collect())
    }

    fn track_by_isrc(&self, isrc: &str) -> Result<Option<Track>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .by_isrc
            .get(isrc)
            .and_then(|id| inner.tracks.get(id).cloned()))
    }

    fn track_by_rendition(
        &self,
        source: &crate::types::SourceId,
        uri: &str,
    ) -> Result<Option<Track>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .by_rendition
            .get(&rendition_key(source.as_str(), uri))
            .and_then(|id| inner.tracks.get(id).cloned()))
    }

    fn tracks_by_title_norm(&self, norm_title: &str) -> Result<Vec<Track>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .by_title_norm
            .get(norm_title)
            .into_iter()
            .flatten()
            .filter_map(|id| inner.tracks.get(id).cloned())
            .collect())
    }

    fn delete_track(&self, id: TrackId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.tracks.remove(&id);
        inner.by_isrc.retain(|_, v| *v != id);
        inner.by_rendition.retain(|_, v| *v != id);
        for ids in inner.by_title_norm.values_mut() {
            ids.retain(|i| *i != id);
        }
        Ok(())
    }

    fn upsert_playlist(&self, p: &Playlist) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .playlists
            .insert(p.id, p.clone());
        Ok(())
    }

    fn get_playlist(&self, id: PlaylistId) -> Result<Option<Playlist>> {
        Ok(self.inner.lock().unwrap().playlists.get(&id).cloned())
    }

    fn all_playlists(&self) -> Result<Vec<Playlist>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .playlists
            .values()
            .cloned()
            .collect())
    }

    fn delete_playlist(&self, id: PlaylistId) -> Result<()> {
        self.inner.lock().unwrap().playlists.remove(&id);
        Ok(())
    }

    fn remote_playlist_ids(&self, key: &str) -> Result<Vec<TrackId>> {
        Ok(self.inner.lock().unwrap().remote_playlist_ids.get(key).cloned().unwrap_or_default())
    }

    fn set_remote_playlist_ids(&self, key: &str, ids: &[TrackId]) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .remote_playlist_ids
            .insert(key.to_string(), ids.to_vec());
        Ok(())
    }

    fn remote_playlist_folders(&self, source: &str) -> Result<StoredFolders> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .remote_playlist_folders
            .get(source)
            .cloned()
            .unwrap_or_default())
    }

    fn set_remote_playlist_folders(&self, source: &str, folders: &[(String, String)], meta: &[(String, NodeMeta)]) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .remote_playlist_folders
            .insert(source.to_string(), (folders.to_vec(), meta.to_vec()));
        Ok(())
    }
}

// --------------------------------------------------------------------------
// RedbStore
// --------------------------------------------------------------------------

const TRACKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tracks");
const PLAYLISTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("playlists");
const IDX_ISRC: TableDefinition<&str, &[u8]> = TableDefinition::new("idx_isrc");
const IDX_RENDITION: TableDefinition<&str, &[u8]> = TableDefinition::new("idx_rendition");
/// `Matcher::norm(title) -> track id`. A multimap: distinct tracks can share
/// a normalized title (same name, different artist/recording).
const IDX_TITLE: MultimapTableDefinition<&str, &[u8]> = MultimapTableDefinition::new("idx_title_norm");
/// Opaque key (see `Store::remote_playlist_ids`) -> JSON `Vec<Uuid>`, in
/// order. One row per remote browse node ever viewed (e.g. Spotify Liked
/// Songs), so a paginated walk can resume displaying instantly from what a
/// previous session already fetched instead of restarting from scratch.
const REMOTE_PLAYLIST_IDS: TableDefinition<&str, &[u8]> = TableDefinition::new("remote_playlist_ids");
/// `SourceId` string -> JSON `Vec<(name, path_id)>`, a source's top-level
/// playlist-folder list — see `Store::remote_playlist_folders`.
const REMOTE_PLAYLIST_FOLDERS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("remote_playlist_folders");
/// `SourceId` string -> JSON `Vec<(path_id, NodeMeta)>`, written with `REMOTE_PLAYLIST_FOLDERS`.
const NODE_META: TableDefinition<&str, &[u8]> = TableDefinition::new("node_meta");

pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = Database::create(path).map_err(store_err)?;
        // ensure every table exists so read txns never hit TableDoesNotExist
        let w = db.begin_write().map_err(store_err)?;
        {
            w.open_table(TRACKS).map_err(store_err)?;
            w.open_table(PLAYLISTS).map_err(store_err)?;
            w.open_table(IDX_ISRC).map_err(store_err)?;
            w.open_table(IDX_RENDITION).map_err(store_err)?;
            w.open_multimap_table(IDX_TITLE).map_err(store_err)?;
            w.open_table(REMOTE_PLAYLIST_IDS).map_err(store_err)?;
            w.open_table(REMOTE_PLAYLIST_FOLDERS).map_err(store_err)?;
            w.open_table(NODE_META).map_err(store_err)?;
        }
        w.commit().map_err(store_err)?;
        Ok(Self { db })
    }
}

impl Store for RedbStore {
    fn upsert_track(&self, t: &Track) -> Result<()> {
        let json = serde_json::to_vec(t).map_err(store_err)?;
        let key = t.id.0.as_bytes().to_vec();
        // Read before overwrite: only need the *old* title, to remove its
        // now-stale IDX_TITLE entry — cheap (one row), unlike scanning the
        // whole index the way IDX_ISRC/IDX_RENDITION do below.
        let old_title_norm = self.get_track(t.id)?.map(|old| Matcher::norm(&old.title));
        let new_title_norm = Matcher::norm(&t.title);
        let w = self.db.begin_write().map_err(store_err)?;
        {
            let mut tracks = w.open_table(TRACKS).map_err(store_err)?;
            tracks
                .insert(key.as_slice(), json.as_slice())
                .map_err(store_err)?;

            let mut idx_title = w.open_multimap_table(IDX_TITLE).map_err(store_err)?;
            if let Some(old) = &old_title_norm
                && old != &new_title_norm
            {
                idx_title.remove(old.as_str(), key.as_slice()).map_err(store_err)?;
            }
            idx_title
                .insert(new_title_norm.as_str(), key.as_slice())
                .map_err(store_err)?;

            let mut idx_isrc = w.open_table(IDX_ISRC).map_err(store_err)?;
            let stale: Vec<String> = idx_isrc
                .iter()
                .map_err(store_err)?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    (v.value() == key.as_slice()).then(|| k.value().to_string())
                })
                .collect();
            for k in stale {
                idx_isrc.remove(k.as_str()).map_err(store_err)?;
            }
            if let Some(isrc) = &t.isrc {
                idx_isrc
                    .insert(isrc.as_str(), key.as_slice())
                    .map_err(store_err)?;
            }

            let mut idx_rend = w.open_table(IDX_RENDITION).map_err(store_err)?;
            let stale: Vec<String> = idx_rend
                .iter()
                .map_err(store_err)?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    (v.value() == key.as_slice()).then(|| k.value().to_string())
                })
                .collect();
            for k in stale {
                idx_rend.remove(k.as_str()).map_err(store_err)?;
            }
            for r in &t.renditions {
                idx_rend
                    .insert(
                        rendition_key(r.source.as_str(), &r.uri).as_str(),
                        key.as_slice(),
                    )
                    .map_err(store_err)?;
            }
        }
        w.commit().map_err(store_err)?;
        Ok(())
    }

    fn get_track(&self, id: TrackId) -> Result<Option<Track>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let tracks = r.open_table(TRACKS).map_err(store_err)?;
        let key = id.0.as_bytes().to_vec();
        match tracks.get(key.as_slice()).map_err(store_err)? {
            Some(g) => Ok(Some(serde_json::from_slice(g.value()).map_err(store_err)?)),
            None => Ok(None),
        }
    }

    /// A record that fails to deserialize (a stale shape from before a
    /// schema change, corruption, ...) is skipped rather than failing the
    /// whole read — one bad track shouldn't make the entire catalog
    /// unreadable.
    fn all_tracks(&self) -> Result<Vec<Track>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let tracks = r.open_table(TRACKS).map_err(store_err)?;
        let mut out = Vec::new();
        for row in tracks.iter().map_err(store_err)? {
            let (k, v) = row.map_err(store_err)?;
            match serde_json::from_slice::<Track>(v.value()) {
                Ok(t) => out.push(t),
                Err(e) => log::warn!("store: skipping unreadable track {:?}: {e}", k.value()),
            }
        }
        Ok(out)
    }

    fn track_by_isrc(&self, isrc: &str) -> Result<Option<Track>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let idx = r.open_table(IDX_ISRC).map_err(store_err)?;
        let Some(g) = idx.get(isrc).map_err(store_err)? else {
            return Ok(None);
        };
        let id = uuid_from_slice(g.value())?;
        self.get_track(TrackId(id))
    }

    fn track_by_rendition(
        &self,
        source: &crate::types::SourceId,
        uri: &str,
    ) -> Result<Option<Track>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let idx = r.open_table(IDX_RENDITION).map_err(store_err)?;
        let Some(g) = idx
            .get(rendition_key(source.as_str(), uri).as_str())
            .map_err(store_err)?
        else {
            return Ok(None);
        };
        let id = uuid_from_slice(g.value())?;
        self.get_track(TrackId(id))
    }

    fn tracks_by_title_norm(&self, norm_title: &str) -> Result<Vec<Track>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let idx = r.open_multimap_table(IDX_TITLE).map_err(store_err)?;
        let mut out = Vec::new();
        for v in idx.get(norm_title).map_err(store_err)? {
            let v = v.map_err(store_err)?;
            let id = uuid_from_slice(v.value())?;
            if let Some(t) = self.get_track(TrackId(id))? {
                out.push(t);
            }
        }
        Ok(out)
    }

    fn delete_track(&self, id: TrackId) -> Result<()> {
        let key = id.0.as_bytes().to_vec();
        let old_title_norm = self.get_track(id)?.map(|old| Matcher::norm(&old.title));
        let w = self.db.begin_write().map_err(store_err)?;
        {
            let mut tracks = w.open_table(TRACKS).map_err(store_err)?;
            tracks.remove(key.as_slice()).map_err(store_err)?;

            if let Some(norm) = &old_title_norm {
                let mut idx_title = w.open_multimap_table(IDX_TITLE).map_err(store_err)?;
                idx_title.remove(norm.as_str(), key.as_slice()).map_err(store_err)?;
            }

            let mut idx_isrc = w.open_table(IDX_ISRC).map_err(store_err)?;
            let stale: Vec<String> = idx_isrc
                .iter()
                .map_err(store_err)?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    (v.value() == key.as_slice()).then(|| k.value().to_string())
                })
                .collect();
            for k in stale {
                idx_isrc.remove(k.as_str()).map_err(store_err)?;
            }

            let mut idx_rend = w.open_table(IDX_RENDITION).map_err(store_err)?;
            let stale: Vec<String> = idx_rend
                .iter()
                .map_err(store_err)?
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    (v.value() == key.as_slice()).then(|| k.value().to_string())
                })
                .collect();
            for k in stale {
                idx_rend.remove(k.as_str()).map_err(store_err)?;
            }
        }
        w.commit().map_err(store_err)?;
        Ok(())
    }

    fn upsert_playlist(&self, p: &Playlist) -> Result<()> {
        let json = serde_json::to_vec(p).map_err(store_err)?;
        let key = p.id.0.as_bytes().to_vec();
        let w = self.db.begin_write().map_err(store_err)?;
        {
            let mut t = w.open_table(PLAYLISTS).map_err(store_err)?;
            t.insert(key.as_slice(), json.as_slice()).map_err(store_err)?;
        }
        w.commit().map_err(store_err)?;
        Ok(())
    }

    fn get_playlist(&self, id: PlaylistId) -> Result<Option<Playlist>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let t = r.open_table(PLAYLISTS).map_err(store_err)?;
        let key = id.0.as_bytes().to_vec();
        match t.get(key.as_slice()).map_err(store_err)? {
            Some(g) => Ok(Some(serde_json::from_slice(g.value()).map_err(store_err)?)),
            None => Ok(None),
        }
    }

    fn all_playlists(&self) -> Result<Vec<Playlist>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let t = r.open_table(PLAYLISTS).map_err(store_err)?;
        let mut out = Vec::new();
        for row in t.iter().map_err(store_err)? {
            let (_, v) = row.map_err(store_err)?;
            out.push(serde_json::from_slice(v.value()).map_err(store_err)?);
        }
        Ok(out)
    }

    fn delete_playlist(&self, id: PlaylistId) -> Result<()> {
        let key = id.0.as_bytes().to_vec();
        let w = self.db.begin_write().map_err(store_err)?;
        {
            let mut t = w.open_table(PLAYLISTS).map_err(store_err)?;
            t.remove(key.as_slice()).map_err(store_err)?;
        }
        w.commit().map_err(store_err)?;
        Ok(())
    }

    fn get_tracks(&self, ids: &[TrackId]) -> Result<Vec<Track>> {
        // One read transaction for the whole batch, not one per id —
        // `Session::ensure_remote_playlist_tracks` hydrating a many-
        // thousand-track cache from `remote_playlist_ids` is exactly the
        // case that would otherwise pay for it.
        let r = self.db.begin_read().map_err(store_err)?;
        let tracks = r.open_table(TRACKS).map_err(store_err)?;
        Ok(ids
            .iter()
            .filter_map(|id| {
                let key = id.0.as_bytes().to_vec();
                tracks.get(key.as_slice()).ok().flatten().and_then(|g| {
                    serde_json::from_slice::<Track>(g.value()).ok()
                })
            })
            .collect())
    }

    fn remote_playlist_ids(&self, key: &str) -> Result<Vec<TrackId>> {
        let r = self.db.begin_read().map_err(store_err)?;
        let t = r.open_table(REMOTE_PLAYLIST_IDS).map_err(store_err)?;
        match t.get(key).map_err(store_err)? {
            Some(g) => serde_json::from_slice(g.value()).map_err(store_err),
            None => Ok(Vec::new()),
        }
    }

    fn set_remote_playlist_ids(&self, key: &str, ids: &[TrackId]) -> Result<()> {
        let json = serde_json::to_vec(ids).map_err(store_err)?;
        let w = self.db.begin_write().map_err(store_err)?;
        {
            let mut t = w.open_table(REMOTE_PLAYLIST_IDS).map_err(store_err)?;
            t.insert(key, json.as_slice()).map_err(store_err)?;
        }
        w.commit().map_err(store_err)?;
        Ok(())
    }

    fn remote_playlist_folders(&self, source: &str) -> Result<StoredFolders> {
        let r = self.db.begin_read().map_err(store_err)?;
        let folders = r.open_table(REMOTE_PLAYLIST_FOLDERS).map_err(store_err)?;
        let meta = r.open_table(NODE_META).map_err(store_err)?;
        let folders = match folders.get(source).map_err(store_err)? {
            Some(g) => serde_json::from_slice(g.value()).map_err(store_err)?,
            None => Vec::new(),
        };
        let meta = match meta.get(source).map_err(store_err)? {
            Some(g) => serde_json::from_slice(g.value()).map_err(store_err)?,
            None => Vec::new(),
        };
        Ok((folders, meta))
    }

    fn set_remote_playlist_folders(&self, source: &str, folders: &[(String, String)], meta: &[(String, NodeMeta)]) -> Result<()> {
        let folders = serde_json::to_vec(folders).map_err(store_err)?;
        let meta = serde_json::to_vec(meta).map_err(store_err)?;
        let w = self.db.begin_write().map_err(store_err)?;
        {
            let mut t = w.open_table(REMOTE_PLAYLIST_FOLDERS).map_err(store_err)?;
            t.insert(source, folders.as_slice()).map_err(store_err)?;
            let mut t = w.open_table(NODE_META).map_err(store_err)?;
            t.insert(source, meta.as_slice()).map_err(store_err)?;
        }
        w.commit().map_err(store_err)?;
        Ok(())
    }
}

fn uuid_from_slice(b: &[u8]) -> Result<uuid::Uuid> {
    uuid::Uuid::from_slice(b).map_err(store_err)
}

