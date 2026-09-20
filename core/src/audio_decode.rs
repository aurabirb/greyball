//! Format-agnostic decode of a stream's audio to PCM for the scan plugins (`BpmPlugin`, `WaveformPlugin`),
//! whatever container/codec symphonia recognises (Ogg Vorbis, MP3, AAC in MP4/fMP4, ...).

use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::stream::{StreamHandle, StreamReader, StreamState};

/// How long a container that cannot be read while incomplete (an MP4 with its index at the end) may take to finish.
const FINISH_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A stream reader as a symphonia source: seekable only once the stream is complete.
struct StreamSource {
    reader: StreamReader,
    len: Option<u64>,
    seekable: bool,
}

impl Read for StreamSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Seek for StreamSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.reader.seek(pos)
    }
}

impl MediaSource for StreamSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Nothing decodable in the stream.
    NoAudio,
    /// The stream failed or was cancelled before the decode finished.
    Interrupted,
}

fn interrupted(stream: &StreamHandle) -> bool {
    matches!(stream.info().state, StreamState::Failed(_) | StreamState::Cancelled)
}

/// Streams `stream` as stereo f32 frames in decoder-sized blocks; `on_block` returns `false` to stop early.
/// Decodes progressively while the stream is still filling (a read waits for the next bytes), and seekably once
/// it is complete. The sample rate on success.
pub fn decode_blocks(stream: &StreamHandle, mut on_block: impl FnMut(&[[f32; 2]]) -> bool) -> Result<u32, DecodeError> {
    let mut delivered = false;
    let complete = stream.info().state == StreamState::Done;
    let mut result = decode_once(stream, complete, &mut |b| {
        delivered = true;
        on_block(b)
    });
    if result == Err(DecodeError::NoAudio) && !complete && !delivered && !interrupted(stream) {
        // The container needs the whole file first: wait for it, then decode seekably.
        if !stream.wait_range(0..u64::MAX, FINISH_TIMEOUT) || stream.info().state != StreamState::Done {
            return Err(DecodeError::Interrupted);
        }
        result = decode_once(stream, true, &mut on_block);
    }
    if result.is_err() && interrupted(stream) {
        return Err(DecodeError::Interrupted);
    }
    result
}

fn decode_once(stream: &StreamHandle, seekable: bool, on_block: &mut dyn FnMut(&[[f32; 2]]) -> bool) -> Result<u32, DecodeError> {
    let len = seekable.then(|| stream.info().len).flatten();
    let source = StreamSource { reader: stream.reader(), len, seekable };
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&Hint::new(), mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|_| DecodeError::NoAudio)?;
    let mut format = probed.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or(DecodeError::NoAudio)?;
    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44_100);
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|_| DecodeError::NoAudio)?;

    let mut block: Vec<[f32; 2]> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        block.clear();
        push_frames(decoded, &mut block);
        if !on_block(&block) {
            return Ok(sample_rate);
        }
    }
    if interrupted(stream) {
        return Err(DecodeError::Interrupted);
    }
    Ok(sample_rate)
}

/// Decode `stream` to stereo f32 frames. `max_frames` (`None` for the whole file) stops decoding once that
/// many frames are in. `NoAudio` for a stream too short to be useful.
pub fn decode_stereo_prefix(stream: &StreamHandle, max_frames: Option<usize>) -> Result<(Vec<[f32; 2]>, u32), DecodeError> {
    let mut frames: Vec<[f32; 2]> = Vec::new();
    let sample_rate = decode_blocks(stream, |block| {
        frames.extend_from_slice(block);
        max_frames.is_none_or(|m| frames.len() < m)
    })?;
    if let Some(m) = max_frames {
        frames.truncate(m);
    }

    // Anything shorter than a second isn't useful for analysis.
    if frames.len() >= sample_rate as usize { Ok((frames, sample_rate)) } else { Err(DecodeError::NoAudio) }
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
