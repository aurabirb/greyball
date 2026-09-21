//! Logical track -> concrete rendition.

use std::path::Path;

use crate::types::{Playlist, Quality, Rendition, SourceId, Track, TrackId};

pub enum Target {
    Playback,
    ExportM3u,
    ExportTo(SourceId),
}

#[derive(Debug)]
pub enum Resolution {
    Ready(Rendition),
    Gap { reason: String },
}

/// Whether `source` names the local pseudo-source (`"local"`) — the one
/// medley-defined source whose renditions live on disk rather than behind a
/// `MediaProvider`.
pub fn is_local_source(source: &SourceId) -> bool {
    source.as_str() == "local"
}

/// Strip a `file://` prefix, if present. A local rendition's `uri` is just
/// the bare filesystem path going forward, but on-disk playlists written
/// before this change (or hand-written ones) may still carry the old
/// `file://`-prefixed form — tolerate it rather than fail to import.
pub fn local_path_from_uri(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

pub struct Resolver;

impl Resolver {
    pub fn resolve_track(t: &Track, target: &Target) -> Resolution {
        match target {
            Target::ExportTo(s) => match t.renditions.iter().find(|r| r.source == *s) {
                Some(r) => Resolution::Ready(r.clone()),
                None => Resolution::Gap {
                    reason: format!("no rendition on source {s}"),
                },
            },
            Target::ExportM3u => {
                // Best line for a *foreign* player.
                // 1. a live local file.
                if let Some(r) = t
                    .renditions
                    .iter()
                    .filter(|r| is_local_source(&r.source) && Self::is_live(r))
                    .max_by_key(|r| r.added_at)
                {
                    return Resolution::Ready(r.clone());
                }
                // 2. highest-rank remote rendition.
                if let Some(r) = best_by_rank(
                    t.renditions
                        .iter()
                        .filter(|r| !is_local_source(&r.source) && Self::is_live(r)),
                ) {
                    return Resolution::Ready(r.clone());
                }
                // 3. nothing.
                Resolution::Gap {
                    reason: format!("nothing exportable for \"{}\"", t.title),
                }
            }
            Target::Playback => {
                let best = best_by_rank(t.renditions.iter().filter(|r| Self::is_live(r)));
                match best {
                    Some(r) => Resolution::Ready(r.clone()),
                    None => Resolution::Gap {
                        reason: format!("nothing playable for \"{}\"", t.title),
                    },
                }
            }
        }
    }

    /// Like `resolve_track(t, &Target::Playback)`, skipping any rendition
    /// whose source is in `exclude`.
    pub fn resolve_playback_excluding(t: &Track, exclude: &[SourceId]) -> Resolution {
        let best =
            best_by_rank(t.renditions.iter().filter(|r| Self::is_live(r) && !exclude.contains(&r.source)));
        match best {
            Some(r) => Resolution::Ready(r.clone()),
            None => Resolution::Gap {
                reason: format!("no other rendition playable for \"{}\"", t.title),
            },
        }
    }

    /// Is this rendition actually playable *right now*? A local file that has
    /// been deleted is not. Everything remote is assumed live (no network here).
    /// `path.exists()` is a cheap `stat`. (`// MVP:`)
    pub fn is_live(r: &Rendition) -> bool {
        if is_local_source(&r.source) {
            Path::new(local_path_from_uri(&r.uri)).exists()
        } else {
            true
        }
    }

    pub fn resolve_playlist(
        p: &Playlist,
        tracks: &[Track],
        target: &Target,
    ) -> Vec<(TrackId, Resolution)> {
        p.items
            .iter()
            .map(|id| {
                let res = match tracks.iter().find(|t| t.id == *id) {
                    Some(t) => Self::resolve_track(t, target),
                    None => Resolution::Gap {
                        reason: "track not found".to_string(),
                    },
                };
                (*id, res)
            })
            .collect()
    }
}

/// The rendition with the highest `playback_rank`, ties broken by newest `added_at`.
fn best_by_rank<'a>(renditions: impl Iterator<Item = &'a Rendition>) -> Option<&'a Rendition> {
    renditions.max_by(|a, b| playback_rank(a).cmp(&playback_rank(b)).then(a.added_at.cmp(&b.added_at)))
}

/// Higher is better: local > remote Lossless > Lossy > Unknown > Preview.
fn playback_rank(r: &Rendition) -> u8 {
    if is_local_source(&r.source) {
        4
    } else {
        match r.quality {
            Quality::Lossless { .. } => 3,
            Quality::Lossy { .. } => 2,
            Quality::Unknown => 1,
            Quality::Preview => 0,
        }
    }
}

