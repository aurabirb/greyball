//! `SpotifyMediaProvider` — a `core::MediaProvider` over the decrypted, header-skipped Ogg Vorbis
//! audio of a track; the stream engine and `RodioPlayer` do the rest.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use core::{Error, Media, MediaProvider, Rendition, Result, SourceId};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{FileId, Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};

use crate::link::{self, Live, Slot};

/// Spotify prepends a proprietary header to its Ogg Vorbis streams; real Vorbis data starts here.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

/// Best first; 320 needs Premium, so a failing format falls through to the next.
const OGG_FORMATS: [AudioFileFormat; 3] =
    [AudioFileFormat::OGG_VORBIS_320, AudioFileFormat::OGG_VORBIS_160, AudioFileFormat::OGG_VORBIS_96];

const RETRY_PAUSE: Duration = Duration::from_millis(200);
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SpotifyMediaProvider {
    slot: Arc<Slot>,
}

impl SpotifyMediaProvider {
    pub fn new(auth: crate::auth::Auth) -> Self {
        Self { slot: link::spawn(auth) }
    }
}

impl MediaProvider for SpotifyMediaProvider {
    fn id(&self) -> SourceId {
        crate::source_id()
    }

    fn open(&self, r: &Rendition, wanted: &dyn Fn() -> bool) -> Result<Media> {
        let inner = open_with_retry(&self.slot, &r.uri, wanted)?;
        Ok(Media::from_reader(inner))
    }
}

/// Waits for a live session and opens the audio on it; a failure only because the link died under
/// it is retried once reconnected, other failures count toward recycling a wedged session.
fn open_with_retry(slot: &Slot, uri: &str, wanted: &dyn Fn() -> bool) -> Result<Subfile> {
    loop {
        let Some(live) = slot.wait_live(wanted) else {
            return Err(Error::Other("spotify: no longer wanted while waiting for a session".into()));
        };
        let opened = live.handle.block_on(async {
            tokio::select! {
                r = tokio::time::timeout(OPEN_TIMEOUT, open_audio(&live.session, uri)) => {
                    r.unwrap_or_else(|_| Err(Error::Other("spotify: opening the audio timed out".into())))
                }
                () = async { while wanted() { tokio::time::sleep(Duration::from_millis(250)).await } } => {
                    Err(Error::Other("spotify: no longer wanted while opening".into()))
                }
            }
        });
        match opened {
            Ok(audio) => {
                slot.succeeded();
                return Ok(audio);
            }
            Err(e) if link_moved(slot, &live) => {
                log::warn!("spotify: open failed over a dead session ({e}), retrying once reconnected");
                std::thread::sleep(RETRY_PAUSE);
            }
            Err(e) => {
                if wanted() {
                    slot.failed(&live);
                }
                return Err(e);
            }
        }
    }
}

fn link_moved(slot: &Slot, live: &Live) -> bool {
    live.session.is_invalid() || slot.current().is_none_or(|now| now.generation != live.generation)
}

fn unavailable(reason: String) -> Error {
    Error::Other(format!("spotify: audio unavailable: {reason}"))
}

async fn open_audio(session: &Session, uri: &str) -> Result<Subfile> {
    let parsed = SpotifyUri::from_uri(uri).map_err(|_| Error::Unsupported("bad spotify uri"))?;
    let track_id: SpotifyId =
        (&parsed).try_into().map_err(|_| unavailable(format!("{uri} isn't a track id")))?;
    let item = AudioItem::get_file(session, parsed)
        .await
        .map_err(|e| unavailable(format!("{uri}: AudioItem::get_file failed: {e}")))?;
    let Some(item) = find_available(session, item).await else {
        return Err(unavailable(format!("{uri}: no available (region-relinked) audio item")));
    };
    let mut last = format!("{uri}: no Ogg Vorbis file available");
    for (format, file_id) in OGG_FORMATS.iter().filter_map(|f| item.files.get(f).map(|id| (*f, *id))) {
        match open_file(session, track_id, format, file_id).await {
            Ok(stream) => return Subfile::new(stream).map_err(|e| unavailable(format!("{uri}: {e}"))),
            Err(e) => {
                log::debug!("spotify: {uri} {format:?} unavailable: {e}");
                last = e;
            }
        }
    }
    Err(unavailable(last))
}

async fn open_file(
    session: &Session,
    track_id: SpotifyId,
    format: AudioFileFormat,
    file_id: FileId,
) -> std::result::Result<(AudioDecrypt<AudioFile>, u64), String> {
    let encrypted = AudioFile::open(session, file_id, stream_data_rate(format))
        .await
        .map_err(|e| format!("{track_id:?}: AudioFile::open failed: {e}"))?;
    let size = encrypted
        .get_stream_loader_controller()
        .map_err(|e| format!("{track_id:?}: no stream controller: {e}"))?
        .len() as u64;
    // Never continue undecrypted: these bytes end up persisted in `MediaCache`.
    let key = session
        .audio_key()
        .request(track_id, file_id)
        .await
        .map_err(|e| format!("{track_id:?}: audio key request failed: {e}"))?;
    Ok((AudioDecrypt::new(Some(key), encrypted), size))
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

/// Nominal bytes per second for a format, used to size streaming reads (librespot's
/// `PlayerTrackLoader::stream_data_rate`).
fn stream_data_rate(format: AudioFileFormat) -> usize {
    let kbps = match format {
        AudioFileFormat::OGG_VORBIS_96 => 12.0,
        AudioFileFormat::OGG_VORBIS_160 => 20.0,
        _ => 40.0,
    };
    (kbps * 1024.0_f32).ceil() as usize
}

/// The decrypted file from `SPOTIFY_OGG_HEADER_END` on, so symphonia sees the Vorbis data without
/// Spotify's leading header. Positions are relative to it; `len` is its real length (librespot's own
/// `SeekFrom::End` is off by one).
struct Subfile {
    stream: AudioDecrypt<AudioFile>,
    len: u64,
}

impl Subfile {
    fn new((mut stream, size): (AudioDecrypt<AudioFile>, u64)) -> io::Result<Self> {
        stream.seek(SeekFrom::Start(SPOTIFY_OGG_HEADER_END))?;
        Ok(Self { stream, len: size.saturating_sub(SPOTIFY_OGG_HEADER_END) })
    }
}

impl Read for Subfile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Seek for Subfile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(n) => self.len.saturating_add_signed(n),
            SeekFrom::Current(n) => self.stream.stream_position()?.saturating_sub(SPOTIFY_OGG_HEADER_END).saturating_add_signed(n),
        };
        self.stream.seek(SeekFrom::Start(target + SPOTIFY_OGG_HEADER_END))?;
        Ok(target)
    }
}
