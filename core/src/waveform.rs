//! The per-track amplitude envelope a scan plugin stores in `Track::attrs`, as hex.

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
