//! `build_session` — the constructor seam.
//!
//! A free function that wires `Catalog`/`Search`/`Queue` from the
//! registries. That wiring already lives in `medley_core::Session::new`, so
//! `build_session` is a thin, explicitly-named wrapper: it is the seam the
//! test and `main.rs` call so `core` never carries a test-only entry point.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use medley_core::{
    Bus, Config, MediaCache, MediaProvider, Player, Plugin, ScanDriver, ScanPlugin, Session, Source,
    SourceId, Store,
};

/// Build a fully-wired headless [`Session`] from the registries.
///
/// Prod (`main.rs`) passes `RodioPlayer` + a real redb path + `Bus::with_sink`;
/// the M1c test passes `NullPlayer` + a tempdir redb path + `Bus::new()`.
#[allow(clippy::too_many_arguments)]
pub fn build_session(
    cfg: Config,
    bus: Bus,
    store: Arc<dyn Store>,
    sources: HashMap<SourceId, Arc<dyn Source>>,
    media: HashMap<SourceId, Arc<dyn MediaProvider>>,
    players: HashMap<SourceId, Arc<dyn Player>>,
    plugins: Vec<Arc<dyn Plugin>>,
    scan_plugins: Vec<Arc<dyn ScanPlugin>>,
    media_cache: Arc<MediaCache>,
    history_path: PathBuf,
) -> Session {
    let cache_full = cfg.scan.cache_full;
    let mut session = Session::new(
        cfg,
        bus,
        store.clone(),
        sources,
        media.clone(),
        players.clone(),
        plugins,
        media_cache.clone(),
        history_path,
    );
    // Always spawned, even with an empty initial plugin list — plugins that
    // only become available after async setup register later via
    // `ScanDriver::register_plugin`.
    let driver = Arc::new(ScanDriver::new(scan_plugins, cache_full));
    driver.spawn(session.catalog.clone(), store, media, players, media_cache);
    session.scan = Some(driver);
    session
}
