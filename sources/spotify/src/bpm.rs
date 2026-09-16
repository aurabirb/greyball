//! `BpmPlugin`: Analyzes song BPM; implements `core::ScanPlugin`.
//! The estimator is a clean-room implementation of standard beat-tracking
//! DSP (no code taken from any GPL library):
//!
//! 1. **Multi-band log-magnitude spectral flux** for the onset envelope,
//!    with a SuperFlux-style frequency max-filter on the reference frame to
//!    suppress vibrato/glissando false positives.
//! 2. **Per-band normalisation** before the bands are summed, so a loud kick
//!    drum doesn't drown out everything happening further up the spectrum.
//! 3. **Windowed autocorrelation with a harmonic comb filter**: each
//!    candidate tempo is scored by the autocorrelation energy at its period
//!    *and its integer multiples*, which makes the true tempo win over its
//!    own half/double time.
//! 4. **Aggregation across overlapping windows** (mode of the per-window
//!    estimates), so an intro, breakdown or slight drift doesn't throw the
//!    whole track off.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use core::{MediaCache, Outcome, ReadSeek, Rendition, ScanPlugin, Track, TrackMeta};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use rustfft::{Fft, FftPlanner, num_complex::Complex};

/// Spotify prepends a proprietary header to its Ogg Vorbis streams; real Vorbis data starts here.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

/// librespot's fixed output sample rate (it resamples nothing; every Spotify stream is 44.1 kHz).
/// The onset/tempo DSP below assumes this rate for its own analysis windows
/// regardless of what `core::audio_decode::decode_stereo_prefix` reports for
/// a non-Spotify source — out of scope here; only the decode/caching layer
/// changed.
const SAMPLE_RATE: f32 = 44_100.0;

/// STFT window and hop for the onset envelope.
const FFT_SIZE: usize = 2048;
const HOP: usize = 512;

/// Onset-envelope sample rate (one value per STFT hop).
const ONSET_RATE: f32 = SAMPLE_RATE / HOP as f32;

/// Number of log-spaced frequency bands the spectral flux is split into.
const BANDS: usize = 8;

/// Lowest / highest frequency (Hz) considered for onsets.
const LOW_HZ: f32 = 30.0;
const HIGH_HZ: f32 = 11_000.0;

/// Radius, in FFT bins, of the max-filter applied to the reference frame (SuperFlux).
const MAX_FILTER_RADIUS: usize = 3;

/// Tempo search range. Everything outside this is treated as a bad estimate.
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;

/// Length / hop of the analysis windows the tempo is estimated on, in seconds.
const WIN_SECONDS: f32 = 8.0;
const WIN_HOP_SECONDS: f32 = 4.0;

/// How many harmonics of a candidate period the comb filter sums over.
const COMB_HARMONICS: usize = 4;

/// `ScanPlugin` for local BPM detection. Registered iff
/// `cfg.scan.bpm.enabled` and the `spotify` cargo feature.
pub struct BpmPlugin {
    min_interval: Duration,
}

impl BpmPlugin {
    pub fn new(min_interval_secs: u64) -> Self {
        Self { min_interval: Duration::from_secs(min_interval_secs) }
    }
}

impl ScanPlugin for BpmPlugin {
    fn id(&self) -> &'static str {
        "bpm"
    }

    fn name(&self) -> &'static str {
        "BPM"
    }

    fn needs(&self, track: &Track) -> bool {
        // Not Spotify-specific: the DSP below only cares about decoded PCM,
        // and `open_scan_audio` already fetches it generically (any
        // source's `MediaProvider`, or a source's own `Player::open_for_scan`
        // for one like Spotify with no `MediaProvider` at all) — so this
        // plugin scans every track, not just ones with a Spotify rendition.
        !track.attrs.contains_key("bpm")
    }

    fn analyze(
        &self,
        track: &Track,
        audio: &dyn Fn() -> Option<(Rendition, Box<dyn ReadSeek + Send>)>,
        media_cache: &MediaCache,
    ) -> Outcome {
        let cached = track
            .renditions
            .iter()
            .find_map(|r| media_cache.cached_path(&r.source, &r.uri))
            .and_then(|p| std::fs::File::open(p).ok())
            .and_then(|f| core::audio_decode::decode_stereo_prefix(Box::new(f)));
        let stereo = match cached {
            Some((stereo, _)) => stereo,
            None => {
                let Some((r, audio)) = audio() else {
                    log::debug!("bpm: \"{}\" — no audio to analyze, skipping", track.title);
                    return Outcome::Skip;
                };
                let Ok(raw) = core::audio_decode::read_all(audio) else {
                    log::warn!("bpm: \"{}\" — couldn't read audio to analyze, skipping", track.title);
                    return Outcome::Skip;
                };
                // Decode from a copy: `raw` itself (already-fetched,
                // already-unencrypted bytes) is what populates `media_cache`
                // below, as-is — no re-encode needed or wanted.
                let Some((stereo, _)) =
                    core::audio_decode::decode_stereo_prefix(Box::new(std::io::Cursor::new(raw.clone())))
                else {
                    log::warn!(
                        "bpm: \"{}\" — decode failed or too short to analyze, skipping",
                        track.title
                    );
                    return Outcome::Skip;
                };
                if let Err(e) = media_cache.put(&r.source, &r.uri, &raw) {
                    log::debug!("bpm: \"{}\" — couldn't populate media cache: {e}", track.title);
                }
                stereo
            }
        };
        match estimate_tempo(&onset_envelope(&downmix(&stereo))) {
            Some(bpm) => {
                log::debug!("bpm: \"{}\" — estimated {bpm:.0} bpm", track.title);
                Outcome::Done(TrackMeta { attrs: [("bpm".to_string(), format!("{bpm:.0}"))].into() })
            }
            None => {
                log::debug!("bpm: \"{}\" — no confident tempo estimate, skipping", track.title);
                Outcome::Skip
            }
        }
    }

    fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

// --- Spotify audio retrieval (used by `SpotifyPlayer::open_for_scan`) --------------------------

/// The Ogg Vorbis formats we're willing to analyze, smallest first.
const OGG_FORMATS: [AudioFileFormat; 3] =
    [AudioFileFormat::OGG_VORBIS_96, AudioFileFormat::OGG_VORBIS_160, AudioFileFormat::OGG_VORBIS_320];

/// Fetch, decrypt and header-skip a Spotify `uri`'s Ogg Vorbis audio for
/// offline analysis. Mirrors `PlayerTrackLoader::load_remote_track`, minus
/// everything playback-specific. `mode`: see `core::ScanFetchMode` —
/// `Full` keeps draining the stream after the returned reader is dropped so
/// librespot commits the whole file to its on-disk cache; `CacheOnly` never
/// fetches over the network at all.
pub async fn fetch_scan_audio(
    session: &Session,
    uri: &str,
    mode: core::ScanFetchMode,
) -> core::Result<Box<dyn ReadSeek + Send>> {
    let parsed = SpotifyUri::from_uri(uri).map_err(|_| core::Error::Unsupported("bad spotify uri"))?;
    let (source, from_cache) = open_audio_stream(session, parsed, mode)
        .await
        .map_err(|reason| core::Error::Other(format!("spotify: scan audio unavailable: {reason}")))?;
    // `Subfile` seeks to the offset on construction, skipping Spotify's
    // custom Ogg header so symphonia sees a clean Vorbis stream.
    let subfile = Subfile::new(source, SPOTIFY_OGG_HEADER_END)
        .map_err(|_| core::Error::Unsupported("spotify: scan seek failed"))?;
    let drain = mode == core::ScanFetchMode::Full && !from_cache;
    Ok(Box::new(DrainOnDrop { inner: subfile, drain }))
}

/// Whether any Ogg Vorbis rendition of `uri` already sits fully in
/// librespot's own on-disk cache. Metadata-only (`AudioItem::get_file`) —
/// never calls `AudioFile::open`, so unlike `fetch_scan_audio` this can
/// never itself originate (or race) a CDN fetch, and is safe for
/// `SpotifyPlayer`'s worker to poll on every tick while a track loads.
pub(crate) async fn is_materialized(session: &Session, uri: &str) -> bool {
    let Ok(parsed) = SpotifyUri::from_uri(uri) else { return false };
    let Ok(audio_item) = AudioItem::get_file(session, parsed).await else { return false };
    let Some(audio_item) = find_available(session, audio_item).await else { return false };
    let Some(cache) = session.cache() else { return false };
    OGG_FORMATS
        .iter()
        .filter_map(|f| audio_item.files.get(f))
        .any(|id| cache.file_path(*id).is_some_and(|p| p.exists()))
}

async fn open_audio_stream(
    session: &Session,
    uri: SpotifyUri,
    mode: core::ScanFetchMode,
) -> Result<(AudioDecrypt<AudioFile>, bool), String> {
    let Some(track_id): Option<SpotifyId> = (&uri).try_into().ok() else {
        return Err("uri isn't a track id, can't fetch scan audio".to_string());
    };

    let audio_item = AudioItem::get_file(session, uri)
        .await
        .map_err(|e| format!("{track_id:?}: AudioItem::get_file failed: {e}"))?;
    let Some(audio_item) = find_available(session, audio_item).await else {
        return Err(format!("{track_id:?}: no available (region-relinked) audio item"));
    };

    // Prefer whichever Vorbis file is already in the local audio cache (i.e.
    // the bitrate the track was played at) so analysis reuses those bytes
    // and never touches the CDN. Only when nothing is cached do we fall
    // back to the smallest available file, to keep that download light.
    let available = || {
        OGG_FORMATS.iter().filter_map(|f| audio_item.files.get(f).map(|id| (*f, *id)))
    };
    let cache = session.cache();
    let cached = available()
        .find(|(_, id)| cache.and_then(|c| c.file_path(*id)).is_some_and(|p| p.exists()));
    let (format, file_id) = match cached {
        Some(hit) => hit,
        // `CacheOnly` (the prioritized now-playing path) never
        // originates a network fetch — the live player is already fetching
        // this exact track, so racing it here would risk a duplicate CDN
        // hit and a corrupt interleaved cache write. Bail out exactly as if
        // no audio were available at all.
        None if mode == core::ScanFetchMode::CacheOnly => {
            return Err(format!("{track_id:?}: not yet cached, skipping (CacheOnly)"));
        }
        None => match available().next() {
            Some(f) => f,
            None => return Err(format!("{track_id:?}: no Ogg Vorbis file available at all")),
        },
    };

    let bytes_per_second = stream_data_rate(format);
    let encrypted = AudioFile::open(session, file_id, bytes_per_second)
        .await
        .map_err(|e| format!("{track_id:?}: AudioFile::open (fetch) failed: {e}"))?;
    let from_cache = matches!(encrypted, AudioFile::Cached(_));

    // Some files aren't encrypted; if the key request fails, continue
    // undecrypted and let the decoder bail if it turns out the data really
    // was scrambled (same policy as librespot).
    let key = match session.audio_key().request(track_id, file_id).await {
        Ok(k) => Some(k),
        Err(e) => {
            log::debug!("bpm: spotify {track_id:?} — audio key request failed, trying undecrypted: {e}");
            None
        }
    };

    Ok((AudioDecrypt::new(key, encrypted), from_cache))
}

/// A track is playable as-is if it has files and is available; otherwise
/// follow its alternatives (region-relinked equivalents) and take the first
/// that is. Condensed from `PlayerTrackLoader::find_available_alternative`.
async fn find_available(session: &Session, audio_item: AudioItem) -> Option<AudioItem> {
    if audio_item.availability.is_err() {
        return None;
    }
    if !audio_item.files.is_empty() {
        return Some(audio_item);
    }

    for alt_uri in audio_item.alternatives?.0 {
        if let Ok(alt) = AudioItem::get_file(session, alt_uri).await
            && alt.availability.is_ok()
            && !alt.files.is_empty()
        {
            return Some(alt);
        }
    }
    None
}

/// Nominal bytes per second for a format, used to size streaming reads.
/// Values match librespot's `PlayerTrackLoader::stream_data_rate`
/// (kilobytes/s * 1024).
fn stream_data_rate(format: AudioFileFormat) -> usize {
    let kbps = match format {
        AudioFileFormat::OGG_VORBIS_96 => 12.0,
        AudioFileFormat::OGG_VORBIS_160 => 20.0,
        _ => 40.0,
    };
    (kbps * 1024.0_f32).ceil() as usize
}

/// Fold a stereo signal to mono (equal-weight L/R average).
fn downmix(stereo: &[[f32; 2]]) -> Vec<f32> {
    stereo.iter().map(|[l, r]| (l + r) * 0.5).collect()
}

// --- Onset envelope --------------------------------------------------------------------------

/// Multi-band log-magnitude spectral flux. For every STFT hop and every frequency band, the sum of
/// the positive changes in (log) magnitude relative to a frequency-max-filtered previous frame.
/// Each band's series is then normalised to unit variance before the bands are summed, so no single
/// part of the spectrum dominates the result.
fn onset_envelope(signal: &[f32]) -> Vec<f32> {
    let mut planner = FftPlanner::<f32>::new();
    let fft: Arc<dyn Fft<f32>> = planner.plan_fft_forward(FFT_SIZE);

    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|n| (std::f32::consts::PI * n as f32 / FFT_SIZE as f32).sin().powi(2))
        .collect();

    let bins = FFT_SIZE / 2 + 1;
    let band_edges = log_spaced_band_edges(bins);

    let frames = signal.len().saturating_sub(FFT_SIZE) / HOP + 1;
    let mut band_flux: [Vec<f32>; BANDS] = std::array::from_fn(|_| Vec::with_capacity(frames));

    let mut prev_log = vec![0.0_f32; bins];
    let mut cur_log = vec![0.0_f32; bins];
    let mut scratch = vec![Complex::<f32>::default(); FFT_SIZE];

    let mut pos = 0;
    let mut first = true;
    while pos + FFT_SIZE <= signal.len() {
        for (i, s) in scratch.iter_mut().enumerate() {
            *s = Complex::new(signal[pos + i] * window[i] / FFT_SIZE as f32, 0.0);
        }
        fft.process(&mut scratch);
        for (slot, bin) in cur_log.iter_mut().zip(&scratch) {
            *slot = (1.0 + 1000.0 * bin.norm()).ln();
        }

        if first {
            for flux in band_flux.iter_mut() {
                flux.push(0.0);
            }
            first = false;
        } else {
            for (b, flux) in band_flux.iter_mut().enumerate() {
                let (lo, hi) = (band_edges[b], band_edges[b + 1]);
                let mut sum = 0.0_f32;
                for (k, &cur) in cur_log.iter().enumerate().take(hi).skip(lo) {
                    let ref_lo = k.saturating_sub(MAX_FILTER_RADIUS);
                    let ref_hi = (k + MAX_FILTER_RADIUS + 1).min(bins);
                    let reference =
                        prev_log[ref_lo..ref_hi].iter().copied().fold(f32::MIN, f32::max);
                    let diff = cur - reference;
                    if diff > 0.0 {
                        sum += diff;
                    }
                }
                flux.push(sum);
            }
        }

        std::mem::swap(&mut prev_log, &mut cur_log);
        pos += HOP;
    }

    let len = band_flux.first().map_or(0, Vec::len);
    let mut envelope = vec![0.0_f32; len];
    for flux in &band_flux {
        let (mean, std) = mean_std(flux);
        if std <= f32::EPSILON {
            continue;
        }
        for (e, &f) in envelope.iter_mut().zip(flux) {
            *e += (f - mean) / std;
        }
    }

    detrend(&mut envelope, (ONSET_RATE * 0.5) as usize);
    envelope
}

/// FFT-bin indices splitting `[LOW_HZ, HIGH_HZ]` into [`BANDS`] logarithmically-spaced bands.
fn log_spaced_band_edges(bins: usize) -> Vec<usize> {
    let bin_of = |hz: f32| ((hz * FFT_SIZE as f32 / SAMPLE_RATE).round() as usize).clamp(1, bins);
    let (lo, hi) = (LOW_HZ.ln(), HIGH_HZ.ln());
    (0..=BANDS)
        .map(|b| {
            let frac = b as f32 / BANDS as f32;
            bin_of((lo + frac * (hi - lo)).exp())
        })
        .collect()
}

// --- Tempo estimation ------------------------------------------------------------------------

/// Centre (BPM) and log-space width of the perceptual tempo preference.
const TEMPO_CENTRE: f32 = 130.0;
const TEMPO_SIGMA: f32 = 0.55;

fn tempo_weight(bpm: f32) -> f32 {
    (-0.5 * ((bpm / TEMPO_CENTRE).ln() / TEMPO_SIGMA).powi(2)).exp()
}

/// Estimate the tempo of an onset envelope. The envelope is cut into overlapping windows; each
/// window is autocorrelated and scored with a harmonic comb filter plus a perceptual tempo weight,
/// the per-window estimates are reduced to their mode, and a final octave check decides between
/// that value and its half / double time over the whole envelope.
fn estimate_tempo(onset: &[f32]) -> Option<f32> {
    let min_lag = (60.0 * ONSET_RATE / MAX_BPM).floor().max(1.0) as usize;
    let max_lag = (60.0 * ONSET_RATE / MIN_BPM).ceil() as usize;

    let win = (WIN_SECONDS * ONSET_RATE) as usize;
    let hop = (WIN_HOP_SECONDS * ONSET_RATE) as usize;
    if onset.len() < win.min(4 * max_lag) || max_lag <= min_lag {
        return None;
    }

    let mut estimates = Vec::new();
    let mut start = 0;
    while start + win <= onset.len() {
        if let Some(bpm) = window_tempo(&onset[start..start + win], min_lag, max_lag) {
            estimates.push(bpm);
        }
        start += hop;
    }
    if estimates.is_empty()
        && let Some(bpm) = window_tempo(onset, min_lag, max_lag)
    {
        estimates.push(bpm);
    }
    if estimates.is_empty() {
        return None;
    }

    let agg = aggregate(&estimates);
    let final_bpm = resolve_octave(onset, agg);
    (MIN_BPM..=MAX_BPM).contains(&final_bpm).then_some(final_bpm.round())
}

/// Decide between `bpm`, its half time and its double time by scoring each over the *whole* onset
/// envelope (better periodicity SNR than any single window) with the harmonic comb filter and the
/// perceptual tempo weight. Fixes the common failure where a backbeat-heavy track autocorrelates
/// most strongly at half its actual tempo.
fn resolve_octave(onset: &[f32], bpm: f32) -> f32 {
    let (mean, _) = mean_std(onset);
    let centred: Vec<f32> = onset.iter().map(|&x| x - mean).collect();
    let energy = centred.iter().map(|&x| x * x).sum::<f32>() / centred.len() as f32;
    if energy <= f32::EPSILON {
        return bpm;
    }

    let max_lag = centred.len() / 2;
    let comb = |candidate: f32| -> f32 {
        let base = 60.0 * ONSET_RATE / candidate;
        let mut acc = 0.0;
        let mut count = 0.0;
        for h in 1..=COMB_HARMONICS {
            let lag = base * h as f32;
            if lag as usize >= max_lag {
                break;
            }
            acc += autocorr_at(&centred, lag) / energy;
            count += 1.0;
        }
        if count == 0.0 { 0.0 } else { acc / count }
    };

    let scored: Vec<(f32, f32)> = [bpm * 0.5, bpm, bpm * 2.0]
        .into_iter()
        .filter(|c| (MIN_BPM..=MAX_BPM).contains(c))
        .map(|c| (c, comb(c)))
        .collect();

    let anchor = scored.iter().copied().fold(f32::MIN, |m, (_, c)| m.max(c));

    if anchor < MIN_COMB_CONFIDENCE {
        bpm
    } else {
        scored
            .iter()
            .filter(|(_, c)| *c >= OCTAVE_COMB_RATIO * anchor)
            .max_by(|a, b| tempo_weight(a.0).total_cmp(&tempo_weight(b.0)))
            .map_or(bpm, |(c, _)| *c)
    }
}

/// A faster/slower octave must reach at least this fraction of the best raw comb salience before
/// the perceptual weight is allowed to pick it over the strongest-periodicity anchor.
const OCTAVE_COMB_RATIO: f32 = 0.80;

/// Below this raw comb salience the onset envelope has no tempo worth arguing the octave of.
const MIN_COMB_CONFIDENCE: f32 = 0.15;

/// Autocorrelation of `centred` (already mean-subtracted) at a possibly fractional `lag`, via
/// linear interpolation between the two neighbouring integer lags.
fn autocorr_at(centred: &[f32], lag: f32) -> f32 {
    let l0 = lag.floor() as usize;
    let frac = lag - l0 as f32;
    let at = |l: usize| -> f32 {
        if l >= centred.len() {
            return 0.0;
        }
        let n = centred.len() - l;
        (0..n).map(|i| centred[i] * centred[i + l]).sum::<f32>() / n as f32
    };
    at(l0) * (1.0 - frac) + at(l0 + 1) * frac
}

/// Tempo of a single window: autocorrelation, harmonic comb scoring, log-normal prior, parabolic
/// interpolation on the winning lag for a fractional BPM.
fn window_tempo(window: &[f32], min_lag: usize, max_lag: usize) -> Option<f32> {
    let max_lag = max_lag.min(window.len() / 2);
    if max_lag <= min_lag {
        return None;
    }

    let (mean, _) = mean_std(window);
    let centred: Vec<f32> = window.iter().map(|&x| x - mean).collect();

    let mut acf = vec![0.0_f32; max_lag + 2];
    for (lag, slot) in acf.iter_mut().enumerate() {
        let mut sum = 0.0_f32;
        for i in 0..centred.len() - lag {
            sum += centred[i] * centred[i + lag];
        }
        *slot = sum / (centred.len() - lag) as f32;
    }
    let zero = acf[0];
    if zero <= f32::EPSILON {
        return None;
    }
    for v in &mut acf {
        *v /= zero;
    }

    let mut best_lag = 0usize;
    let mut best_score = f32::MIN;
    for lag in min_lag..=max_lag {
        let mut acc = 0.0_f32;
        let mut count = 0.0_f32;
        for h in 1..=COMB_HARMONICS {
            let hl = lag * h;
            if hl > max_lag {
                break;
            }
            acc += acf[hl];
            count += 1.0;
        }
        let bpm = 60.0 * ONSET_RATE / lag as f32;
        let prior = (-0.5 * ((bpm / TEMPO_CENTRE).ln() / 0.9).powi(2)).exp();
        let score = (acc / count) * prior;
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }

    if best_lag == 0 || best_score <= 0.0 {
        return None;
    }

    let refined = parabolic_peak(&acf, best_lag);
    Some(60.0 * ONSET_RATE / refined)
}

/// Reduce per-window tempo estimates to a single value: bucket to the nearest BPM, take the most
/// common bucket (ties broken towards 120), then average the raw estimates that fall in it.
fn aggregate(estimates: &[f32]) -> f32 {
    let mut counts: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for &b in estimates {
        *counts.entry(b.round() as i32).or_default() += 1;
    }
    let mode = counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| (b.0 - 120).abs().cmp(&(a.0 - 120).abs())))
        .map(|(bpm, _)| bpm)
        .unwrap_or(120);

    let (sum, n) = estimates
        .iter()
        .filter(|&&b| (b.round() as i32 - mode).abs() <= 1)
        .fold((0.0_f32, 0.0_f32), |(s, n), &b| (s + b, n + 1.0));
    if n > 0.0 { sum / n } else { mode as f32 }
}

/// Parabolic interpolation of the peak position around integer index `i` of `data`, for
/// sub-sample precision. Falls back to `i` at the array edges.
fn parabolic_peak(data: &[f32], i: usize) -> f32 {
    if i == 0 || i + 1 >= data.len() {
        return i as f32;
    }
    let (a, b, c) = (data[i - 1], data[i], data[i + 1]);
    let denom = a - 2.0 * b + c;
    if denom.abs() < f32::EPSILON {
        return i as f32;
    }
    i as f32 + 0.5 * (a - c) / denom
}

// --- small numeric helpers -----------------------------------------------------------------

/// Mean and (population) standard deviation of `data`. Returns `(0, 0)` for an empty slice.
fn mean_std(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let n = data.len() as f32;
    let mean = data.iter().sum::<f32>() / n;
    let var = data.iter().map(|&x| (x - mean).powi(2)).sum::<f32>() / n;
    (mean, var.sqrt())
}

/// Subtract a centred moving average of half-width `w` from `data`, clamping negatives to zero.
fn detrend(data: &mut [f32], w: usize) {
    if w == 0 || data.is_empty() {
        return;
    }
    let n = data.len();
    let prefix: Vec<f32> = std::iter::once(0.0)
        .chain(data.iter().scan(0.0, |acc, &x| {
            *acc += x;
            Some(*acc)
        }))
        .collect();
    for (i, value) in data.iter_mut().enumerate() {
        let lo = i.saturating_sub(w);
        let hi = (i + w + 1).min(n);
        let mean = (prefix[hi] - prefix[lo]) / (hi - lo) as f32;
        *value = (*value - mean).max(0.0);
    }
}

// --- Subfile / DrainOnDrop -------------------------------------------------------------------

/// A read-only window into `stream` starting at `offset`, `length` bytes long, with positions
/// reported relative to `offset`. Reimplemented from librespot's private `player::Subfile` so
/// symphonia sees the Vorbis data without Spotify's leading header.
struct Subfile<T: Read + Seek> {
    stream: T,
    offset: u64,
}

impl<T: Read + Seek> Subfile<T> {
    fn new(mut stream: T, offset: u64) -> io::Result<Self> {
        stream.seek(SeekFrom::Start(offset))?;
        Ok(Self { stream, offset })
    }
}

impl<T: Read + Seek> Read for Subfile<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<T: Read + Seek> Seek for Subfile<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(offset) => SeekFrom::Start(offset + self.offset),
            other => other,
        };
        let newpos = self.stream.seek(pos)?;
        Ok(newpos.saturating_sub(self.offset))
    }
}

/// Wraps a `Read + Seek` stream (a `Subfile`)
/// so that, with `drain` set, dropping it (once the plugin's `analyze` is
/// done reading) pulls the rest of the stream to completion — this is what
/// makes librespot's fetcher commit the whole file to its on-disk cache
/// instead of just the bytes the plugin actually read. This mirrors
/// `ScanOptions::cache_full` behaviour, applied on the fetch side
/// (`Player::open_for_scan`).
struct DrainOnDrop<S: Read + Seek> {
    inner: S,
    drain: bool,
}

impl<S: Read + Seek> Read for DrainOnDrop<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Read + Seek> Seek for DrainOnDrop<S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl<S: Read + Seek> Drop for DrainOnDrop<S> {
    fn drop(&mut self) {
        if !self.drain {
            return;
        }
        let mut buf = [0u8; 32 * 1024];
        while matches!(self.inner.read(&mut buf), Ok(n) if n > 0) {}
    }
}

