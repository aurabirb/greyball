//! Enriched M3U playlist grammar.
//!
//! Pure format module: it owns the wire grammar and nothing else. No I/O, no
//! network, no `Store` / `Catalog`. `Session` (`app.rs`) builds an [`M3uDoc`]
//! from the resolver, calls [`write_m3u`], and does the file write; on import it
//! calls [`parse_m3u`] and folds the result into the catalog.

use chrono::{DateTime, Utc};

use crate::traits::{Error, Result};
use crate::types::{Quality, Rendition, Uuid};

/// A whole parsed / to-be-written playlist file.
#[derive(Clone, Debug, PartialEq)]
pub struct M3uDoc {
    pub playlist: PlaylistMeta,
    pub entries: Vec<M3uEntry>,
}

/// Header block (`#PLAYLIST`, `#MEDLEY-PLAYLIST-ID`, `#MEDLEY-NOTES`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaylistMeta {
    /// From `#MEDLEY-PLAYLIST-ID`. `None` → the importer mints a fresh id.
    pub id: Option<Uuid>,
    /// From `#PLAYLIST`. Empty → the importer falls back to the file stem.
    pub name: String,
    /// From `#MEDLEY-NOTES` (percent-decoded).
    pub notes: String,
}

/// One playlist entry: the standard `#EXTINF` + primary URI line, plus medley's
/// `#MEDLEY-*` sidecar directives.
#[derive(Clone, Debug, PartialEq)]
pub struct M3uEntry {
    /// `(round(duration_ms/1000) or -1, "Artist - Title")`.
    pub extinf: Option<(i64, String)>,
    /// Advisory `#MEDLEY-TRACK-ID`; import assigns its own id.
    pub track_id: Option<Uuid>,
    /// From `#MEDLEY-PLAYED-AT` — when this play started (play-history
    /// entries only; absent on a regular exported playlist).
    pub played_at: Option<DateTime<Utc>>,
    pub meta: SoftMeta,
    pub renditions: Vec<ParsedRendition>,
    /// The one non-`#` line — what a foreign player plays.
    pub primary_uri: String,
}

/// Soft metadata carried on `#MEDLEY-META`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SoftMeta {
    pub isrc: Option<String>,
    pub album: Option<String>,
    pub year: Option<u16>,
    pub bpm: Option<f32>,
    pub key: Option<String>,
    pub tags: Vec<String>,
}

impl SoftMeta {
    fn is_empty(&self) -> bool {
        self.isrc.is_none()
            && self.album.is_none()
            && self.year.is_none()
            && self.bpm.is_none()
            && self.key.is_none()
            && self.tags.is_empty()
    }
}

/// One `#MEDLEY-RENDITION` line, fields already percent-decoded.
///
/// Neither `Rendition::link` (`LinkReason` — how this candidate got matched
/// to the track: ISRC, fuzzy, or manual) nor an availability/locality field
/// is part of this: `link` was write-only on the wire (parsed back but never
/// read by the importer), and locality is fully recoverable from `source` +
/// `uri` alone (`source == "local"` and `uri` is the bare filesystem path) —
/// see `resolver::is_local_source`/`local_path_from_uri`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedRendition {
    pub source: String,
    pub quality: Quality,
    pub added_at: DateTime<Utc>,
    pub uri: String,
}

impl ParsedRendition {
    /// Serialise a live [`Rendition`] for `#MEDLEY-RENDITION`.
    pub fn from_rendition(r: &Rendition) -> Self {
        Self {
            source: r.source.as_str().to_string(),
            quality: r.quality.clone(),
            added_at: r.added_at,
            uri: r.uri.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Render an [`M3uDoc`] to text (Unix newlines, `.m3u8`-style).
pub fn write_m3u(doc: &M3uDoc) -> String {
    let mut out = write_header(&doc.playlist);
    for e in &doc.entries {
        out.push_str(&write_entry(e));
    }
    out
}

/// The `#EXTM3U`/`#PLAYLIST`/... header block, written once at the top of a
/// file. Split out from [`write_m3u`] so an append-only writer (play
/// history) can write this once and then [`write_entry`] per record.
pub fn write_header(playlist: &PlaylistMeta) -> String {
    let mut out = String::from("#EXTM3U\n");
    out.push_str(&format!("#PLAYLIST:{}\n", playlist.name));
    if let Some(id) = playlist.id {
        out.push_str(&format!("#MEDLEY-PLAYLIST-ID:{id}\n"));
    }
    if !playlist.notes.is_empty() {
        out.push_str(&format!("#MEDLEY-NOTES:{}\n", pct_encode(&playlist.notes)));
    }
    out
}

/// Render one entry: a leading blank line, `#EXTINF` + `#MEDLEY-*`
/// directives, then the primary URI. Self-contained so it can be appended to
/// an existing file one record at a time.
pub fn write_entry(e: &M3uEntry) -> String {
    let mut out = String::from("\n");
    if let Some((secs, title)) = &e.extinf {
        out.push_str(&format!("#EXTINF:{secs},{title}\n"));
    }
    if let Some(tid) = e.track_id {
        out.push_str(&format!("#MEDLEY-TRACK-ID:{tid}\n"));
    }
    if let Some(ts) = e.played_at {
        out.push_str(&format!(
            "#MEDLEY-PLAYED-AT:{}\n",
            ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));
    }
    if !e.meta.is_empty() {
        out.push_str(&format!("#MEDLEY-META:{}\n", write_meta(&e.meta)));
    }
    for r in &e.renditions {
        out.push_str(&format!("#MEDLEY-RENDITION:{}\n", write_rendition(r)));
    }
    out.push_str(&e.primary_uri);
    out.push('\n');
    out
}

fn write_meta(m: &SoftMeta) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = &m.isrc {
        parts.push(format!("isrc={}", pct_encode(v)));
    }
    if let Some(v) = &m.album {
        parts.push(format!("album={}", pct_encode(v)));
    }
    if let Some(v) = m.year {
        parts.push(format!("year={v}"));
    }
    if let Some(v) = m.bpm {
        parts.push(format!("bpm={v}"));
    }
    if let Some(v) = &m.key {
        parts.push(format!("key={}", pct_encode(v)));
    }
    if !m.tags.is_empty() {
        parts.push(format!("tags={}", pct_encode(&m.tags.join(","))));
    }
    parts.join(";")
}

fn write_rendition(r: &ParsedRendition) -> String {
    let fields = [
        pct_encode(&r.source),
        write_quality(&r.quality),
        r.added_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        pct_encode(&r.uri),
    ];
    fields.join("|")
}

fn write_quality(q: &Quality) -> String {
    match q {
        Quality::Unknown => "unknown".to_string(),
        Quality::Lossy { kbps: None } => "lossy".to_string(),
        Quality::Lossy { kbps: Some(k) } => format!("lossy:{k}"),
        Quality::Lossless {
            bits: None,
            hz: None,
        } => "lossless".to_string(),
        Quality::Lossless { bits, hz } => format!(
            "lossless:{}:{}",
            bits.map(|b| b.to_string()).unwrap_or_default(),
            hz.map(|h| h.to_string()).unwrap_or_default(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

/// Parse enriched-M3U text. Tolerates `\r\n`; unknown `#` lines are ignored.
pub fn parse_m3u(text: &str) -> Result<M3uDoc> {
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));

    // "#EXTM3U" must be the first non-blank line.
    let first = lines
        .by_ref()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .unwrap_or_default();
    if first != "#EXTM3U" {
        return Err(Error::Other("not an m3u".to_string()));
    }

    let mut meta = PlaylistMeta::default();
    let mut entries: Vec<M3uEntry> = Vec::new();
    let mut cur: Option<M3uEntry> = None;

    for raw in lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            handle_directive(rest, &mut meta, &mut cur);
        } else {
            // The one non-comment line: closes the entry.
            let mut entry = cur.take().unwrap_or_else(new_entry);
            entry.primary_uri = line.to_string();
            entries.push(entry);
        }
    }
    // A trailing entry with directives but no primary line is dropped.

    Ok(M3uDoc {
        playlist: meta,
        entries,
    })
}

fn new_entry() -> M3uEntry {
    M3uEntry {
        extinf: None,
        track_id: None,
        played_at: None,
        meta: SoftMeta::default(),
        renditions: Vec::new(),
        primary_uri: String::new(),
    }
}

fn ensure(cur: &mut Option<M3uEntry>) -> &mut M3uEntry {
    cur.get_or_insert_with(new_entry)
}

fn handle_directive(rest: &str, meta: &mut PlaylistMeta, cur: &mut Option<M3uEntry>) {
    if let Some(v) = rest.strip_prefix("PLAYLIST:") {
        meta.name = v.to_string();
    } else if let Some(v) = rest.strip_prefix("MEDLEY-PLAYLIST-ID:") {
        meta.id = Uuid::parse_str(v.trim()).ok();
    } else if let Some(v) = rest.strip_prefix("MEDLEY-NOTES:") {
        meta.notes = pct_decode(v);
    } else if let Some(v) = rest.strip_prefix("EXTINF:") {
        let (secs, title) = match v.split_once(',') {
            Some((s, t)) => (s.trim().parse::<i64>().unwrap_or(-1), t.to_string()),
            None => (v.trim().parse::<i64>().unwrap_or(-1), String::new()),
        };
        ensure(cur).extinf = Some((secs, title));
    } else if let Some(v) = rest.strip_prefix("MEDLEY-TRACK-ID:") {
        ensure(cur).track_id = Uuid::parse_str(v.trim()).ok();
    } else if let Some(v) = rest.strip_prefix("MEDLEY-PLAYED-AT:") {
        ensure(cur).played_at = DateTime::parse_from_rfc3339(v.trim())
            .ok()
            .map(|d| d.with_timezone(&Utc));
    } else if let Some(v) = rest.strip_prefix("MEDLEY-META:") {
        ensure(cur).meta = parse_meta(v);
    } else if let Some(v) = rest.strip_prefix("MEDLEY-RENDITION:")
        && let Some(r) = parse_rendition(v)
    {
        ensure(cur).renditions.push(r);
    }
    // "EXTM3U" and anything unrecognised: ignored (forward-compat).
}

fn parse_meta(s: &str) -> SoftMeta {
    let mut m = SoftMeta::default();
    for kv in s.split(';') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        let v = pct_decode(v);
        match k.trim() {
            "isrc" => m.isrc = Some(v),
            "album" => m.album = Some(v),
            "year" => m.year = v.parse().ok(),
            "bpm" => m.bpm = v.parse().ok(),
            "key" => m.key = Some(v),
            "tags" => {
                m.tags = v
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect()
            }
            _ => {}
        }
    }
    m
}

fn parse_rendition(s: &str) -> Option<ParsedRendition> {
    let f: Vec<&str> = s.splitn(4, '|').collect();
    if f.len() < 4 {
        return None;
    }
    let added_at = DateTime::parse_from_rfc3339(f[2].trim())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    Some(ParsedRendition {
        source: pct_decode(f[0]),
        quality: parse_quality(f[1].trim()),
        added_at,
        uri: pct_decode(f[3]),
    })
}

fn parse_quality(s: &str) -> Quality {
    if s == "unknown" || s.is_empty() {
        return Quality::Unknown;
    }
    if let Some(rest) = s.strip_prefix("lossless") {
        let rest = rest.strip_prefix(':').unwrap_or(rest);
        if rest.is_empty() {
            return Quality::Lossless {
                bits: None,
                hz: None,
            };
        }
        let mut it = rest.split(':');
        let bits = it.next().and_then(|x| x.parse::<u8>().ok());
        let hz = it.next().and_then(|x| x.parse::<u32>().ok());
        return Quality::Lossless { bits, hz };
    }
    if let Some(rest) = s.strip_prefix("lossy") {
        let rest = rest.strip_prefix(':').unwrap_or(rest);
        let kbps = rest.parse::<u32>().ok();
        return Quality::Lossy { kbps };
    }
    Quality::Unknown
}

// ---------------------------------------------------------------------------
// Percent-encoding
// ---------------------------------------------------------------------------

/// Escape `%`, `|`, `;`, `=`, `\n`, `\r`, and a leading `#` as `%XX` (upper hex).
fn pct_encode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    for (i, &c) in bytes.iter().enumerate() {
        if matches!(c, b'%' | b'|' | b';' | b'=' | b'\n' | b'\r') || (c == b'#' && i == 0) {
            out.extend_from_slice(format!("%{c:02X}").as_bytes());
        } else {
            out.push(c);
        }
    }
    // Only ASCII bytes were ever replaced, so the result is still valid UTF-8.
    String::from_utf8(out).expect("ascii-only escaping keeps utf8 valid")
}

/// Reverse [`pct_encode`]. Tolerates stray `%` (kept literal).
fn pct_decode(s: &str) -> String {
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
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

