//! `HttpDirSource` — the first medley source plugin.
//!
//! Crawls plain HTTP directory listings (`python -m http.server`, copyparty,
//! nginx autoindex, …). No HTML parser: a regex pulls `href` values out of the
//! page and every link is resolved against the directory URL.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Duration;

use core::{
    BrowseNode, BrowsePage, Error, Media, MediaProvider, Quality, Rendition, Result, SearchHit,
    SearchQuery, Source, SourceId,
};
use regex::Regex;
use url::Url;

/// This source's id. `SourceId` is refcounted and not `const`-constructible, so
/// this is a cheap constructor (one small alloc) rather than a constant.
fn source_id() -> SourceId {
    SourceId::from("http")
}

fn href_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)href\s*=\s*["']([^"'#?]+)["']"#).unwrap())
}

pub struct HttpDirSource {
    roots: Vec<Url>,
    depth: usize,
    client: reqwest::blocking::Client,
}

impl HttpDirSource {
    /// Build from raw root strings + a recursion depth. Invalid roots are logged
    /// and dropped.
    pub fn new(roots: &[String], depth: usize) -> Self {
        let roots = roots
            .iter()
            .filter_map(|r| match Url::parse(r) {
                Ok(u) => Some(u),
                Err(e) => {
                    log::warn!("http source: ignoring invalid root {r:?}: {e}");
                    None
                }
            })
            .collect();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            roots,
            depth,
            client,
        }
    }

    /// Convenience wired from `core::Config`.
    pub fn from_cfg(cfg: &core::Config) -> Self {
        Self::new(&cfg.http.roots, cfg.http.recurse_depth)
    }

    /// GET a directory URL and split its links into `(subdirs, audio hits)`.
    /// `rel_path` is the path of the dir relative to its root, used for the
    /// query-token substring filter.
    fn fetch_dir(&self, dir: &Url) -> std::result::Result<String, String> {
        let resp = self
            .client
            .get(dir.clone())
            .send()
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        resp.text().map_err(|e| e.to_string())
    }
}

/// The last non-empty `/`-separated segment of a URL path (still %-encoded).
fn last_segment(u: &Url) -> &str {
    u.path().rsplit('/').find(|s| !s.is_empty()).unwrap_or("")
}

/// Percent-decoded last path segment with its extension removed.
fn stem_of(u: &Url) -> String {
    let decoded = percent_decode(last_segment(u));
    match decoded.rsplit_once('.') {
        Some((name, _ext)) => name.to_string(),
        None => decoded,
    }
}

/// Audio-extension + lossy/lossless heuristics now live in `core::audio` so the
/// playlist importer and this source agree. We pass the last path segment
/// (still %-encoded; extensions are ASCII so that is fine).
fn is_audio(u: &Url) -> bool {
    core::audio_ext(last_segment(u))
}

fn quality_for(u: &Url) -> Quality {
    core::audio::audio_quality(last_segment(u))
}

/// Minimal percent-decoding (enough for filenames; not a full URL decoder).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build a `SearchHit` from an absolute audio URL (no network).
fn hit_from_url(u: &Url) -> SearchHit {
    let stem = stem_of(u);
    let (artists, title) = core::parse_artist_title(&stem);
    SearchHit {
        source: source_id(),
        uri: u.to_string(),
        title,
        artists,
        duration_ms: 0,
        isrc: None,
        album: None,
        quality: quality_for(u),
    }
}

/// Extract every usable link from a listing body, resolved against `base`.
/// Skips `..`, anchors, external hosts, query-only links.
fn links_in(base: &Url, body: &str) -> Vec<Url> {
    let mut out = Vec::new();
    for cap in href_re().captures_iter(body) {
        let raw = &cap[1];
        if raw.starts_with('?') || raw == "." || raw == "./" {
            continue;
        }
        let Ok(abs) = base.join(raw) else { continue };
        if abs.host_str() != base.host_str() || abs.scheme() != base.scheme() {
            continue;
        }
        // reject parent / same dir
        if abs.path() == base.path() {
            continue;
        }
        if !abs.path().starts_with(trim_to_dir(base.path()).as_str()) {
            continue; // '..' or sibling — outside this directory
        }
        out.push(abs);
    }
    out
}

/// The directory portion of a path (everything up to and including the last `/`).
fn trim_to_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..=i].to_string(),
        None => "/".to_string(),
    }
}

impl Source for HttpDirSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn recognizes(&self, uri: &str) -> bool {
        let Ok(u) = Url::parse(uri) else { return false };
        if !matches!(u.scheme(), "http" | "https") {
            return false;
        }
        if is_audio(&u) {
            return true;
        }
        self.roots.iter().any(|root| {
            u.host_str() == root.host_str()
                && u.path().starts_with(trim_to_dir(root.path()).as_str())
        })
    }

    fn search(&self, q: &SearchQuery, sink: &mut dyn FnMut(SearchHit)) -> Result<()> {
        if self.roots.is_empty() {
            return Err(Error::Source {
                src: source_id(),
                message: "no roots configured".to_string(),
            });
        }
        let tokens: Vec<String> = q.text.to_lowercase().split_whitespace().map(String::from).collect();
        let limit = if q.limit == 0 { usize::MAX } else { q.limit };

        // queue entries: (url, depth, root_prefix)
        let mut queue: VecDeque<(Url, usize, String)> = VecDeque::new();
        for root in &self.roots {
            queue.push_back((root.clone(), 0, root.to_string()));
        }
        let roots_total = self.roots.len();
        let mut roots_failed = 0usize;
        let mut emitted = 0usize;

        while let Some((dir, depth, root_prefix)) = queue.pop_front() {
            if emitted >= limit {
                break;
            }
            let body = match self.fetch_dir(&dir) {
                Ok(b) => b,
                Err(e) => {
                    if depth == 0 {
                        log::warn!("http source: root {dir} unreachable: {e}");
                        roots_failed += 1;
                    } else {
                        log::info!("http source: skipping dead subfolder {dir}: {e}");
                    }
                    continue;
                }
            };
            for link in links_in(&dir, &body) {
                let is_dir = link.path().ends_with('/');
                if is_dir {
                    if depth < self.depth && link.as_str().starts_with(root_prefix.as_str()) {
                        queue.push_back((link, depth + 1, root_prefix.clone()));
                    }
                    continue;
                }
                if !is_audio(&link) {
                    continue;
                }
                let rel = link.as_str().strip_prefix(root_prefix.as_str()).unwrap_or(link.path());
                let hay = percent_decode(rel).to_lowercase();
                if !tokens.iter().all(|t| hay.contains(t.as_str())) {
                    continue;
                }
                sink(hit_from_url(&link));
                emitted += 1;
                if emitted >= limit {
                    break;
                }
            }
        }

        if roots_total > 0 && roots_failed == roots_total {
            return Err(Error::Source {
                src: source_id(),
                message: format!("all {roots_total} root(s) unreachable"),
            });
        }
        Ok(())
    }

    fn resolve(&self, uri: &str) -> Result<SearchHit> {
        let u = Url::parse(uri).map_err(|e| Error::Source {
            src: source_id(),
            message: format!("bad url {uri:?}: {e}"),
        })?;
        Ok(hit_from_url(&u))
    }

    fn browse(&self, node: &BrowseNode, _want: usize) -> Result<BrowsePage> {
        match node {
            BrowseNode::Root => {
                let folders = self
                    .roots
                    .iter()
                    .map(|r| (r.to_string(), BrowseNode::Path(r.to_string())))
                    .collect();
                Ok(BrowsePage {
                    title: "http roots".to_string(),
                    tracks: vec![],
                    folders,
                    partial: false,
                    errored: false,
                })
            }
            BrowseNode::Path(p) => {
                let dir = Url::parse(p).map_err(|e| Error::Source {
                    src: source_id(),
                    message: format!("bad url {p:?}: {e}"),
                })?;
                let body = self.fetch_dir(&dir).map_err(|e| Error::Source {
                    src: source_id(),
                    message: format!("{dir}: {e}"),
                })?;
                let mut tracks = Vec::new();
                let mut folders = Vec::new();
                for link in links_in(&dir, &body) {
                    if link.path().ends_with('/') {
                        let name = percent_decode(last_segment(&link));
                        folders.push((name, BrowseNode::Path(link.to_string())));
                    } else if is_audio(&link) {
                        tracks.push(hit_from_url(&link));
                    }
                }
                Ok(BrowsePage {
                    title: dir.to_string(),
                    tracks,
                    folders,
                    partial: false,
                    errored: false,
                })
            }
        }
    }
}

impl MediaProvider for HttpDirSource {
    fn id(&self) -> SourceId {
        source_id()
    }

    fn open(&self, r: &Rendition) -> Result<Media> {
        // MVP: hand the raw URL to the player, which downloads it.
        Ok(Media::Url(r.uri.clone()))
    }
}
