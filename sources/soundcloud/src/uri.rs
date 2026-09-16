//! SoundCloud URI / URL recognition.
//!
//! Two forms are accepted:
//! - the canonical `soundcloud:track:<id>` medley uses internally, and
//! - a public `https://soundcloud.com/<user>/<slug>` permalink (or an
//!   `api.soundcloud.com` / `api-v2.soundcloud.com` resource URL).
//!
//! `core` treats the whole string as opaque; only this crate parses it.

use url::{Host, Url};

/// A resolved reference to a SoundCloud track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackRef {
    /// `soundcloud:track:<id>` — numeric id known.
    Id(u64),
    /// A `soundcloud.com/...` permalink that still needs a `/resolve` call.
    Permalink(String),
}

impl TrackRef {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("soundcloud:track:") {
            return rest.parse::<u64>().ok().map(Self::Id);
        }
        let url = Url::parse(s).ok()?;
        match url.host() {
            // https://api-v2.soundcloud.com/tracks/12345  (also api.soundcloud.com)
            Some(Host::Domain(h))
                if h == "api-v2.soundcloud.com" || h == "api.soundcloud.com" =>
            {
                let mut segs = url.path_segments()?;
                if segs.next()? != "tracks" {
                    return None;
                }
                segs.next()?.parse::<u64>().ok().map(Self::Id)
            }
            // https://soundcloud.com/<user>/<slug>  (not /sets/, not a bare user)
            Some(Host::Domain(h)) if h == "soundcloud.com" || h == "www.soundcloud.com" => {
                let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
                if segs.len() != 2 || segs[1] == "sets" {
                    return None;
                }
                // permalink resolve needs the canonical, query-free URL
                Some(Self::Permalink(format!(
                    "https://soundcloud.com/{}/{}",
                    segs[0], segs[1]
                )))
            }
            _ => None,
        }
    }
}

/// Would `SoundcloudSource` claim this pasted string? (`Source::recognizes`)
pub fn recognizes(uri: &str) -> bool {
    TrackRef::parse(uri).is_some()
}

