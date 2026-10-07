//! Genre-embedding encode/decode shared by `sources/genre-embed` (writer) and the UI's
//! genre-map pane (reader), so the base64/float-vector convention lives in exactly one place.

use std::sync::Arc;

use base64::Engine;

use crate::Track;

/// `Track.attrs` key a genre embedding is stored under.
pub const GENRE_EMBEDDING_ATTR: &str = "embedding:genre";

pub fn has_genre_embedding(track: &Track) -> bool {
    track.attrs.contains_key(GENRE_EMBEDDING_ATTR)
}

/// Little-endian `f32`s, base64-encoded.
pub fn encode_genre_embedding(v: &[f32]) -> String {
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

pub fn decode_genre_embedding(s: &str) -> Option<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    let v: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    v.iter().all(|x| x.is_finite()).then_some(v)
}

/// A track and its decoded genre embedding, decoded once when it enters the cache.
#[derive(Clone)]
pub struct EmbeddedTrack {
    pub track: Arc<Track>,
    pub embedding: Arc<[f32]>,
}

impl EmbeddedTrack {
    /// `None` without a decodable, non-empty embedding.
    pub fn new(track: Track) -> Option<Self> {
        let embedding = decode_genre_embedding(track.attrs.get(GENRE_EMBEDDING_ATTR)?).filter(|e| !e.is_empty())?;
        Some(Self { track: Arc::new(track), embedding: embedding.into() })
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// L2-normalizes `v` in place, returning its pre-normalization norm.
pub fn normalize(v: &mut [f32]) -> f32 {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    norm
}
