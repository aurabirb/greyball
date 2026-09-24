//! `GetSongBpmPlugin`: looks up BPM from GetSongBPM.com's API (needs an API key).
//! Implements `core::ScanPlugin`, mirroring `sources/bpm-deezer`'s shape.

use std::time::Duration;

use core::http::describe;
use core::matcher::artist_fuzzy_matches;
use core::{Outcome, ScanPlugin, StreamHandle, Track, TrackMeta};
use serde::Deserialize;
use serde_json::Value;

const SEARCH_URL: &str = "https://api.getsong.co/search/";

#[derive(Deserialize)]
struct SearchResult {
    tempo: String,
    artist: SearchArtist,
}

#[derive(Deserialize)]
struct SearchArtist {
    name: String,
}

pub struct GetSongBpmPlugin {
    api_key: String,
    min_interval: Duration,
    client: reqwest::blocking::Client,
}

impl GetSongBpmPlugin {
    pub fn new(api_key: String, min_interval_secs: u64) -> Self {
        Self {
            api_key,
            min_interval: Duration::from_secs(min_interval_secs),
            client: reqwest::blocking::Client::new(),
        }
    }
}

impl ScanPlugin for GetSongBpmPlugin {
    fn id(&self) -> &'static str {
        "bpm-getsongbpm"
    }

    fn name(&self) -> &'static str {
        "GetSongBPM"
    }

    fn needs(&self, track: &Track) -> bool {
        !track.attrs.contains_key("bpm:getsongbpm")
    }

    fn needs_audio(&self) -> bool {
        false
    }

    fn analyze(&self, track: &Track, _audio: &dyn Fn() -> Result<StreamHandle, Outcome>, _wanted: &dyn Fn() -> bool) -> Outcome {
        if track.artists.is_empty() || track.title.is_empty() {
            return Outcome::Skip;
        }
        let resp = match self
            .client
            .get(SEARCH_URL)
            .query(&[("api_key", self.api_key.as_str()), ("type", "song"), ("lookup", track.title.as_str()), ("limit", "3")])
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                log::warn!("bpm-getsongbpm: search request failed: {}", describe(&e));
                return Outcome::Retry;
            }
        };
        if !resp.status().is_success() {
            log::warn!("bpm-getsongbpm: search returned {}", resp.status());
            return Outcome::Retry;
        }
        let body: Value = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                log::warn!("bpm-getsongbpm: search response parse failed: {}", describe(&e));
                return Outcome::Retry;
            }
        };
        // A no-match response is `{"search": {"error": "no result"}}` (an object); a hit is
        // `{"search": [...]}` (an array) — check the shape before deserializing results.
        let Some(results) = body.get("search").and_then(Value::as_array) else {
            log::debug!("bpm-getsongbpm: \"{}\" — no result, skipping", track.title);
            return Outcome::Skip;
        };
        let matched = results
            .iter()
            .filter_map(|r| serde_json::from_value::<SearchResult>(r.clone()).ok())
            .find(|r| artist_fuzzy_matches(&r.artist.name, &track.artists));
        let Some(result) = matched else {
            log::debug!("bpm-getsongbpm: \"{}\" — no match, skipping", track.title);
            return Outcome::Skip;
        };
        // Round to match the DSP/Deezer plugins' integer formatting — GetSongBPM's `tempo` can
        // carry decimals (e.g. "128.50"), and every consumer of the canonical "bpm" attr expects
        // a plain integer string.
        let Ok(tempo) = result.tempo.trim().parse::<f64>() else {
            log::warn!("bpm-getsongbpm: \"{}\" — unexpected tempo value {:?}, skipping", track.title, result.tempo);
            return Outcome::Skip;
        };
        log::debug!("bpm-getsongbpm: \"{}\" — estimated {tempo:.0}bpm", track.title);
        Outcome::Done(TrackMeta { attrs: [("bpm:getsongbpm".to_string(), format!("{tempo:.0}"))].into() })
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}
