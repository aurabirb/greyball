//! `GenreEmbedPlugin`: computes a 1280-dim genre/style embedding per track via Essentia's
//! discogs-effnet model (**CC BY-NC-SA 4.0**, MTG-UPF — see `model` module doc), run with the
//! pure-Rust `tract-onnx` runtime (no Python/native Essentia dependency, CPU-only inference).
//! Implements `core::ScanPlugin`, mirroring `sources/bpm/src/lib.rs`'s shape. The embedding is
//! the foundation for a separate (not-yet-built) 2D PCA-projection visualization pane.

mod mel;
mod model;
mod resample;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use core::audio_decode::DecodeError;
use core::{Outcome, ScanPlugin, StreamHandle, Track, TrackMeta};
use mel::MelSpectrogram;

/// How much of a track to decode and analyze — the first minute, not the whole file. Mirrors
/// `sources/bpm`'s `ANALYSIS_SECONDS`; a single prefix window (rather than several windows spread
/// across the track) keeps this on the existing `decode_mono_prefix` helper instead of adding
/// seek-based decoding to `core::audio_decode` just for this plugin.
const ANALYSIS_SECONDS: f32 = 60.0;

pub struct GenreEmbedPlugin {
    min_interval: Duration,
    enabled: Arc<AtomicBool>,
    data_dir: PathBuf,
    mel: MelSpectrogram,
    /// Loaded lazily on first `analyze()`, not per track: parsing/optimizing the ONNX graph is
    /// too expensive to repeat. Already `Arc`-backed (`model::Plan`), so cloning it is cheap.
    plan: Mutex<Option<model::Plan>>,
}

impl GenreEmbedPlugin {
    pub fn new(data_dir: PathBuf, min_interval_secs: u64, enabled: Arc<AtomicBool>) -> Self {
        Self {
            min_interval: Duration::from_secs(min_interval_secs),
            enabled,
            data_dir,
            mel: MelSpectrogram::new(),
            plan: Mutex::new(None),
        }
    }

    /// Downloads (if needed) and loads the model, caching the result on the plugin.
    fn loaded_plan(&self) -> Result<model::Plan, String> {
        let mut guard = self.plan.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(plan) = guard.as_ref() {
            return Ok(plan.clone());
        }
        let path = model::ensure_model(&self.data_dir)?;
        let plan = model::load(&path)?;
        *guard = Some(plan.clone());
        Ok(plan)
    }
}

impl ScanPlugin for GenreEmbedPlugin {
    fn id(&self) -> &'static str {
        "genre-embed"
    }

    fn name(&self) -> &'static str {
        "Genre Embedding"
    }

    fn needs(&self, track: &Track) -> bool {
        self.enabled.load(Ordering::Relaxed) && !track.attrs.contains_key("embedding:genre")
    }

    fn needs_audio(&self) -> bool {
        true
    }

    fn analyze(&self, track: &Track, audio: &dyn Fn() -> Result<StreamHandle, Outcome>, wanted: &dyn Fn() -> bool) -> Outcome {
        let stream = match audio() {
            Ok(s) => s,
            Err(outcome) => return outcome,
        };
        let (mono, sample_rate) = match core::audio_decode::decode_mono_prefix(&stream, Some(ANALYSIS_SECONDS), wanted) {
            Ok(v) => v,
            Err(DecodeError::Interrupted) => return Outcome::Retry,
            Err(DecodeError::NoAudio) => {
                log::debug!("genre-embed: \"{}\" — no decodable audio to analyze, skipping", track.title);
                return Outcome::Skip;
            }
        };

        let mono16k = if sample_rate == mel::RATE { mono } else { resample::resample(&mono, sample_rate, mel::RATE) };
        let frames = self.mel.frames(&mono16k);

        let plan = match self.loaded_plan() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("genre-embed: model unavailable, retrying later: {e}");
                return Outcome::Retry;
            }
        };

        let embedding = match model::embed(&plan, &frames) {
            Ok(Some(v)) => v,
            Ok(None) => {
                log::debug!("genre-embed: \"{}\" — too short for one inference patch, skipping", track.title);
                return Outcome::Skip;
            }
            Err(e) => {
                log::warn!("genre-embed: \"{}\" — inference failed: {e}", track.title);
                return Outcome::Retry;
            }
        };

        if embedding.iter().any(|v| !v.is_finite()) {
            log::warn!("genre-embed: \"{}\" — embedding contains NaN/Inf, discarding (bad model load?)", track.title);
            return Outcome::Skip;
        }

        let bytes: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        log::debug!("genre-embed: \"{}\" — embedding computed ({} dims, {} b64 chars)", track.title, embedding.len(), encoded.len());
        Outcome::Done(TrackMeta { attrs: [("embedding:genre".to_string(), encoded)].into() })
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}
