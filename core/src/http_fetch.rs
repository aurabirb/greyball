//! One-shot HTTP GET, shared by scanning (`scan::open_scan_audio`) and live
//! playback (`player::RodioPlayer`) for their raw-bytes fetch of a
//! `Media::Url` rendition. No on-disk caching here — see `MediaCache`'s own
//! doc: it only ever stores decoded audio, never as-fetched bytes, so every
//! caller either decodes what it gets (scanning) or throws it away once
//! playback ends (a `NamedTempFile`).

use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::Duration;

/// Same timeout the player's own pre-`MediaCache` download code used — no
/// point hanging a scan or playback thread forever on a stalled server.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .expect("reqwest client with just a timeout set should never fail to build")
    })
}

/// GETs `url` and streams the whole body into `w`.
pub fn fetch_url_to(url: &str, w: &mut impl Write) -> io::Result<()> {
    let mut resp = client().get(url).send().and_then(|r| r.error_for_status()).map_err(io::Error::other)?;
    resp.copy_to(w).map_err(io::Error::other)?;
    Ok(())
}

/// GETs `url` and returns the whole body.
pub fn fetch_url_bytes(url: &str) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    fetch_url_to(url, &mut buf)?;
    Ok(buf)
}
