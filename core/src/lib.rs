//! `core` — the generic, source-agnostic engine for medley.

pub mod app;
pub mod audio;
pub mod audio_decode;
pub mod catalog;
pub mod config;
pub mod embedding;
pub mod event;
mod hotkeys;
mod revised;
pub mod http;
pub mod logbuf;
pub mod matcher;
pub mod media_cache;
pub mod paged;
pub mod playlist_m3u;
pub mod plugin;
pub mod queue;
pub mod rate_limit;
pub mod resolver;
pub mod scan;
pub mod search;
pub mod similarity;
pub mod store;
pub mod stream;
pub mod traits;
pub mod types;
pub mod update;
pub mod waveform;
mod enqueue;
mod view_cache;

pub use app::{
    BindError, BuiltinAction, Command, Dispatch, HotkeyMembership, HotkeyTarget, LastPlayed, LikeKind, LikeMark, ListRef,
    PLUGIN_HEALTH_CHECK_INTERVAL, RECENTLY_PLAYED_MERGE_INTERVAL, Session, cache_rendition,
};
pub use view_cache::{PendingRows, ResultSet};
pub use catalog::Catalog;
pub use config::{
    expand_home, tilde, Axis, BpmScanConfig, Config, HttpConfig, Layout, PaneLayoutConfig, PaneMode, ScanConfig, Side,
    SoulseekConfig, SoundcloudConfig, SpotifyConfig, TOGGLABLE_SOURCES, VisConfig, WaveformScanConfig,
};
pub use event::{Bus, CoreEvent, MembershipOutcome, PlayerEvent};
pub use http::{HttpOptions, RangeReader, fetch_url_bytes, fetch_url_to};
pub use logbuf::LogBuf;
pub use audio::audio_ext;
pub use matcher::{Matcher, matches_all_tokens};
pub use media_cache::{MediaCache, move_cache};
pub use paged::{PagedList, PagedMap, RemotePage};
pub use playlist_m3u::{M3uDoc, M3uEntry, ParsedRendition, PlaylistMeta, SoftMeta, parse_m3u, write_m3u};
pub use plugin::{MutexExt, Plugin, PluginCommand, PluginHealth, SetupLog, SetupPrompt, SharedMedia, Wiring};
pub use queue::{Queue, RepeatSetting};
pub use rate_limit::{RateGate, RateLimiter};
pub use resolver::{Resolution, Resolver, Target, is_local_source, local_path_from_uri};
pub use scan::{Outcome, ScanDriver, ScanMode, ScanPlugin, ScanStatus, TrackMeta};
pub use search::Search;
pub use similarity::similarity;
pub use store::{MemStore, RedbStore};
pub use stream::{Claim, Intent, Key as StreamKey, StreamEngine, StreamHandle, StreamInfo, StreamReader, StreamState, StreamWriter, Stopped, fill_from_seekable, retry as stream_retry};
pub use traits::{
    BrowseNode, BrowsePage, Error, Media, MediaProvider, NodeMeta, PlaybackReport, Player, PlayerState, PlayerStatus,
    Result, Source, Store,
};
pub use types::{
    ItemKind, LinkReason, Playlist, PlaylistId, Quality, Rendition, SearchQuery,
    SourceId, Track, TrackId, Uuid, parse_artist_title,
};
