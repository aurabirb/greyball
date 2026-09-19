//! `SoulseekSource` — `Source` + `MediaProvider` backed by a local `slskd`.
//!
//! Search hits a live `slskd` search and converts its file responses
//! straight to `SearchHit`s (capped at `client::MAX_RESULTS`, see the TODO
//! this closes). Playback has no CDN URL to stream: `open` enqueues a
//! download through slskd's own transfer queue, blocks polling until it
//! lands on disk, then hands back that file's `Media::Path` — same shape
//! the player already expects for a local file, no new `Player`/`MediaCache`
//! plumbing needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use core::{
    BrowseNode, BrowsePage, Error, Media, MediaProvider, Quality, Rendition, Result, SearchHit,
    SearchQuery, Source, SourceId,
};

use crate::client::{MAX_RESULTS, SearchFile, SearchResponse, SlskdClient};
use crate::uri::TrackRef;

/// How long `open` waits for a download to finish before giving up. A
/// Soulseek transfer is a full peer-to-peer file copy, not a CDN hit — this
/// has to be generous, but a hung/queued-forever peer still needs a ceiling.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(750);
/// How long a search is allowed to keep collecting responses server-side.
const SEARCH_TIMEOUT_SECS: u32 = 12;

fn source_id() -> SourceId {
    SourceId::from("soulseek")
}

fn src_err(message: impl Into<String>) -> Error {
    Error::Source { src: source_id(), message: message.into() }
}

pub struct SoulseekSource {
    client: SlskdClient,
    /// slskd's downloads directory as this machine sees it — `None` until plugin
    /// setup knows it, in which case playback (but not search) is unavailable.
    downloads_dir: Option<PathBuf>,
    /// Tracks already downloaded this run, so replaying the same search hit
    /// doesn't re-enqueue a fresh peer-to-peer transfer for it.
    downloaded: Mutex<HashMap<String, PathBuf>>,
}

impl SoulseekSource {
    pub fn new(client: SlskdClient, downloads_dir: Option<PathBuf>) -> Self {
        Self { client, downloads_dir, downloaded: Mutex::new(HashMap::new()) }
    }

    fn downloads_dir(&self) -> Result<&Path> {
        self.downloads_dir
            .as_deref()
            .ok_or_else(|| src_err("no slskd downloads directory known — finish setup from the warnings panel"))
    }

    /// Downloads `track` (blocking) and returns its local path, first
    /// checking (then populating) `downloaded`.
    fn fetch(&self, uri: &str, track: &TrackRef) -> Result<PathBuf> {
        let cached = self.downloaded.lock().unwrap().get(uri).cloned();
        if let Some(p) = cached
            && p.exists()
        {
            return Ok(p);
        }

        let downloads_dir = self.downloads_dir()?;
        let batch_id = self
            .client
            .enqueue_download(&track.username, &track.filename, track.size)
            .map_err(src_err)?;
        let dest_dir = downloads_dir.join("medley").join(batch_id.to_string());

        let deadline = Instant::now() + DOWNLOAD_TIMEOUT;
        loop {
            match self.client.batch_transfer(batch_id) {
                Ok(Some(t)) if t.succeeded() => break,
                Ok(Some(t)) if t.failed() => {
                    return Err(src_err(format!(
                        "download failed: {}",
                        t.exception.unwrap_or(t.state)
                    )));
                }
                Ok(_) => {}
                Err(e) => log::warn!("soulseek: polling download {batch_id}: {e}"),
            }
            if Instant::now() >= deadline {
                self.client.cancel_download(&track.username, batch_id);
                return Err(src_err(format!(
                    "download timed out after {}s — the peer may be offline or too slow",
                    DOWNLOAD_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }

        let entry = std::fs::read_dir(&dest_dir)
            .map_err(|e| src_err(format!("reading {}: {e}", dest_dir.display())))?
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_file())
            .ok_or_else(|| src_err("download reported complete but no file was found"))?;
        let path = entry.path();
        self.downloaded.lock().unwrap().insert(uri.to_string(), path.clone());
        Ok(path)
    }
}

impl Source for SoulseekSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    /// Soulseek has no pasteable per-file URL scheme of its own — every
    /// `soulseek:` uri this source produces is internal, never something a
    /// user copies from elsewhere.
    fn recognizes(&self, _uri: &str) -> bool {
        false
    }

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(SearchHit)) -> Result<()> {
        let text = q.text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let responses = self.client.search(text, SEARCH_TIMEOUT_SECS).map_err(src_err)?;
        let mut count = 0usize;
        for resp in &responses {
            for file in &resp.files {
                if count >= MAX_RESULTS {
                    return Ok(());
                }
                if let Some(hit) = file_to_hit(resp, file) {
                    sink(hit);
                    count += 1;
                }
            }
        }
        Ok(())
    }

    fn resolve(&self, uri: &str) -> Result<SearchHit> {
        let track = TrackRef::parse(uri).ok_or_else(|| src_err(format!("not a soulseek track: {uri:?}")))?;
        // No per-file lookup endpoint exists outside a live search — best
        // effort from the filename alone (still enough to play/queue it).
        let (mut artists, title) = core::parse_artist_title(&stem(&track.filename));
        if artists.is_empty() {
            artists.push(track.username.clone());
        }
        Ok(SearchHit {
            source: source_id(),
            uri: uri.to_string(),
            title,
            artists,
            duration_ms: 0,
            isrc: None,
            album: None,
            quality: quality_for(&extension(&track.filename), None, None, None),
        })
    }

    fn browse(&self, node: &BrowseNode, _want: usize) -> Result<BrowsePage> {
        match node {
            // MVP: search-only, no user/library browsing (that's a
            // different slskd API — `GET /users/{username}/browse` — out of
            // scope here).
            BrowseNode::Root => Ok(BrowsePage {
                title: "Soulseek".to_string(),
                tracks: vec![],
                folders: vec![],
                partial: false,
                errored: false,
            }),
            BrowseNode::Path(id) => Err(src_err(format!("no such Soulseek folder: {id}"))),
        }
    }
}

impl MediaProvider for SoulseekSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn open(&self, r: &Rendition) -> Result<Media> {
        let track = TrackRef::parse(&r.uri).ok_or_else(|| src_err(format!("not a soulseek track: {:?}", r.uri)))?;
        let path = self.fetch(&r.uri, &track)?;
        Ok(Media::Path(path))
    }
}

/// Basename with directory separators (Soulseek filenames are Windows-style,
/// `\`-separated) and extension stripped, for `parse_artist_title`.
fn stem(filename: &str) -> String {
    let base = filename.rsplit(['\\', '/']).next().unwrap_or(filename);
    match base.rfind('.') {
        Some(i) if i > 0 => base[..i].to_string(),
        _ => base.to_string(),
    }
}

fn extension(filename: &str) -> String {
    let base = filename.rsplit(['\\', '/']).next().unwrap_or(filename);
    base.rsplit_once('.').map(|(_, ext)| ext.to_lowercase()).unwrap_or_default()
}

fn quality_for(ext: &str, bit_rate: Option<u32>, bit_depth: Option<u32>, sample_rate: Option<u32>) -> Quality {
    match ext {
        "flac" | "wav" | "alac" | "ape" | "wv" => Quality::Lossless { bits: bit_depth.map(|b| b as u8), hz: sample_rate },
        "mp3" | "ogg" | "m4a" | "opus" | "wma" | "aac" => Quality::Lossy { kbps: bit_rate },
        _ if bit_rate.is_some() => Quality::Lossy { kbps: bit_rate },
        _ => Quality::Unknown,
    }
}

fn file_to_hit(resp: &SearchResponse, file: &SearchFile) -> Option<SearchHit> {
    let track = TrackRef { username: resp.username.clone(), filename: file.filename.clone(), size: file.size };
    let (mut artists, title) = core::parse_artist_title(&stem(&file.filename));
    if title.trim().is_empty() {
        return None;
    }
    if artists.is_empty() {
        artists.push(resp.username.clone());
    }
    Some(SearchHit {
        source: source_id(),
        uri: track.to_uri(),
        title,
        artists,
        duration_ms: file.length.unwrap_or(0).saturating_mul(1000),
        isrc: None,
        album: None,
        quality: quality_for(&file.extension.to_lowercase(), file.bit_rate, file.bit_depth, file.sample_rate),
    })
}
