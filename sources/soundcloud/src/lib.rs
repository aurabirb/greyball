//! `SoundcloudSource` — a medley source plugin for SoundCloud (MVP).
//!
//! Talks to the undocumented `api-v2.soundcloud.com` endpoints the web player
//! uses. It needs a `client_id`: taken from `[soundcloud] client_id` in the
//! config if set, otherwise scraped once from the public web player bundle
//! (same trick the browser does). `core` sees none of this — it hands the
//! plugin an opaque `uri` string and gets `SearchHit`s / a `Media::Url` back.
//!
//! MVP scope: track search, URL/URI resolve, and progressive-stream playback
//! (the player downloads the resolved CDN URL, as with the http source).
//! Playlists, users, HLS-only tracks, and auth'd (go+) content are out.

mod auth;
mod client;
mod plugin;
mod uri;

pub use client::SoundcloudSource;
pub use plugin::SoundcloudPlugin;
pub use uri::recognizes;
