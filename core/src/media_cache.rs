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
//! several renditions (one cache entry each). `put`/`persist_file`/`link_local`/
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
//! through `(source, uri)`, never the filename. Extra `<name>[-_. ]*.redb` files beside the index are merged at startup.
//!
//! Resolving a display name needs a `Track`, which `MediaCache` doesn't
//! otherwise have — it takes a `Store` handle solely to look one up
//! (`Store::track_by_rendition`) at write time. That
//! `Store` handle is never used to originate a fetch, and deliberately isn't
//! threaded through `Player`/`RodioPlayer`/`SpotifyPlayer`'s own `put`/
//! `persist_file`/`link_local` call sites — those stay exactly as they were,
//! passing only `(source, uri, bytes-or-path)`.

use std::collections::HashMap;
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use redb::{Database, ReadableTable, TableDefinition};

use crate::traits::Store;
use crate::types::SourceId;

/// `(source, uri)` (via `index_key`) -> the filename it was assigned inside
/// that source's cache subdirectory.
const INDEX: TableDefinition<&str, &str> = TableDefinition::new("index");

/// Subdirectory of the cache dir holding in-progress stream files.
const STREAM_DIR: &str = ".streams";

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
/// (`persist_file`/`link_local`) — reads a handful of bytes, not the whole file.
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
    /// Read-only from here: resolves a display name at write time.
    store: Arc<dyn Store>,
    /// Per-key locks so two callers wanting the same not-yet-cached
    /// rendition serialize (one decodes+writes, the other waits and then
    /// finds it cached) instead of decoding the same audio twice.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl MediaCache {
    pub fn new(dir: PathBuf, store: Arc<dyn Store>) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.join(STREAM_DIR));
        let index_path = dir.with_extension("redb");
        let index = Database::create(&index_path)
            .unwrap_or_else(|e| panic!("media cache index {}: {e}", index_path.display()));
        {
            // ensure the table exists so a read before any write never hits TableDoesNotExist
            let w = index.begin_write().expect("media cache index: begin_write");
            w.open_table(INDEX).expect("media cache index: open_table");
            w.commit().expect("media cache index: commit");
        }
        let index = ingest_extra_indexes(index, &dir, &index_path);
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

    /// Moves a finished stream's temp file (from `new_stream_file`) into place as `(source, uri)`'s entry. Writes
    /// the index, so call it from a producer thread, never the UI or audio thread.
    pub fn persist_file(&self, source: &SourceId, uri: &str, src: &Path) -> io::Result<PathBuf> {
        let lock = self.lock(source, uri);
        let _guard = lock.lock().unwrap();
        let dest = self.dest(source, uri, sniff_ext_path(src))?;
        std::fs::create_dir_all(dest.parent().expect("dest always has a parent"))?;
        std::fs::rename(src, &dest)?;
        Ok(dest)
    }

    /// An empty file for a stream to fill, inside the cache dir so `persist_file` can rename it.
    pub fn new_stream_file(&self) -> io::Result<(std::fs::File, PathBuf)> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = self.dir.join(STREAM_DIR);
        std::fs::create_dir_all(&dir)?;
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("{}-{n}.part", std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok((file, path))
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

    /// Logs how many files the cache dir holds versus how many index entries exist.
    pub fn log_file_stats(&self) {
        let mut files = 0usize;
        let mut pending = vec![self.dir.clone()];
        while let Some(d) = pending.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for entry in rd.filter_map(|e| e.ok()) {
                match entry.file_type() {
                    Ok(t) if t.is_dir() => pending.push(entry.path()),
                    Ok(_) => files += 1,
                    Err(_) => {}
                }
            }
        }
        let indexed = self.index_cache.read().unwrap().len();
        log::info!(
            "media_cache: {files} files in cache dir, {indexed} in index ({} without a record)",
            files.saturating_sub(indexed)
        );
    }

    /// Drops index entries whose cache file no longer exists; never deletes files.
    pub fn prune_orphans(&self) {
        let entries: Vec<(String, String)> =
            self.index_cache.read().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let mut dropped = 0usize;
        for chunk in entries.chunks(1000) {
            let missing: Vec<&String> = chunk
                .iter()
                .filter_map(|(key, filename)| {
                    let (source, _) = key.split_once('\0')?;
                    (!self.dir.join(sanitize(source)).join(filename).exists()).then_some(key)
                })
                .collect();
            if missing.is_empty() {
                continue;
            }

            let committed = (|| -> Result<(), Box<dyn std::error::Error>> {
                let w = self.index.begin_write()?;
                {
                    let mut t = w.open_table(INDEX)?;
                    for key in &missing {
                        t.remove(key.as_str())?;
                    }
                }
                w.commit()?;
                Ok(())
            })();
            if let Err(e) = committed {
                log::warn!("media_cache: prune failed: {e}");
                continue;
            }

            let mut cache = self.index_cache.write().unwrap();
            for key in &missing {
                cache.remove(*key);
            }
            dropped += missing.len();
        }
        if dropped > 0 {
            log::info!(
                "media_cache: dropped {dropped} index entr{} whose files are missing",
                if dropped == 1 { "y" } else { "ies" }
            );
        }
    }
}

/// Merges sibling `<cache dir name>[-_. ]*.redb` files into the live index (existing keys win), then deletes them.
fn ingest_extra_indexes(index: Database, dir: &Path, index_path: &Path) -> Database {
    let (Some(parent), Some(stem), Some(live_name)) = (index_path.parent(), dir.file_name(), index_path.file_name())
    else {
        return index;
    };
    let stem = stem.to_string_lossy().into_owned();
    let Ok(read_dir) = std::fs::read_dir(parent) else { return index };
    let extras: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            let sep_rest = n
                .strip_suffix(".redb")
                .and_then(|s| s.strip_prefix(stem.as_str()))
                .is_some_and(|r| r.starts_with(['-', '_', '.', ' ']));
            name != live_name && sep_rest && e.path().is_file()
        })
        .map(|e| e.path())
        .collect();
    if extras.is_empty() {
        return index;
    }

    let mut bak = index_path.as_os_str().to_owned();
    bak.push(".bak");
    let bak = PathBuf::from(bak);
    if let Err(e) = std::fs::copy(index_path, &bak) {
        log::warn!("media_cache: index backup {} failed, skipping ingest: {e}", bak.display());
        return index;
    }

    let mut merged = Vec::new();
    for extra in extras {
        let incoming = match (|| -> Result<Database, Box<dyn std::error::Error>> {
            let db = Database::open(&extra)?;
            db.begin_read()?.open_table(INDEX)?;
            Ok(db)
        })() {
            Ok(db) => db,
            Err(e) => {
                log::warn!("media_cache: {} is not a usable index, left in place: {e}", extra.display());
                continue;
            }
        };
        match merge_index(&incoming, &index, dir) {
            Ok((inserted, fileless, skipped)) => {
                drop(incoming);
                let note = if fileless > 0 {
                    format!(" ({fileless} without a file, will be dropped by the next prune)")
                } else {
                    String::new()
                };
                log::info!("media_cache: ingested {}: inserted {inserted}{note}, skipped {skipped}", extra.display());
                merged.push(extra);
            }
            Err(e) => {
                log::warn!("media_cache: ingest of {} failed, restoring backup: {e}", extra.display());
                drop(incoming);
                drop(index);
                let mut tmp = index_path.as_os_str().to_owned();
                tmp.push(".restore.tmp");
                let tmp = PathBuf::from(tmp);
                if let Err(e) = std::fs::copy(&bak, &tmp) {
                    let _ = std::fs::remove_file(&tmp);
                    panic!("media cache index restore {}: {e}", bak.display());
                }
                std::fs::rename(&tmp, index_path)
                    .unwrap_or_else(|e| panic!("media cache index restore {}: {e}", index_path.display()));
                let restored = Database::create(index_path)
                    .unwrap_or_else(|e| panic!("media cache index {}: {e}", index_path.display()));
                return restored;
            }
        }
    }
    for extra in merged {
        if let Err(e) = std::fs::remove_file(&extra) {
            log::warn!("media_cache: ingested {} but could not delete it: {e}", extra.display());
        }
    }
    index
}

fn merge_index(from: &Database, to: &Database, dir: &Path) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    let read = from.begin_read()?;
    let src = read.open_table(INDEX)?;
    let (mut inserted, mut fileless, mut skipped) = (0, 0, 0);
    let mut last: Option<String> = None;
    loop {
        let chunk: Vec<(String, String)> = match &last {
            Some(k) => src.range::<&str>((Bound::Excluded(k.as_str()), Bound::Unbounded))?,
            None => src.range::<&str>(..)?,
        }
        .take(1000)
        .map(|row| row.map(|(k, v)| (k.value().to_string(), v.value().to_string())))
        .collect::<Result<_, _>>()?;
        let Some((k, _)) = chunk.last() else { break };
        last = Some(k.clone());

        let w = to.begin_write()?;
        {
            let mut t = w.open_table(INDEX)?;
            for (k, v) in &chunk {
                if t.get(k.as_str())?.is_some() {
                    skipped += 1;
                } else {
                    t.insert(k.as_str(), v.as_str())?;
                    inserted += 1;
                    if let Some((source, _)) = k.split_once('\0') {
                        fileless += usize::from(!dir.join(sanitize(source)).join(v).exists());
                    }
                }
            }
        }
        w.commit()?;
    }
    Ok((inserted, fileless, skipped))
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dest) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dest)?;
        } else if kind.is_dir() {
            copy_tree(&src, &dest)?;
        } else {
            std::fs::copy(&src, &dest)?;
        }
    }
    Ok(())
}

fn clear_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if entry.file_type().is_ok_and(|k| k.is_dir()) { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        }
    }
}

/// Moves a cache (directory plus its `.redb` index) to `to`, which must hold no cache yet. Renames on one
/// filesystem, else copies and deletes the old only after the copy fully succeeded; on error the old cache is intact.
pub fn move_cache(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (from_idx, to_idx) = (from.with_extension("redb"), to.with_extension("redb"));
    if to_idx.exists() || std::fs::read_dir(to).is_ok_and(|mut d| d.next().is_some()) {
        return Err(io::Error::other(format!("{} already holds a cache", to.display())));
    }
    std::fs::create_dir_all(to)?;
    let has_idx = from_idx.exists();
    if !from.exists() && !has_idx {
        return Ok(());
    }
    if from.metadata()?.dev() == to.metadata()?.dev() {
        if has_idx {
            std::fs::rename(&from_idx, &to_idx)?;
        }
        if let Err(e) = std::fs::rename(from, to) {
            if has_idx {
                let _ = std::fs::rename(&to_idx, &from_idx);
            }
            return Err(e);
        }
        return Ok(());
    }
    let copied = copy_tree(from, to).and_then(|()| if has_idx { std::fs::copy(&from_idx, &to_idx).map(drop) } else { Ok(()) });
    if let Err(e) = copied {
        clear_dir(to);
        let _ = std::fs::remove_file(&to_idx);
        return Err(e);
    }
    std::fs::remove_dir_all(from)?;
    if has_idx {
        std::fs::remove_file(&from_idx)?;
    }
    Ok(())
}
