//! Fetches a Spotify track's raw Ogg Vorbis audio for background scanning
//! (e.g. by `bpm::BpmPlugin`), used from `SpotifyPlayer::open_for_scan`. The
//! actual analysis is generic and lives in the separate `bpm` crate — this
//! module only knows how to get bytes out of Spotify.

use std::io::{self, Read, Seek, SeekFrom};

use core::ReadSeek;
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{FileId, Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};

/// Spotify prepends a proprietary header to its Ogg Vorbis streams; real Vorbis data starts here.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

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
    let audio_item = AudioItem::get_file(session, parsed)
        .await
        .map_err(|e| unavailable(format!("{uri}: AudioItem::get_file failed: {e}")))?;
    let Some(audio_item) = find_available(session, audio_item).await else {
        return Err(unavailable(format!("{uri}: no available (region-relinked) audio item")));
    };
    open_audio(session, uri, &audio_item, mode).await
}

/// `fetch_scan_audio` off an already-resolved `audio_item`: no metadata request, never a CDN fetch.
pub(crate) async fn open_materialized(
    session: &Session,
    uri: &str,
    audio_item: &AudioItem,
) -> core::Result<Box<dyn ReadSeek + Send>> {
    open_audio(session, uri, audio_item, core::ScanFetchMode::CacheOnly).await
}

/// Whether an Ogg Vorbis file of `audio_item` sits fully in librespot's on-disk cache; disk-only.
pub(crate) fn is_materialized(session: &Session, audio_item: &AudioItem) -> bool {
    cached_file(session, audio_item).is_some()
}

fn unavailable(reason: String) -> core::Error {
    core::Error::Other(format!("spotify: scan audio unavailable: {reason}"))
}

fn ogg_files(audio_item: &AudioItem) -> impl Iterator<Item = (AudioFileFormat, FileId)> + '_ {
    OGG_FORMATS.iter().filter_map(|f| audio_item.files.get(f).map(|id| (*f, *id)))
}

fn cached_file(session: &Session, audio_item: &AudioItem) -> Option<(AudioFileFormat, FileId)> {
    let cache = session.cache()?;
    ogg_files(audio_item).find(|(_, id)| cache.file_path(*id).is_some_and(|p| p.exists()))
}

async fn open_audio(
    session: &Session,
    uri: &str,
    audio_item: &AudioItem,
    mode: core::ScanFetchMode,
) -> core::Result<Box<dyn ReadSeek + Send>> {
    let (source, from_cache) =
        open_audio_stream(session, uri, audio_item, mode).await.map_err(unavailable)?;
    // `Subfile` seeks to the offset on construction, skipping Spotify's
    // custom Ogg header so symphonia sees a clean Vorbis stream.
    let subfile = Subfile::new(source, SPOTIFY_OGG_HEADER_END)
        .map_err(|_| core::Error::Unsupported("spotify: scan seek failed"))?;
    let drain = mode == core::ScanFetchMode::Full && !from_cache;
    Ok(Box::new(DrainOnDrop { inner: subfile, drain }))
}

async fn open_audio_stream(
    session: &Session,
    uri: &str,
    audio_item: &AudioItem,
    mode: core::ScanFetchMode,
) -> Result<(AudioDecrypt<AudioFile>, bool), String> {
    let track_id: SpotifyId = SpotifyUri::from_uri(uri)
        .ok()
        .and_then(|u| (&u).try_into().ok())
        .ok_or_else(|| format!("{uri} isn't a track id, can't fetch scan audio"))?;

    // Prefer whichever Vorbis file is already in the local audio cache (i.e.
    // the bitrate the track was played at) so analysis reuses those bytes
    // and never touches the CDN. Only when nothing is cached do we fall
    // back to the smallest available file, to keep that download light.
    let (format, file_id) = match cached_file(session, audio_item) {
        Some(hit) => hit,
        // `CacheOnly` (the prioritized now-playing path) never
        // originates a network fetch — the live player is already fetching
        // this exact track, so racing it here would risk a duplicate CDN
        // hit and a corrupt interleaved cache write. Bail out exactly as if
        // no audio were available at all.
        None if mode == core::ScanFetchMode::CacheOnly => {
            return Err(format!("{track_id:?}: not yet cached, skipping (CacheOnly)"));
        }
        None => match ogg_files(audio_item).next() {
            Some(f) => f,
            None => return Err(format!("{track_id:?}: no Ogg Vorbis file available at all")),
        },
    };

    let bytes_per_second = stream_data_rate(format);
    let encrypted = AudioFile::open(session, file_id, bytes_per_second)
        .await
        .map_err(|e| format!("{track_id:?}: AudioFile::open (fetch) failed: {e}"))?;
    let from_cache = matches!(encrypted, AudioFile::Cached(_));

    // Unlike playback, never continue undecrypted: these bytes end up persisted in `MediaCache`.
    let key = session
        .audio_key()
        .request(track_id, file_id)
        .await
        .map_err(|e| format!("{track_id:?}: audio key request failed: {e}"))?;

    Ok((AudioDecrypt::new(Some(key), encrypted), from_cache))
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
