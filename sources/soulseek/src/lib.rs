//! `SoulseekSource` — a medley source plugin talking to a local `slskd`
//! (<https://github.com/slskd/slskd>), the commonly-run headless Soulseek
//! daemon with a documented REST API. There's no single project literally
//! named `soulseekd` with a documented protocol; `slskd` is what the TODO's
//! "look for a local soulseekd" means in practice.
//!
//! Search hits `POST /api/v0/searches` and converts its file results to
//! `SearchHit`s, hard-capped at [`client::MAX_RESULTS`] — Soulseek searches
//! can otherwise return an unbounded flood as slow peers keep trickling in.
//! Playback has no streamable URL: `open` enqueues a download through
//! slskd's own transfer queue (`POST .../transfers/downloads/batches`),
//! blocks polling until it lands on disk under the configured slskd data
//! directory, then hands back that file directly as `Media::Path`.
//!
//! No local `slskd` reachable: `Plugin::probe` reports a `Warn` (per the
//! TODO) with setup guidance instead of failing outright, and the source
//! can be turned off entirely via `[soulseek] enabled = false`.

mod client;
mod docker;
mod persist;
mod plugin;
mod source;
mod uri;
mod yaml_config;

pub use client::{SlskdClient, SlskdConfig};
pub use plugin::SoulseekPlugin;
pub use source::SoulseekSource;
