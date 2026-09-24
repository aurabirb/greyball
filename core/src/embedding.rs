//! Genre-embedding encode/decode shared by `sources/genre-embed` (writer) and the UI's
//! genre-map pane (reader), so the base64/float-vector convention lives in exactly one place.

use base64::Engine;

/// `Track.attrs` key a genre embedding is stored under.
pub const GENRE_EMBEDDING_ATTR: &str = "embedding:genre";

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
    Some(bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
}
