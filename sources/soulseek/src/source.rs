//! `SoulseekSource` — `Source` + `MediaProvider` backed by a local `slskd`.
//!
//! Search hits a live `slskd` search and converts its file responses
//! straight to `Track`s (capped at `client::MAX_RESULTS`, see the TODO
//! this closes). Playback has no CDN URL to stream: `open` enqueues a
//! download through slskd's own transfer queue, blocks polling until it
//! lands on disk, then hands back that file's `Media::Path` — same shape
//! the player already expects for a local file, no new `Player`/`MediaCache`
//! plumbing needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{
    BrowseNode, BrowsePage, Error, Media, MediaProvider, Quality, Rendition, Result, Track,
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
    /// Per-uri lock so concurrent fetches of one track share a single download.
    in_flight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl SoulseekSource {
    pub fn new(client: SlskdClient, downloads_dir: Option<PathBuf>) -> Self {
        Self { client, downloads_dir, downloaded: Mutex::new(HashMap::new()), in_flight: Mutex::new(HashMap::new()) }
    }

    fn downloads_dir(&self) -> Result<&Path> {
        self.downloads_dir
            .as_deref()
            .ok_or_else(|| src_err("no slskd downloads directory known — finish setup from the warnings panel"))
    }

    /// Downloads `track` (blocking) and returns its local path, first
    /// checking (then populating) `downloaded`.
    fn fetch(&self, uri: &str, track: &TrackRef) -> Result<PathBuf> {
        let lock = self.in_flight.lock().unwrap().entry(uri.to_string()).or_default().clone();
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
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
            .find(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .ok_or_else(|| src_err("download reported complete but no file was found"))?;
        let path = place_download(&downloads_dir.join("medley"), &entry.path(), &track.filename)?;
        let _ = std::fs::remove_dir(&dest_dir);
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

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(Track)) -> Result<()> {
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
                if let Some(hit) = track_from_file(resp, file) {
                    sink(hit);
                    count += 1;
                }
            }
        }
        Ok(())
    }

    fn resolve(&self, uri: &str) -> Result<Track> {
        let track = TrackRef::parse(uri).ok_or_else(|| src_err(format!("not a soulseek track: {uri:?}")))?;
        // No per-file lookup endpoint exists outside a live search — best
        // effort from the filename alone (still enough to play/queue it).
        let (mut artists, title) = core::parse_artist_title(&stem(&track.filename));
        if artists.is_empty() {
            artists.push(track.username.clone());
        }
        let quality = quality_for(&extension(&track.filename), None, None, None);
        Ok(Track::fresh(title, artists, Rendition::fresh(source_id(), uri.to_string(), 0, quality)))
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

const MAX_COMPONENT_BYTES: usize = 255;

/// One path component derived from remote data: never empty, hidden, or containing separators/control chars.
fn sanitize_component(raw: &str, placeholder: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).map(|c| if matches!(c, '/' | '\\') { '_' } else { c }).collect();
    let trimmed = cleaned.trim_start_matches('.').trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return placeholder.to_string();
    }
    if trimmed.len() <= MAX_COMPONENT_BYTES {
        return trimmed.to_string();
    }
    let ext = trimmed.rfind('.').map(|i| &trimmed[i..]).filter(|e| e.len() <= 16).unwrap_or("");
    let mut end = MAX_COMPONENT_BYTES - ext.len();
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", &trimmed[..end])
}

/// `<parent-2>/<parent-1>/<file>` of a remote path, each component sanitized.
fn album_relative_path(remote: &str) -> PathBuf {
    let parts: Vec<&str> = remote.split(['\\', '/']).filter(|p| !p.is_empty() && *p != "." && *p != "..").collect();
    let (name, dirs) = parts.split_last().map_or((&"", &[][..]), |(n, d)| (n, d));
    let mut out = PathBuf::new();
    for d in &dirs[dirs.len().saturating_sub(2)..] {
        out.push(sanitize_component(d, "unknown"));
    }
    out.push(sanitize_component(name, "track"));
    out
}

/// Moves `src` to its album-style place under `root` (keeping an existing file), refusing symlinks and escapes.
fn place_download(root: &Path, src: &Path, remote: &str) -> Result<PathBuf> {
    let rel = album_relative_path(remote);
    if !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
        return Err(src_err("refusing unsafe download path"));
    }
    let mut cur = root.to_path_buf();
    let dirs = rel.parent().map(|p| p.components().count()).unwrap_or(0);
    for (i, comp) in rel.components().enumerate() {
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(src_err(format!("{} is a symlink", cur.display()))),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur).map_err(|e| src_err(format!("creating {}: {e}", cur.display())))?;
            }
            Err(e) => return Err(src_err(format!("{}: {e}", cur.display()))),
        }
        cur.push(comp);
        if i == dirs {
            break;
        }
    }
    let dest = cur;
    if !dest.starts_with(root) {
        return Err(src_err("refusing unsafe download path"));
    }
    match std::fs::symlink_metadata(&dest) {
        Ok(m) if m.file_type().is_file() => {
            let _ = std::fs::remove_file(src);
            return Ok(dest);
        }
        Ok(_) => return Err(src_err(format!("{} exists and is not a regular file", dest.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(src_err(format!("{}: {e}", dest.display()))),
    }
    if std::fs::rename(src, &dest).is_err() {
        std::fs::copy(src, &dest).and_then(|_| std::fs::remove_file(src)).map_err(|e| src_err(format!("moving download: {e}")))?;
    }
    Ok(dest)
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

fn track_from_file(resp: &SearchResponse, file: &SearchFile) -> Option<Track> {
    let track = TrackRef { username: resp.username.clone(), filename: file.filename.clone(), size: file.size };
    let (mut artists, title) = core::parse_artist_title(&stem(&file.filename));
    if title.trim().is_empty() {
        return None;
    }
    if artists.is_empty() {
        artists.push(resp.username.clone());
    }
    let duration_ms = file.length.unwrap_or(0).saturating_mul(1000);
    let quality = quality_for(&file.extension.to_lowercase(), file.bit_rate, file.bit_depth, file.sample_rate);
    Some(Track::fresh(title, artists, Rendition::fresh(source_id(), track.to_uri(), duration_ms, quality)))
}
