//! `sources-spotify` — a Spotify source + media provider plugin for medley.
//!
//! Built behind `app`'s off-by-default `spotify` cargo feature; `core` never
//! depends on this crate.
//!
//! * [`SpotifySource`] — `core::Source`: URI recognition + Web API track
//!   search / resolve.
//! * [`SpotifyMediaProvider`] — `core::MediaProvider`: the decrypted Ogg Vorbis, played by `RodioPlayer`.
//! * [`auth::Auth`] — librespot OAuth login + credential/token cache.

pub mod auth;
mod link;
mod plugin;
mod provider;
mod source;
pub mod uri;
mod web_player;
mod webapi;

pub use auth::Auth;
pub use provider::SpotifyMediaProvider;
pub use plugin::SpotifyPlugin;
pub use source::SpotifySource;
pub use uri::{recognizes, SpotifyRef};

/// This crate's `SourceId` — shared by `source`, `webapi` and `provider`
/// instead of each redefining it.
fn source_id() -> core::SourceId {
    core::SourceId::from("spotify")
}

// Spotify streams are Ogg Vorbis/AAC, lossy, bitrate negotiated at playback.
fn track(title: String, artists: Vec<String>, isrc: Option<String>, album: Option<String>, uri: String, duration_ms: u32) -> core::Track {
    let rendition = core::Rendition::fresh(source_id(), uri, duration_ms, core::Quality::Lossy { kbps: None });
    core::Track { isrc, album, ..core::Track::fresh(title, artists, rendition) }
}
