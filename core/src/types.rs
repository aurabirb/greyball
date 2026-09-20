//! Core data model.

use serde::{Deserialize, Serialize};

pub type Uuid = uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct TrackId(pub Uuid);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct PlaylistId(pub Uuid);

impl TrackId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TrackId {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaylistId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PlaylistId {
    fn default() -> Self {
        Self::new()
    }
}

/// A short label identifying a source. Sources return their own via
/// `Source::id()`. Cheap to clone (refcounted); compares and hashes by value.
#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct SourceId(std::sync::Arc<str>);

impl SourceId {
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(std::sync::Arc::from(s.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A short (2-char) label for compact display, e.g. a track list's
    /// source column: `spotify` -> `sp`, `soundcloud` -> `sc`, `local` ->
    /// `lo`, `http` -> `ht`, the `"external"` pseudo-source -> `ex`.
    /// Anything else falls back to its own first two characters.
    pub fn short(&self) -> String {
        match self.as_str() {
            "spotify" => "sp".to_string(),
            "soundcloud" => "sc".to_string(),
            "local" => "lo".to_string(),
            "http" => "ht".to_string(),
            "external" => "ex".to_string(),
            other => other.chars().take(2).collect::<String>().to_lowercase(),
        }
    }
}

impl From<&str> for SourceId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl std::borrow::Borrow<str> for SourceId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl Serialize for SourceId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SourceId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Ok(Self::new(String::deserialize(d)?))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Quality {
    Lossless { bits: Option<u8>, hz: Option<u32> },
    Lossy { kbps: Option<u32> },
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum LinkReason {
    Isrc,
    Fuzzy,
    Manual,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rendition {
    pub source: SourceId,
    /// OPAQUE to core. Never parsed here.
    pub uri: String,
    /// 0 = unknown
    pub duration_ms: u32,
    pub quality: Quality,
    pub link: LinkReason,
    pub added_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    pub artists: Vec<String>,
    /// best known; 0 = unknown
    pub duration_ms: u32,
    pub isrc: Option<String>,
    pub album: Option<String>,
    pub year: Option<u16>,
    /// Generic scan-plugin metadata (bpm, musical key, later genre/mood/...),
    /// key -> display string. `BTreeMap` for deterministic iteration (row
    /// rendering, `:togglescan` output) without a separate sort step.
    /// `default` so a track record written before this field existed still
    /// deserializes (empty map) instead of getting dropped by `Store`.
    #[serde(default)]
    pub attrs: std::collections::BTreeMap<String, String>,
    pub tags: Vec<String>,
    /// invariant: len >= 1
    pub renditions: Vec<Rendition>,
}

impl Rendition {
    pub fn fresh(source: SourceId, uri: String, duration_ms: u32, quality: Quality) -> Self {
        Self { source, uri, duration_ms, quality, link: LinkReason::Manual, added_at: chrono::Utc::now() }
    }
}

impl Track {
    /// A not-yet-ingested track carrying exactly one rendition.
    pub fn fresh(title: String, artists: Vec<String>, rendition: Rendition) -> Self {
        Self {
            id: TrackId::new(),
            title,
            artists,
            duration_ms: rendition.duration_ms,
            isrc: None,
            album: None,
            year: None,
            attrs: Default::default(),
            tags: vec![],
            renditions: vec![rendition],
        }
    }

    /// The single rendition of a source-output track.
    pub fn rendition(&self) -> &Rendition {
        debug_assert_eq!(self.renditions.len(), 1);
        &self.renditions[0]
    }

    pub fn display_artist(&self) -> String {
        if self.artists.is_empty() {
            "Unknown Artist".to_string()
        } else {
            self.artists.join(", ")
        }
    }

    /// "Artist - Title", or just the title with no known artist.
    pub fn display_name(&self) -> String {
        if self.artists.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.display_artist(), self.title)
        }
    }

    pub fn duration_str(&self) -> String {
        let ms = if self.duration_ms > 0 {
            self.duration_ms
        } else {
            self.best_rendition().map(|r| r.duration_ms).unwrap_or(0)
        };
        if ms == 0 {
            return "?:??".to_string();
        }
        let total = ms / 1000;
        format!("{}:{:02}", total / 60, total % 60)
    }

    pub fn source_badges(&self) -> String {
        let mut seen: Vec<&str> = Vec::new();
        for r in &self.renditions {
            if !seen.contains(&r.source.as_str()) {
                seen.push(r.source.as_str());
            }
        }
        format!("[{}]", seen.join("+"))
    }

    /// Like [`Track::source_badges`] but abbreviated (`SourceId::short`) and
    /// without brackets, for a compact source column.
    pub fn source_badges_short(&self) -> String {
        let mut seen: Vec<String> = Vec::new();
        for r in &self.renditions {
            let short = r.source.short();
            if !seen.contains(&short) {
                seen.push(short);
            }
        }
        seen.join("+")
    }

    pub fn best_rendition(&self) -> Option<Rendition> {
        match crate::resolver::Resolver::resolve_track(self, &crate::resolver::Target::Playback) {
            crate::resolver::Resolution::Ready(r) => Some(r),
            crate::resolver::Resolution::Gap { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    pub notes: String,
    pub items: Vec<TrackId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ItemKind {
    Track,
    Album,
    Playlist,
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    pub text: String,
    /// Milestone 1: always [Track]
    pub kinds: Vec<ItemKind>,
    /// per-source soft cap, default 100
    pub limit: usize,
}

impl SearchQuery {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kinds: vec![ItemKind::Track],
            limit: 100,
        }
    }

    pub fn all_kinds(text: impl Into<String>) -> Self {
        Self {
            kinds: vec![ItemKind::Track, ItemKind::Album, ItemKind::Playlist],
            ..Self::text(text)
        }
    }
}

/// Split "Artist - Title" / "Artist – Title" / "Artist_-_Title". If no separator,
/// returns (vec![], whole_string). Trims, collapses whitespace, strips a leading
/// track number like "03 " or "03. " or "03 - ".
pub fn parse_artist_title(name: &str) -> (Vec<String>, String) {
    // collapse whitespace
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = collapsed.trim().to_string();

    // strip a leading track number: "03", "03.", "03 -" (the "03 - " dash is
    // handled by re-checking after this strip).
    s = strip_leading_track_number(&s);

    // separator candidates, in order
    for sep in [" - ", " – ", "_-_", " — "] {
        if let Some(idx) = s.find(sep) {
            let artist = s[..idx].trim();
            let title = s[idx + sep.len()..].trim();
            if !artist.is_empty() && !title.is_empty() {
                let artists = artist
                    .split(&[',', '&'][..])
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty())
                    .collect();
                return (artists, title.to_string());
            }
        }
    }

    (vec![], s)
}

fn strip_leading_track_number(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i > 3 {
        return s.to_string();
    }
    let rest = &s[i..];
    // "03. Foo", "03 - Foo", "03 Foo", "03_-_Foo"
    for pat in [". ", " - ", " ", "_-_", "-"] {
        if let Some(stripped) = rest.strip_prefix(pat) {
            let stripped = stripped.trim_start();
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    s.to_string()
}

