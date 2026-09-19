//! Persistent on-disk cache of a rendition's playable audio — the single
//! cache every source funnels through: a `ScanPlugin`, `RodioPlayer`, or
//! Spotify's playback materializer (`sources/spotify/src/player.rs`) all
//! write here on a cache miss, and playback/scanning read straight from it
//! before touching a source's own fetch mechanics. `MediaCache` itself
//! never originates a fetch — it's just the on-disk store.
//!
//! Entries are whatever bytes the caller hands `put` — no forced re-encode.
//! `audio_decode::decode_and_cache` stores a source's own already-fetched
//! bytes as-is whenever they're already a plain, unencrypted, playable file
//! (HTTP/SoundCloud's fetched MP3/AAC, Spotify's already-decrypted,
//! header-stripped Ogg Vorbis), only transcoding when nothing playable-as-is
//! is available. Every reader (symphonia for decode/analysis,
//! `rodio::Decoder::try_from` for playback) sniffs the container from
//! content, not a filename extension, so lookups here never depend on one —
//! but a first-time write does cheaply sniff the magic bytes (`sniff_ext`)
//! to append `.mp3`/`.ogg` to the filename purely for `ls`-ability.
//!
//! Keyed by `(SourceId, Rendition::uri)` — the stable, source-attributed id
//! a source actually gives us. Never `TrackId`: that's a medley-only
//! playlist abstraction with no meaning to a source, and a track can have
//! several renditions (one cache entry each). `put`/`put_file`/`link_local`/
//! `cached_path` all still key on `(source, uri)` exactly as before — that
//! didn't change.
//!
//! What changed is what a `(source, uri)` maps to on disk: instead of
//! sanitizing `uri` itself into the filename (opaque, sometimes a long
//! id/token), the file is named after the track's own "Artist - Title"
//! (`Track::display_name`), which two different tracks can share, so a
//! short disambiguating suffix (`unique_filename`) is appended on collision.
//! That mapping — `(source, uri)` -> chosen filename — is the one piece of
//! real state this module keeps beyond the bytes themselves, in a small redb
//! sidecar database (`<dir>.redb`, a sibling of the cache directory, not
//! nested inside it). The filename is a readability nicety for anyone
//! `ls`-ing the cache dir; the redb index is the actual source of truth
//! mapping it back to `(source, uri)`, and every lookup here still goes
//! through `(source, uri)`, never the filename.
//!
//! Resolving a display name needs a `Track`, which `MediaCache` doesn't
//! otherwise have — it takes a `Store` handle solely to look one up
//! (`Store::track_by_rendition`) at write time, and, for `prune_orphans`, to
//! check whether a `(source, uri)` still belongs to any track at all. That
//! `Store` handle is never used to originate a fetch, and deliberately isn't
//! threaded through `Player`/`RodioPlayer`/`SpotifyPlayer`'s own `put`/
//! `put_file`/`link_local` call sites — those stay exactly as they were,
//! passing only `(source, uri, bytes-or-path)`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use redb::{Database, ReadableTable, TableDefinition};

use crate::traits::Store;
use crate::types::SourceId;

/// `(source, uri)` (via `index_key`) -> the filename it was assigned inside
/// that source's cache subdirectory.
const INDEX: TableDefinition<&str, &str> = TableDefinition::new("index");

fn index_key(source: &SourceId, uri: &str) -> String {
    format!("{}\0{}", source.as_str(), uri)
}

fn to_io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Anything outside this set becomes `_` — keeps entries readable
/// (`ls`-able) while staying safe across filesystems. No hashing: once keyed
/// by a stable source id instead of a resolved/ephemeral URL, there's
/// nothing left that needs one.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// Like `sanitize`, but for a human display name: keeps spaces and common
/// punctuation instead of collapsing everything non-ASCII-alphanumeric to
/// `_`, and caps the length so a long title can't run into filesystem
/// limits.
fn sanitize_display(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '\'' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.trim().chars().take(120).collect()
}

/// Recognizes the magic bytes of the containers actually observed in this
/// cache (raw or ID3-tagged MPEG audio, Ogg Vorbis, and stitched-HLS fMP4
/// AAC) — cheap sniff only, never a format probe. Anything else (or too few
/// bytes) is `None`, which leaves the filename extension-less, matching
/// prior behavior.
fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 4 && &bytes[..4] == b"OggS" {
        return Some("ogg");
    }
    if bytes.len() >= 3 && &bytes[..3] == b"ID3" {
        return Some("mp3");
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0 {
        return Some("mp3");
    }
    if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        return Some("m4a");
    }
    None
}

/// Same sniff as `sniff_ext`, but for a caller that only has a path
/// (`put_file`/`link_local`) — reads a handful of bytes, not the whole file.
fn sniff_ext_path(path: &Path) -> Option<&'static str> {
    use std::io::Read;
    let mut buf = [0u8; 8];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    sniff_ext(&buf[..n])
}

/// `dir/base(.ext)`, `dir/base (2)(.ext)`, `dir/base (3)(.ext)`, ... — two
/// different `(source, uri)` can share a display name, so the pretty name
/// alone isn't a unique filename; this just needs to not collide on disk,
/// not be race-proof against a concurrent first-time write for a
/// *different* key with the same name landing on the same candidate (rare,
/// and no worse than any other best-effort naming scheme here — a same-key
/// rewrite never calls this, it always reuses its already-indexed filename).
/// The disambiguator goes before `ext`, not after, so collisions read as
/// `Title (2).mp3` rather than `Title.mp3 (2)`.
fn unique_filename(dir: &Path, base: &str, ext: Option<&str>) -> String {
    let name = |suffix: Option<u32>| match (suffix, ext) {
        (None, None) => base.to_string(),
        (None, Some(e)) => format!("{base}.{e}"),
        (Some(n), None) => format!("{base} ({n})"),
        (Some(n), Some(e)) => format!("{base} ({n}).{e}"),
    };
    if !dir.join(name(None)).exists() {
        return name(None);
    }
    let mut n = 2;
    loop {
        let candidate = name(Some(n));
        if !dir.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// MVP: no eviction — grows unboundedly with distinct renditions cached.
pub struct MediaCache {
    dir: PathBuf,
    /// The naming/pruning index, persisted — never audio bytes, see module
    /// doc. `index_cache` below is what every lookup actually reads; this is
    /// only touched on a write (a fresh assignment, or a prune removal).
    index: Database,
    /// In-memory mirror of `index`, loaded once at startup: `cached_path`
    /// (and `filename` generally) is on the hot path for every row of every
    /// list built (`ui::view::tracks_to_rows` calls `is_track_cached` once
    /// per track), so a redb read transaction per lookup — cheap in
    /// isolation — added up to a visible UI stall for a library in the
    /// thousands. A plain `RwLock<HashMap>` read has none of that
    /// per-transaction overhead. Kept in sync with `index` on every write
    /// (see `dest`) and removal (see `prune_orphans`).
    index_cache: RwLock<HashMap<String, String>>,
    /// Read-only from here: resolves a display name at write time, and
    /// checks a track still exists at prune time.
    store: Arc<dyn Store>,
    /// Per-key locks so two callers wanting the same not-yet-cached
    /// rendition serialize (one decodes+writes, the other waits and then
    /// finds it cached) instead of decoding the same audio twice.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl MediaCache {
    pub fn new(dir: PathBuf, store: Arc<dyn Store>) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let index_path = dir.with_extension("redb");
        let index = Database::create(&index_path)
            .unwrap_or_else(|e| panic!("media cache index {}: {e}", index_path.display()));
        {
            // ensure the table exists so a read before any write never hits TableDoesNotExist
            let w = index.begin_write().expect("media cache index: begin_write");
            w.open_table(INDEX).expect("media cache index: open_table");
            w.commit().expect("media cache index: commit");
        }
        let index_cache = {
            let r = index.begin_read().expect("media cache index: begin_read");
            let t = r.open_table(INDEX).expect("media cache index: open_table");
            let map = t
                .iter()
                .expect("media cache index: iter")
                .filter_map(|row| {
                    let (k, v) = row.ok()?;
                    Some((k.value().to_string(), v.value().to_string()))
                })
                .collect();
            RwLock::new(map)
        };
        Self { dir, index, index_cache, store, inflight: Mutex::new(HashMap::new()) }
    }

    fn key(source: &SourceId, uri: &str) -> String {
        format!("{}:{}", source.as_str(), uri)
    }

    fn filename(&self, source: &SourceId, uri: &str) -> Option<String> {
        self.index_cache.read().unwrap().get(&index_key(source, uri)).cloned()
    }

    /// "Artist - Title" for `(source, uri)` via the `Store`'s own
    /// `(SourceId, uri)` -> `Track` index — falls back to the raw `uri` if
    /// the track isn't (or isn't yet) in the `Store`, so a cache write never
    /// stalls or fails just because the name lookup came up empty.
    fn display_name(&self, source: &SourceId, uri: &str) -> String {
        self.store
            .track_by_rendition(source, uri)
            .ok()
            .flatten()
            .map(|t| t.display_name())
            .unwrap_or_else(|| uri.to_string())
    }

    /// Destination path for `(source, uri)`, assigning and persisting a
    /// fresh human-readable filename the first time this identity is ever
    /// cached. Later calls for the same key always return the same
    /// filename, even if the track's title later changes — the redb index,
    /// not the `Store`'s current state, is the source of truth for an
    /// already-cached entry's on-disk name. `ext` (from `sniff_ext`) is only
    /// used on that first-write branch; an already-indexed entry keeps its
    /// existing filename regardless of what this call's `ext` is.
    fn dest(&self, source: &SourceId, uri: &str, ext: Option<&str>) -> io::Result<PathBuf> {
        let source_dir = self.dir.join(sanitize(source.as_str()));
        let filename = match self.filename(source, uri) {
            Some(f) => f,
            None => {
                let base = sanitize_display(&self.display_name(source, uri));
                let base = if base.is_empty() { sanitize(uri) } else { base };
                let filename = unique_filename(&source_dir, &base, ext);
                let w = self.index.begin_write().map_err(to_io_err)?;
                {
                    let mut t = w.open_table(INDEX).map_err(to_io_err)?;
                    t.insert(index_key(source, uri).as_str(), filename.as_str()).map_err(to_io_err)?;
                }
                w.commit().map_err(to_io_err)?;
                self.index_cache.write().unwrap().insert(index_key(source, uri), filename.clone());
                filename
            }
        };
        Ok(source_dir.join(filename))
    }

    /// Local path to `(source, uri)`'s cached audio, if already populated.
    /// Never touches the network or decodes anything.
    pub fn cached_path(&self, source: &SourceId, uri: &str) -> Option<PathBuf> {
        let filename = self.filename(source, uri)?;
        let p = self.dir.join(sanitize(source.as_str())).join(filename);
        p.exists().then_some(p)
    }

    /// Store already-fetched bytes (whatever playable format the caller
    /// hands over) for `(source, uri)`, persisting atomically via a temp
    /// file in the same directory. Locks per-key so concurrent writers for
    /// the same rendition (a scan plugin racing `play_from_cache`'s
    /// decode-on-demand) don't clobber each other.
    pub fn put(&self, source: &SourceId, uri: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        let ext = sniff_ext(bytes);
        self.store_bytes(source, uri, ext, |f| io::Write::write_all(f, bytes))
    }

    /// Like `put`, but copies from an existing local file instead of
    /// buffering it in memory — for a caller (e.g. a player that just
    /// downloaded, or already has, a local copy) that already has the bytes
    /// on disk.
    pub fn put_file(&self, source: &SourceId, uri: &str, src: &std::path::Path) -> io::Result<PathBuf> {
        let ext = sniff_ext_path(src);
        self.store_bytes(source, uri, ext, |f| {
            io::copy(&mut std::fs::File::open(src)?, f).map(|_| ())
        })
    }

    /// Records that `(source, uri)`'s audio already exists locally at
    /// `src` — a symlink, not a copy, since nothing needs duplicating (e.g.
    /// Soulseek's own downloads dir). If `src` later disappears, so does
    /// this entry: `cached_path`'s `exists()` follows symlinks and fails
    /// closed on a dangling one, so a deleted/moved source just goes back
    /// to "not cached" instead of serving a broken path.
    pub fn link_local(&self, source: &SourceId, uri: &str, src: &std::path::Path) -> io::Result<PathBuf> {
        let lock = self.lock(source, uri);
        let _guard = lock.lock().unwrap();
        let ext = sniff_ext_path(src);
        let dest = self.dest(source, uri, ext)?;
        std::fs::create_dir_all(dest.parent().expect("dest always has a parent"))?;
        let _ = std::fs::remove_file(&dest);
        #[cfg(unix)]
        std::os::unix::fs::symlink(src, &dest)?;
        #[cfg(not(unix))]
        std::fs::copy(src, &dest).map(|_| ())?;
        Ok(dest)
    }

    fn lock(&self, source: &SourceId, uri: &str) -> Arc<Mutex<()>> {
        let mut inflight = self.inflight.lock().unwrap();
        inflight.entry(Self::key(source, uri)).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    fn store_bytes(
        &self,
        source: &SourceId,
        uri: &str,
        ext: Option<&str>,
        write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        let lock = self.lock(source, uri);
        let _guard = lock.lock().unwrap();

        let dest = self.dest(source, uri, ext)?;
        let dir = dest.parent().expect("dest always has a parent");
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        write(tmp.as_file_mut())?;
        tmp.persist(&dest).map_err(|e| e.error)?;
        Ok(dest)
    }

    /// Drops any index entry (and its cache file) whose `(source, uri)` no
    /// longer resolves to a track in the `Store` — e.g. a rendition removed
    /// from its source. Run once at startup (`app/src/main.rs`, on a
    /// background thread so it never delays the UI coming up) rather than on
    /// a recurring timer: entries only go stale between runs (a source
    /// dropping a rendition while medley isn't running), never mid-session,
    /// so there's nothing a periodic sweep would catch that a startup one
    /// wouldn't. Deletes the orphaned file too, not just the index row — a
    /// stale multi-hundred-MB audio file with no index entry pointing at it
    /// is worse than the row.
    pub fn prune_orphans(&self) {
        let stale: Vec<(String, String)> =
            self.index_cache.read().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let mut removed = 0usize;
        for (key, filename) in stale {
            let Some((source, uri)) = key.split_once('\0') else { continue };
            let still_live =
                self.store.track_by_rendition(&SourceId::new(source), uri).ok().flatten().is_some();
            if still_live {
                continue;
            }
            let path = self.dir.join(sanitize(source)).join(&filename);
            let _ = std::fs::remove_file(&path);
            if let Ok(w) = self.index.begin_write() {
                if let Ok(mut t) = w.open_table(INDEX) {
                    let _ = t.remove(key.as_str());
                }
                let _ = w.commit();
            }
            self.index_cache.write().unwrap().remove(&key);
            removed += 1;
        }
        if removed > 0 {
            log::info!(
                "media_cache: pruned {removed} orphaned cache entr{}",
                if removed == 1 { "y" } else { "ies" }
            );
        }
    }
}
