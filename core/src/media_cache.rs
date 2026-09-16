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
//! is available. Format doesn't need tracking here — every reader (symphonia
//! for decode/analysis, `rodio::Decoder::try_from` for playback) sniffs the
//! container from content, not a filename extension.
//!
//! Keyed by `(SourceId, Rendition::uri)` — the stable, source-attributed id
//! a source actually gives us, sanitized into a filesystem-safe path. Never
//! `TrackId`: that's a medley-only playlist abstraction with no meaning to
//! a source, and a track can have several renditions (one cache entry each).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::types::SourceId;

/// Anything outside this set becomes `_` — keeps entries readable
/// (`ls`-able) while staying safe across filesystems. No hashing: once keyed
/// by a stable source id instead of a resolved/ephemeral URL, there's
/// nothing left that needs one.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// MVP: no eviction — grows unboundedly with distinct renditions cached.
pub struct MediaCache {
    dir: PathBuf,
    /// Per-key locks so two callers wanting the same not-yet-cached
    /// rendition serialize (one decodes+writes, the other waits and then
    /// finds it cached) instead of decoding the same audio twice.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl MediaCache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, inflight: Mutex::new(HashMap::new()) }
    }

    fn key(source: &SourceId, uri: &str) -> String {
        format!("{}:{}", source.as_str(), uri)
    }

    fn path_for(&self, source: &SourceId, uri: &str) -> PathBuf {
        self.dir.join(sanitize(source.as_str())).join(sanitize(uri))
    }

    /// Local path to `(source, uri)`'s cached audio, if already populated.
    /// Never touches the network or decodes anything.
    pub fn cached_path(&self, source: &SourceId, uri: &str) -> Option<PathBuf> {
        let p = self.path_for(source, uri);
        p.exists().then_some(p)
    }

    /// Store already-fetched bytes (whatever playable format the caller
    /// hands over) for `(source, uri)`, persisting atomically via a temp
    /// file in the same directory. Locks per-key so concurrent writers for
    /// the same rendition (a scan plugin racing `play_from_cache`'s
    /// decode-on-demand) don't clobber each other.
    pub fn put(&self, source: &SourceId, uri: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        self.store(source, uri, |f| io::Write::write_all(f, bytes))
    }

    /// Like `put`, but copies from an existing local file instead of
    /// buffering it in memory — for a caller (e.g. a player that just
    /// downloaded, or already has, a local copy) that already has the bytes
    /// on disk.
    pub fn put_file(&self, source: &SourceId, uri: &str, src: &std::path::Path) -> io::Result<PathBuf> {
        self.store(source, uri, |f| {
            io::copy(&mut std::fs::File::open(src)?, f).map(|_| ())
        })
    }

    fn store(
        &self,
        source: &SourceId,
        uri: &str,
        write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        let lock = {
            let mut inflight = self.inflight.lock().unwrap();
            inflight.entry(Self::key(source, uri)).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
        };
        let _guard = lock.lock().unwrap();

        let dest = self.path_for(source, uri);
        let dir = dest.parent().expect("path_for always has a parent");
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        write(tmp.as_file_mut())?;
        tmp.persist(&dest).map_err(|e| e.error)?;
        Ok(dest)
    }
}
