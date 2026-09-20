//! Spotify URI / URL recognition and parsing.
//!
//! One table-driven parser. `core` never sees any of this — it hands the
//! plugin an opaque `uri` string.

use url::{Host, Url};

/// The kind of thing a Spotify URI points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Track,
    Album,
    Artist,
    Playlist,
    Show,
    Episode,
}

impl ItemKind {
    fn from_seg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "track" => Self::Track,
            "album" => Self::Album,
            "artist" => Self::Artist,
            "playlist" => Self::Playlist,
            "show" => Self::Show,
            "episode" => Self::Episode,
            _ => return None,
        })
    }

    pub fn seg(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Show => "show",
            Self::Episode => "episode",
        }
    }
}

/// A parsed Spotify reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpotifyRef {
    pub kind: ItemKind,
    pub id: String,
}

impl SpotifyRef {
    /// Canonical `spotify:<kind>:<id>` form.
    pub fn uri(&self) -> String {
        format!("spotify:{}:{}", self.kind.seg(), self.id)
    }

    /// Parse either a `spotify:…` URI or an `open.spotify.com/…` URL.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.starts_with("spotify:") {
            Self::parse_uri(s)
        } else {
            Self::parse_url(s)
        }
    }

    fn parse_uri(s: &str) -> Option<Self> {
        // spotify:track:ID  /  spotify:user:NAME:playlist:ID
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            ["spotify", kind, id] => Some(Self {
                kind: ItemKind::from_seg(kind)?,
                id: (*id).to_string(),
            }),
            ["spotify", "user", _user, "playlist", id] => Some(Self {
                kind: ItemKind::Playlist,
                id: (*id).to_string(),
            }),
            _ => None,
        }
    }

    fn parse_url(s: &str) -> Option<Self> {
        let url = Url::parse(s).ok()?;
        if url.host() != Some(Host::Domain("open.spotify.com")) {
            return None;
        }
        let mut segs = url.path_segments()?;
        let mut entity = segs.next()?;
        // Locale prefix, e.g. /intl-pt/track/…
        if entity.to_ascii_lowercase().starts_with("intl-") {
            entity = segs.next()?;
        }
        if entity.eq_ignore_ascii_case("user") {
            let _user = segs.next()?;
            let next = segs.next()?;
            if !next.eq_ignore_ascii_case("playlist") {
                return None;
            }
            return Some(Self {
                kind: ItemKind::Playlist,
                id: segs.next()?.to_string(),
            });
        }
        let kind = ItemKind::from_seg(entity)?;
        let id = segs.next()?;
        if id.is_empty() {
            return None;
        }
        Some(Self {
            kind,
            id: id.to_string(),
        })
    }
}

/// Would `SpotifySource` claim this pasted string? (`Source::recognizes`)
pub fn recognizes(uri: &str) -> bool {
    SpotifyRef::parse(uri).is_some()
}

