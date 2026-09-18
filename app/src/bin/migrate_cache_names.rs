//! One-off: adopt cache files written before the human-readable naming
//! index (commit 349d240) existed, renaming each to its proper
//! "Artist - Title.ext" and registering it in `MediaCache`'s redb index —
//! no re-fetch, no re-encode. Revert the commit that added this file once
//! it's been run; it's not part of the normal app.
//!
//! `cargo run --bin migrate_cache_names` (medley must not be running at the
//! same time — both open the same redb files).

use std::path::PathBuf;
use std::sync::Arc;

use medley_core::{MediaCache, RedbStore, Store};

fn data_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(x).join("medley");
    }
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local").join("share").join("medley")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = data_dir();
    let store: Arc<dyn Store> = Arc::new(RedbStore::open(dir.join("db"))?);
    let media_cache = MediaCache::new(dir.join("media-cache"), store.clone());

    let mut migrated = 0usize;
    let mut failed = 0usize;
    for track in store.all_tracks()? {
        for r in &track.renditions {
            match media_cache.migrate_legacy(&r.source, &r.uri) {
                Ok(Some(path)) => {
                    println!("migrated: {} -> {}", track.display_name(), path.display());
                    migrated += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("failed: {} ({}:{}): {e}", track.display_name(), r.source.as_str(), r.uri);
                    failed += 1;
                }
            }
        }
    }
    println!("done: {migrated} migrated, {failed} failed");
    Ok(())
}
