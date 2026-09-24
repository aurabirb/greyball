//! `DeezerBpmPlugin`: looks up BPM from Deezer's public catalog API (no auth needed).
//! Implements `core::ScanPlugin`, mirroring `sources/bpm`'s local DSP plugin's shape but over HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use core::http::describe;
use core::matcher::artist_fuzzy_matches;
use core::{Outcome, ScanPlugin, StreamHandle, Track, TrackMeta};
use serde::Deserialize;

const SEARCH_URL: &str = "https://api.deezer.com/search";
const TRACK_URL: &str = "https://api.deezer.com/track";

#[derive(Deserialize)]
struct SearchResponse {
    data: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    id: u64,
    artist: SearchArtist,
}

#[derive(Deserialize)]
struct SearchArtist {
    name: String,
}

#[derive(Deserialize)]
struct TrackDetail {
    bpm: f32,
}

pub struct DeezerBpmPlugin {
    min_interval: Duration,
    client: reqwest::blocking::Client,
    /// Settings-toggled: gates this plugin only, independent of the global scan mode.
    enabled: Arc<AtomicBool>,
}

impl DeezerBpmPlugin {
    /// `min_interval_secs`: Deezer's public API has no documented rate limit but is known to
    /// soft-throttle; the caller's default (5s) is a conservative guess, not a verified number.
    pub fn new(min_interval_secs: u64, enabled: Arc<AtomicBool>) -> Self {
        Self {
            min_interval: Duration::from_secs(min_interval_secs),
            client: reqwest::blocking::Client::new(),
            enabled,
        }
    }

    /// Search Deezer for `track`, returning the id of the first result whose artist fuzzy-matches.
    fn find_match(&self, track: &Track) -> Result<Option<u64>, Outcome> {
        let artist = track.artists.first().map(String::as_str).unwrap_or("");
        let query = format!("{artist} {}", track.title);
        let resp = self
            .client
            .get(SEARCH_URL)
            .query(&[("q", query.as_str())])
            .send()
            .map_err(|e| {
                log::warn!("bpm-deezer: search request failed: {}", describe(&e));
                Outcome::Retry
            })?;
        if !resp.status().is_success() {
            log::warn!("bpm-deezer: search returned {}", resp.status());
            return Err(Outcome::Retry);
        }
        let body: SearchResponse = resp.json().map_err(|e| {
            log::warn!("bpm-deezer: search response parse failed: {}", describe(&e));
            Outcome::Retry
        })?;
        Ok(body
            .data
            .into_iter()
            .find(|r| artist_fuzzy_matches(&r.artist.name, &track.artists))
            .map(|r| r.id))
    }

    fn fetch_bpm(&self, id: u64) -> Result<f32, Outcome> {
        let resp = self.client.get(format!("{TRACK_URL}/{id}")).send().map_err(|e| {
            log::warn!("bpm-deezer: track detail request failed: {}", describe(&e));
            Outcome::Retry
        })?;
        if !resp.status().is_success() {
            log::warn!("bpm-deezer: track detail returned {}", resp.status());
            return Err(Outcome::Retry);
        }
        let detail: TrackDetail = resp.json().map_err(|e| {
            log::warn!("bpm-deezer: track detail parse failed: {}", describe(&e));
            Outcome::Retry
        })?;
        Ok(detail.bpm)
    }
}

impl ScanPlugin for DeezerBpmPlugin {
    fn id(&self) -> &'static str {
        "bpm-deezer"
    }

    fn name(&self) -> &'static str {
        "Deezer BPM"
    }

    fn needs(&self, track: &Track) -> bool {
        self.enabled.load(Ordering::Relaxed) && !track.attrs.contains_key("bpm:deezer")
    }

    fn needs_audio(&self) -> bool {
        false
    }

    fn analyze(&self, track: &Track, _audio: &dyn Fn() -> Result<StreamHandle, Outcome>, _wanted: &dyn Fn() -> bool) -> Outcome {
        if track.artists.is_empty() || track.title.is_empty() {
            return Outcome::Skip;
        }
        let id = match self.find_match(track) {
            Ok(Some(id)) => id,
            Ok(None) => {
                log::debug!("bpm-deezer: \"{}\" — no match, skipping", track.title);
                return Outcome::Skip;
            }
            Err(outcome) => return outcome,
        };
        let bpm = match self.fetch_bpm(id) {
            Ok(bpm) => bpm,
            Err(outcome) => return outcome,
        };
        if bpm <= 0.0 {
            log::debug!("bpm-deezer: \"{}\" — matched but Deezer has no BPM for it, skipping", track.title);
            return Outcome::Skip;
        }
        log::debug!("bpm-deezer: \"{}\" — estimated {bpm:.0}bpm", track.title);
        Outcome::Done(TrackMeta { attrs: [("bpm:deezer".to_string(), format!("{bpm:.0}"))].into() })
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}
