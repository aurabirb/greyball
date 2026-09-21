//! The per-track amplitude envelope a scan plugin stores in `Track::attrs`, as hex.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::TrackId;

pub const ATTR: &str = "waveform";

pub fn encode(buckets: &[u8]) -> String {
    buckets.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks_exact(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// Envelope length stored per track.
pub const BUCKETS: usize = 400;

/// Buckets filled so far for tracks still being analyzed, left to right; never persisted.
static LIVE: LazyLock<Mutex<HashMap<TrackId, Arc<[u8]>>>> = LazyLock::new(Mutex::default);

fn live_map() -> MutexGuard<'static, HashMap<TrackId, Arc<[u8]>>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The filled prefix of `id`'s envelope, when it is being built.
pub fn live(id: TrackId) -> Option<Arc<[u8]>> {
    live_map().get(&id).cloned()
}

pub fn publish_live(id: TrackId, prefix: &[u8]) {
    live_map().insert(id, prefix.into());
}

pub fn clear_live(id: TrackId) {
    live_map().remove(&id);
}
