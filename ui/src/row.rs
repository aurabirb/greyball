//! `RowItem` — how a list row is rendered.

use core::Track;

/// Anything that can be shown as a row in a list screen, split into the
/// track list's columns: tags, title/artist, source, duration.
pub trait RowItem: Send + Sync {
    /// Tags column: the track's values for `Config::visible_track_attrs`, in
    /// configured order, space-joined — bpm ("128") is the canonical MVP
    /// entry, but any other attr the user opts into (`key`, later
    /// `genre`/`mood`) shows here too, not just bpm.
    fn tags(&self, visible: &[String]) -> String;
    /// Main column: "artist - title", plus the album if known.
    fn main(&self) -> String;
    /// Abbreviated source column, e.g. "sp", "sc", "lo" (see
    /// `SourceId::short`); "+"-joined for a track with renditions on more
    /// than one source. `cached` (`Session::is_track_cached`) prefixes a
    /// "*" onto that — "already on disk, plays instantly" plus which
    /// source(s) it came from.
    fn source(&self, cached: bool) -> String;
    /// Duration column, e.g. "3:45".
    fn duration(&self) -> String;
}

impl RowItem for Track {
    fn tags(&self, visible: &[String]) -> String {
        visible
            .iter()
            .filter_map(|k| self.attrs.get(k))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn main(&self) -> String {
        let mut m = format!("{} - {}", self.display_artist(), self.title);
        if let Some(album) = &self.album {
            m = format!("{m} · {album}");
        }
        m
    }

    fn source(&self, cached: bool) -> String {
        if cached {
            format!("*{}", self.source_badges_short())
        } else {
            self.source_badges_short()
        }
    }

    fn duration(&self) -> String {
        self.duration_str()
    }
}
