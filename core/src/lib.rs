//! `core` — the generic, source-agnostic engine for medley.

pub mod app;
pub mod audio;
pub mod audio_decode;
pub mod catalog;
pub mod config;
pub mod event;
pub mod http_fetch;
pub mod logbuf;
pub mod matcher;
pub mod media_cache;
pub mod paged;
pub mod playlist_m3u;
pub mod plugin;
pub mod queue;
pub mod resolver;
pub mod scan;
pub mod search;
pub mod store;
pub mod traits;
pub mod types;
mod view_cache;

pub use app::{BuiltinAction, Command, Dispatch, HotkeyTarget, Session, cache_rendition};
pub use catalog::Catalog;
pub use config::{
    Axis, BpmScanConfig, Config, HttpConfig, PaneLayoutConfig, PaneMode, ScanConfig, Side,
    SoulseekConfig, SoundcloudConfig, SpotifyConfig,
};
pub use audio_decode::decode_and_cache;
pub use event::{Bus, CoreEvent, PlayerEvent};
pub use http_fetch::{fetch_url_bytes, fetch_url_to};
pub use logbuf::LogBuf;
pub use audio::audio_ext;
pub use matcher::Matcher;
pub use media_cache::MediaCache;
pub use paged::{PagedList, RemotePage};
pub use playlist_m3u::{M3uDoc, M3uEntry, ParsedRendition, PlaylistMeta, SoftMeta, parse_m3u, write_m3u};
pub use plugin::{Plugin, PluginCommand, PluginHealth, SetupKind, Wiring};
pub use queue::{Queue, RepeatSetting};
pub use resolver::{Resolution, Resolver, Target, is_local_source, local_path_from_uri};
pub use scan::{Outcome, ScanDriver, ScanFetchMode, ScanPlugin, ScanStatus, TrackMeta, open_scan_audio};
pub use search::Search;
pub use store::{MemStore, RedbStore};
pub use traits::{
    BrowseNode, BrowsePage, Error, Media, MediaProvider, Player, PlayerState, PlayerStatus,
    ReadSeek, Result, Source, Store,
};
pub use types::{
    ItemKind, LinkReason, Playlist, PlaylistId, Quality, Rendition, SearchHit, SearchQuery,
    SourceId, Track, TrackId, Uuid, parse_artist_title,
};
