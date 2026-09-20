//! Generic, format-agnostic decode of compressed audio to PCM (for BPM
//! analysis), plus populating `MediaCache` from a source's raw fetched
//! bytes. The decode side is shared by every scan plugin (`BpmPlugin`, ...)
//! and the playback-cache-fallback path (`Session::play_from_cache`) —
//! neither needs its own codec-specific decoder, regardless of whether the
//! source was Ogg Vorbis (Spotify), MP3/AAC (SoundCloud/HTTP) or anything
//! else symphonia recognises.

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::media_cache::MediaCache;
use crate::traits::ReadSeek;
use crate::types::{Rendition, SourceId, Track};

/// Wraps a `Box<dyn ReadSeek + Send>` (not `Sync`) so symphonia's
/// `MediaSource` (which requires `Sync`) accepts it. Sound because the
/// decode below only ever touches it from the single thread that owns it.
struct SourceAdapter<R>(R, Option<u64>);

unsafe impl<R> Sync for SourceAdapter<R> {}

impl<R: Read> Read for SourceAdapter<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R: Seek> Seek for SourceAdapter<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl<R: Read + Seek + Send> MediaSource for SourceAdapter<R> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.1
    }
}

/// Streams `audio` as stereo f32 frames in decoder-sized blocks, auto-detecting the container/codec;
/// `on_block` returns `false` to stop early. The sample rate, or `None` when nothing could be decoded.
pub fn decode_blocks(mut audio: Box<dyn ReadSeek + Send>, mut on_block: impl FnMut(&[[f32; 2]]) -> bool) -> Option<u32> {
    let len = audio.seek(SeekFrom::End(0)).ok();
    audio.seek(SeekFrom::Start(0)).ok()?;
    let mss = MediaSourceStream::new(Box::new(SourceAdapter(audio, len)), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&Hint::new(), mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44_100);
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).ok()?;

    let mut block: Vec<[f32; 2]> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        block.clear();
        push_frames(decoded, &mut block);
        if !on_block(&block) {
            break;
        }
    }
    Some(sample_rate)
}

/// Decode `audio` to stereo f32 frames. `max_frames` (`None` for the whole file) stops decoding once
/// that many frames are in. `None` on a decode failure or a stream too short to be useful.
pub fn decode_stereo_prefix(
    audio: Box<dyn ReadSeek + Send>,
    max_frames: Option<usize>,
) -> Option<(Vec<[f32; 2]>, u32)> {
    let mut frames: Vec<[f32; 2]> = Vec::new();
    let sample_rate = decode_blocks(audio, |block| {
        frames.extend_from_slice(block);
        max_frames.is_none_or(|m| frames.len() < m)
    })?;
    if let Some(m) = max_frames {
        frames.truncate(m);
    }

    // Require at least a second — anything shorter isn't useful for
    // analysis.
    (frames.len() >= sample_rate as usize).then_some((frames, sample_rate))
}

/// The track's audio for analysis: its cached file, else fetched via `audio` and stored in `media_cache`.
pub fn open_analysis_audio(
    track: &Track,
    audio: &dyn Fn() -> Option<(Rendition, Box<dyn ReadSeek + Send>)>,
    media_cache: &MediaCache,
) -> Option<Box<dyn ReadSeek + Send>> {
    let cached = track
        .renditions
        .iter()
        .find_map(|r| media_cache.cached_path(&r.source, &r.uri))
        .and_then(|p| std::fs::File::open(p).ok());
    if let Some(file) = cached {
        return Some(Box::new(file));
    }
    let (r, audio) = audio()?;
    let raw = read_all(audio).inspect_err(|e| log::warn!("\"{}\" — couldn't read audio to analyze: {e}", track.title)).ok()?;
    if let Err(e) = media_cache.put(&r.source, &r.uri, &raw) {
        log::debug!("\"{}\" — couldn't populate media cache: {e}", track.title);
    }
    Some(Box::new(io::Cursor::new(raw)))
}

/// Convert one decoded packet's samples (any symphonia sample format, any
/// channel count) into stereo frames appended to `out`. Mono is duplicated
/// to both channels; >2 channels keep only the first two.
fn push_frames(decoded: AudioBufferRef, out: &mut Vec<[f32; 2]>) {
    let spec = *decoded.spec();
    let channels = spec.channels.count().max(1);
    let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
    buf.copy_interleaved_ref(decoded);
    for chunk in buf.samples().chunks(channels) {
        let l = chunk[0];
        let r = if channels >= 2 { chunk[1] } else { l };
        out.push([l, r]);
    }
}

/// Buffer `audio`'s entire byte stream into memory — for storing as-is in
/// `MediaCache` (see `decode_and_cache`) and/or decoding from the same
/// buffer without a second fetch (see `BpmPlugin::analyze`).
pub fn read_all(mut audio: Box<dyn ReadSeek + Send>) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    audio.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Populate `(source, uri)`'s `MediaCache` entry directly from `audio`'s raw
/// bytes, returning the resulting file path — no decode/re-encode step.
/// Every reader this is handed (an already-fetched HTTP/SoundCloud file,
/// Spotify's already-decrypted, header-stripped Ogg Vorbis) is already a
/// plain, unencrypted, playable file, so storing exactly what was fetched
/// avoids both the CPU cost and the quality loss of a needless transcode.
/// Used both by a scan plugin that had to fetch audio itself and by the
/// playback-cache-fallback path caching on demand for a rendition no scan
/// has cached yet.
pub fn decode_and_cache(
    cache: &MediaCache,
    source: &SourceId,
    uri: &str,
    audio: Box<dyn ReadSeek + Send>,
) -> Option<std::path::PathBuf> {
    let bytes = read_all(audio)
        .inspect_err(|e| log::debug!("audio_decode: read failed for {source} {uri}: {e}"))
        .ok()?;
    cache
        .put(source, uri, &bytes)
        .inspect_err(|e| log::debug!("audio_decode: couldn't populate media cache for {source} {uri}: {e}"))
        .ok()
}
