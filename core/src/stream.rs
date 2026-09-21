//! Streams: a sparse file a producer fills in any order while readers read what is already there.
//! `StreamEngine::open` is the one entry point; see `docs/streaming-playback.md`.

use std::collections::HashMap;
use std::fs::File;
use std::mem::Discriminant;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use crate::event::{Bus, CoreEvent};
use crate::http::{HttpOptions, RangeReader};
use crate::media_cache::MediaCache;
use crate::plugin::SharedMedia;
use crate::traits::{Error, Media, MediaProvider, Result};
use crate::types::{Rendition, SourceId};

pub type Key = (SourceId, String);

/// How long a producer keeps going after its last claim dropped, so skipping A -> B -> A reuses A's download.
const GRACE: Duration = Duration::from_secs(8);
/// A reader stalled this long marks the stream as buffering.
const STALL_BEFORE_BUFFERING: Duration = Duration::from_millis(250);
const RETRIES: u32 = 3;
/// Consecutive producer restarts without new bytes before a started stream is given up on.
const RESUMES: u32 = 6;
const RESUME_BACKOFF: Duration = Duration::from_secs(2);
const RESUME_BACKOFF_MAX: Duration = Duration::from_secs(16);
const FILL_CHUNK: usize = 64 * 1024;
const WAIT_SLICE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Playback: claims the download and steers it with `want`s.
    Play,
    /// Background cache fill: claims the download; a cache hit is not announced.
    Fetch,
    /// An analyzer joining a stream that is already running (or cached): no claim, no `want`s, no `Done` announcement.
    Peek,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamState {
    Connecting,
    Fetching,
    Buffering,
    /// Complete and moving into `MediaCache`; not announced.
    Committing,
    /// Complete and persisted in `MediaCache`.
    Done,
    Failed(String),
    Cancelled,
}

impl StreamState {
    fn is_terminal(&self) -> bool {
        matches!(self, StreamState::Done | StreamState::Failed(_) | StreamState::Cancelled)
    }

    fn is_active(&self) -> bool {
        matches!(self, StreamState::Connecting | StreamState::Fetching | StreamState::Buffering)
    }
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub ranges: Vec<Range<u64>>,
    /// Contiguous bytes from offset 0.
    pub prefix: u64,
    pub len: Option<u64>,
    /// The producer honors `want`s.
    pub jumpable: bool,
    pub state: StreamState,
    /// Contiguous bytes on disk from where the playback reader is; `u64::MAX` once that run reaches the end.
    pub ahead: u64,
    /// The share on disk, 0..=100.
    pub percent: Option<u8>,
}

/// The producer must stop: the stream was cancelled, failed or lost its last claim.
#[derive(Debug)]
pub struct Stopped;

struct State {
    file: Arc<File>,
    path: PathBuf,
    /// The file is a temp file this stream removes when dropped.
    owns_file: bool,
    ranges: Vec<Range<u64>>,
    len: Option<u64>,
    jumpable: bool,
    phase: StreamState,
    want: Option<u64>,
    claims: usize,
    released: Option<Instant>,
    /// A producer without a byte length (HLS) reports its own fraction.
    progress: Option<f32>,
    play_pos: u64,
    /// Why the last producer run ended without finishing; `produce` decides whether to resume.
    failure: Option<String>,
    /// Index of the unit a resumed `Media::Stream` producer continues from.
    checkpoint: usize,
}

impl State {
    fn new(file: File, path: PathBuf, owns_file: bool, phase: StreamState) -> Self {
        Self {
            file: Arc::new(file),
            path,
            owns_file,
            ranges: Vec::new(),
            len: None,
            jumpable: false,
            phase,
            want: None,
            claims: 0,
            released: None,
            progress: None,
            play_pos: 0,
            failure: None,
            checkpoint: 0,
        }
    }
}

struct Shared {
    key: Key,
    state: Mutex<State>,
    cond: Condvar,
    bus: Bus,
    cache: Arc<MediaCache>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        if st.owns_file {
            let _ = std::fs::remove_file(&st.path);
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Moves to `new` unless a terminal state was reached first; announces it.
    fn set_phase(&self, st: &mut State, new: StreamState) -> bool {
        if st.phase == new || st.phase.is_terminal() {
            return false;
        }
        log::debug!("stream {} {}: {:?} -> {:?}", self.key.0, self.key.1, st.phase, new);
        st.phase = new.clone();
        self.cond.notify_all();
        if new != StreamState::Committing {
            self.bus.send(CoreEvent::Stream { key: self.key.clone(), state: new });
        }
        true
    }

    /// Whether the stream is still wanted; also applies the `GRACE` release.
    fn alive(&self) -> bool {
        self.check(&mut self.lock()).is_ok()
    }

    /// Cancels an active stream whose last claim is older than `GRACE`.
    fn check(&self, st: &mut State) -> std::result::Result<(), Stopped> {
        if st.phase.is_active() && st.claims == 0 && st.released.is_some_and(|t| t.elapsed() >= GRACE) {
            self.set_phase(st, StreamState::Cancelled);
        }
        if st.phase.is_terminal() { Err(Stopped) } else { Ok(()) }
    }

    fn info(&self) -> StreamInfo {
        let st = self.lock();
        let downloaded = downloaded(&st.ranges);
        let percent = match st.len {
            Some(0) => None,
            Some(l) => Some((downloaded * 100 / l).min(100) as u8),
            None => st.progress.map(|p| (p * 100.0) as u8),
        };
        StreamInfo {
            prefix: prefix(&st.ranges),
            len: st.len,
            jumpable: st.jumpable,
            state: st.phase.clone(),
            ahead: match run_end(&st.ranges, st.play_pos) {
                Some(end) if st.len.is_some_and(|l| end >= l) => u64::MAX,
                Some(end) => end - st.play_pos,
                None => 0,
            },
            percent,
            ranges: st.ranges.clone(),
        }
    }

    /// A complete stream over an existing file.
    fn complete(key: Key, path: PathBuf, bus: Bus, cache: Arc<MediaCache>, announce: bool) -> io::Result<Arc<Self>> {
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        let st = State { ranges: whole(len), len: Some(len), jumpable: true, ..State::new(file, path, false, StreamState::Done) };
        let shared = Arc::new(Self { key, state: Mutex::new(st), cond: Condvar::new(), bus, cache });
        if announce {
            shared.bus.send(CoreEvent::Stream { key: shared.key.clone(), state: StreamState::Done });
        }
        Ok(shared)
    }
}

/// The ranges of a fully present file of `len` bytes.
fn whole(len: u64) -> Vec<Range<u64>> {
    if len == 0 { Vec::new() } else { std::iter::once(0..len).collect() }
}

fn downloaded(ranges: &[Range<u64>]) -> u64 {
    ranges.iter().map(|r| r.end - r.start).sum()
}

fn prefix(ranges: &[Range<u64>]) -> u64 {
    ranges.first().filter(|r| r.start == 0).map_or(0, |r| r.end)
}

fn insert_range(v: &mut Vec<Range<u64>>, r: Range<u64>) {
    if r.is_empty() {
        return;
    }
    let (mut start, mut end) = (r.start, r.end);
    let mut out = Vec::with_capacity(v.len() + 1);
    let mut placed = false;
    for x in v.drain(..) {
        if x.end < start {
            out.push(x);
        } else if x.start > end {
            if !placed {
                out.push(start..end);
                placed = true;
            }
            out.push(x);
        } else {
            start = start.min(x.start);
            end = end.max(x.end);
        }
    }
    if !placed {
        out.push(start..end);
    }
    *v = out;
}

/// End of the range containing `pos`.
fn run_end(ranges: &[Range<u64>], pos: u64) -> Option<u64> {
    ranges.iter().find(|r| r.start <= pos && pos < r.end).map(|r| r.end)
}

fn covers(ranges: &[Range<u64>], r: Range<u64>) -> bool {
    r.is_empty() || run_end(ranges, r.start).is_some_and(|end| end >= r.end)
}

/// The first missing span of `0..len` ending after `from`, starting no earlier than `from`.
fn hole_from(ranges: &[Range<u64>], len: u64, from: u64) -> Option<Range<u64>> {
    let mut cursor = 0;
    for r in ranges.iter().chain(std::iter::once(&(len..len))) {
        if r.start > cursor && r.start > from {
            return Some(cursor.max(from)..r.start.min(len));
        }
        cursor = cursor.max(r.end);
    }
    None
}

pub struct StreamWriter {
    shared: Arc<Shared>,
    done: bool,
}

impl StreamWriter {
    /// Fails with `Stopped` once the stream was cancelled, failed or released past `GRACE`.
    pub fn write_at(&mut self, offset: u64, chunk: &[u8]) -> std::result::Result<(), Stopped> {
        let file = {
            let mut st = self.shared.lock();
            self.shared.check(&mut st)?;
            st.file.clone()
        };
        if let Err(e) = file.write_all_at(chunk, offset) {
            self.fail_now(format!("write: {e}"));
            return Err(Stopped);
        }
        let mut st = self.shared.lock();
        insert_range(&mut st.ranges, offset..offset + chunk.len() as u64);
        if st.phase == StreamState::Connecting {
            self.shared.set_phase(&mut st, StreamState::Fetching);
        }
        self.shared.cond.notify_all();
        Ok(())
    }

    /// The final length, when the source knows it up front; false if a resumed source disagrees with the earlier one.
    pub fn set_len(&self, n: u64) -> bool {
        let mut st = self.shared.lock();
        if st.len.is_some_and(|l| l != n) {
            return false;
        }
        st.len = Some(n);
        let _ = st.file.set_len(n);
        self.shared.cond.notify_all();
        true
    }

    pub fn set_jumpable(&self, yes: bool) {
        self.shared.lock().jumpable = yes;
    }

    /// Download fraction for a producer with no byte length.
    pub fn set_progress(&self, fraction: f32) {
        self.shared.lock().progress = Some(fraction);
    }

    /// Where a playback reader is stalled, if it is.
    pub fn next_want(&mut self) -> Option<u64> {
        let mut st = self.shared.lock();
        let want = st.want.take()?;
        (!st.ranges.iter().any(|r| r.contains(&want))).then_some(want)
    }

    /// Whether the stream is still wanted; also applies the `GRACE` release.
    pub fn alive(&self) -> bool {
        self.shared.alive()
    }

    /// Where a resumed sequential `Media::Stream` producer continues: (unit index it saved, offset after the bytes written so far).
    pub fn checkpoint(&self) -> (usize, u64) {
        let st = self.shared.lock();
        (st.checkpoint, prefix(&st.ranges))
    }

    pub fn set_checkpoint(&self, index: usize) {
        self.shared.lock().checkpoint = index;
    }

    /// The first missing span at or after `from` (needs `set_len`).
    fn hole_from(&self, from: u64) -> Option<Range<u64>> {
        let st = self.shared.lock();
        hole_from(&st.ranges, st.len?, from)
    }

    /// Persists into `MediaCache` and announces `Done`, or fails if bytes are missing.
    pub fn finish(mut self) {
        self.done = true;
        let path = {
            let mut st = self.shared.lock();
            if st.phase.is_terminal() {
                return;
            }
            let end = st.ranges.last().map_or(0, |r| r.end);
            let len = st.len.unwrap_or(end);
            // An empty source is a failure, never a cache hit.
            if st.ranges.len() != 1 || st.ranges[0] != (0..len) {
                let why = if len == 0 { "empty".to_string() } else { format!("incomplete: {} of {len} bytes", downloaded(&st.ranges)) };
                self.shared.set_phase(&mut st, StreamState::Failed(why));
                return;
            }
            st.len = Some(len);
            self.shared.set_phase(&mut st, StreamState::Committing);
            st.path.clone()
        };
        let persisted = self.shared.cache.persist_file(&self.shared.key.0, &self.shared.key.1, &path);
        let mut st = self.shared.lock();
        match persisted {
            Ok(dest) => {
                st.path = dest;
                st.owns_file = false;
            }
            Err(e) => log::warn!("stream {} {}: couldn't persist into media cache: {e}", self.shared.key.0, self.shared.key.1),
        }
        self.shared.set_phase(&mut st, StreamState::Done);
    }

    /// This run of the producer ended early; a started download is resumed, else the stream fails.
    pub fn fail(self, why: String) {
        self.shared.lock().failure = Some(why);
    }

    fn fail_now(&self, why: String) {
        let mut st = self.shared.lock();
        self.shared.set_phase(&mut st, StreamState::Failed(why));
    }
}

impl Drop for StreamWriter {
    fn drop(&mut self) {
        if !self.done {
            self.shared.lock().failure.get_or_insert_with(|| "producer ended early".into());
        }
    }
}

fn sleep_while_alive(shared: &Shared, until: Instant) {
    while Instant::now() < until && shared.alive() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs `f`, retrying a failure 3 times with backoff while the stream is still wanted.
pub fn retry<T>(writer: &StreamWriter, what: &str, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 0;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if attempt >= RETRIES || !writer.alive() => return Err(e),
            Err(e) => {
                attempt += 1;
                log::debug!("stream: {what} failed (retry {attempt}/{RETRIES}): {e}");
                let until = Instant::now() + Duration::from_millis(500 << (attempt - 1));
                sleep_while_alive(&writer.shared, until);
            }
        }
    }
}

/// Fills `writer` from `src`: from 0, jumping where a playback reader is stalled, then backfilling the gaps.
/// A source that cannot report its length is copied in order instead.
pub fn fill_from_seekable<R: Read + Seek>(mut src: R, mut writer: StreamWriter) {
    let Ok(len) = src.seek(SeekFrom::End(0)) else {
        return fill_in_order(src, writer);
    };
    if !writer.set_len(len) {
        return writer.fail(format!("source length changed to {len}"));
    }
    writer.set_jumpable(true);
    let mut buf = vec![0u8; FILL_CHUNK];
    let mut pos = 0;
    loop {
        if let Some(want) = writer.next_want() {
            pos = want;
        }
        let Some(hole) = writer.hole_from(pos).or_else(|| writer.hole_from(0)) else { break };
        let at = hole.start;
        let n = buf.len().min((hole.end - at) as usize);
        let read = src.seek(SeekFrom::Start(at)).and_then(|_| src.read(&mut buf[..n]));
        match read {
            Ok(0) => return writer.fail(format!("source ended at {at} of {len} bytes")),
            Ok(k) => {
                if writer.write_at(at, &buf[..k]).is_err() {
                    return;
                }
                pos = at + k as u64;
            }
            Err(e) => return writer.fail(format!("read at {at}: {e}")),
        }
    }
    writer.finish();
}

fn fill_in_order<R: Read>(mut src: R, mut writer: StreamWriter) {
    let mut buf = vec![0u8; FILL_CHUNK];
    let mut at = 0u64;
    loop {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                if writer.write_at(at, &buf[..k]).is_err() {
                    return;
                }
                at += k as u64;
            }
            Err(e) => return writer.fail(format!("read at {at}: {e}")),
        }
    }
    writer.finish();
}

/// `Read + Seek` over a stream. Reads inside a filled range never wait; a read at a hole waits for data
/// (and, for a playback reader, asks the producer to jump there). Errors are never `Interrupted`.
pub struct StreamReader {
    shared: Arc<Shared>,
    pos: u64,
    play: bool,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut stalled_since: Option<Instant> = None;
        let mut marked = false;
        let mut st = self.shared.lock();
        if self.play {
            st.play_pos = self.pos;
        }
        loop {
            if st.len.is_some_and(|l| self.pos >= l) {
                return Ok(0);
            }
            if let Some(end) = run_end(&st.ranges, self.pos) {
                if marked && st.phase == StreamState::Buffering {
                    self.shared.set_phase(&mut st, StreamState::Fetching);
                }
                let n = (end - self.pos).min(buf.len() as u64) as usize;
                let file = st.file.clone();
                drop(st);
                let n = file.read_at(&mut buf[..n], self.pos)?;
                self.pos += n as u64;
                return Ok(n);
            }
            match &st.phase {
                StreamState::Done => return Ok(0),
                StreamState::Failed(why) => return Err(io::Error::other(why.clone())),
                StreamState::Cancelled => return Err(io::Error::other("stream cancelled")),
                _ => {}
            }
            if self.play && st.jumpable {
                st.want = Some(self.pos);
            }
            let since = *stalled_since.get_or_insert_with(Instant::now);
            if !marked && since.elapsed() >= STALL_BEFORE_BUFFERING && st.phase == StreamState::Fetching {
                marked = self.shared.set_phase(&mut st, StreamState::Buffering);
            }
            st = self.shared.cond.wait_timeout(st, WAIT_SLICE).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                let len = self.shared.lock().len.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "stream length unknown"))?;
                len.checked_add_signed(d)
            }
        };
        self.pos = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

#[derive(Clone)]
pub struct StreamHandle {
    shared: Arc<Shared>,
    intent: Intent,
}

impl StreamHandle {
    pub fn same_stream(&self, other: &StreamHandle) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StreamHandle").field(&self.shared.key).finish()
    }
}

impl StreamHandle {
    /// An independent cursor. It never keeps the download alive; only a `Claim` does.
    pub fn reader(&self) -> StreamReader {
        StreamReader { shared: self.shared.clone(), pos: 0, play: self.intent == Intent::Play }
    }

    pub fn info(&self) -> StreamInfo {
        self.shared.info()
    }

    pub fn key(&self) -> &Key {
        &self.shared.key
    }

    /// Blocks until `r` (clipped to the length, once known) is on disk or the stream is `Done`.
    /// False on timeout, failure or cancel. For load and analyzer threads only.
    pub fn wait_range(&self, r: Range<u64>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            let end = st.len.map_or(r.end, |l| r.end.min(l));
            if st.phase == StreamState::Done || covers(&st.ranges, r.start..end) {
                return true;
            }
            if matches!(st.phase, StreamState::Failed(_) | StreamState::Cancelled) {
                return false;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return false };
            st = self.shared.cond.wait_timeout(st, left.min(WAIT_SLICE)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Whether the stream failed or was cancelled; cheap, for a decode loop to poll.
    pub fn stopped(&self) -> bool {
        matches!(self.shared.lock().phase, StreamState::Failed(_) | StreamState::Cancelled)
    }

    /// Stops the download now; every waiting reader wakes with an error.
    pub fn cancel(&self) {
        let mut st = self.shared.lock();
        if st.phase.is_active() {
            self.shared.set_phase(&mut st, StreamState::Cancelled);
        }
    }
}

/// Keeps a download wanted while alive; when the last one drops the producer stops after `GRACE`.
pub struct Claim(Option<Arc<Shared>>);

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(shared) = &self.0 {
            let mut st = shared.lock();
            st.claims -= 1;
            if st.claims == 0 {
                st.released = Some(Instant::now());
            }
        }
    }
}

#[derive(Clone)]
pub struct StreamEngine {
    media: SharedMedia,
    cache: Arc<MediaCache>,
    bus: Bus,
    running: Arc<Mutex<HashMap<Key, Weak<Shared>>>>,
}

/// Below 32 kbps-equivalent for a track over 35 s: a cut-off preview, not a real file.
fn truncated(path: &Path, duration_ms: u32) -> bool {
    const MIN_DURATION_MS: u32 = 35_000;
    const MIN_BYTES_PER_SEC: u64 = 4_000;
    duration_ms > MIN_DURATION_MS
        && std::fs::metadata(path).is_ok_and(|m| m.len() < u64::from(duration_ms) / 1000 * MIN_BYTES_PER_SEC)
}

impl StreamEngine {
    pub fn new(media: SharedMedia, cache: Arc<MediaCache>, bus: Bus) -> Self {
        Self { media, cache, bus, running: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Returns at once: a cache hit or local file is complete, anything else is `Connecting` while a
    /// core thread runs the provider's `open` and fetches. A running download of the same key is shared.
    pub fn open(&self, r: &Rendition, intent: Intent) -> Result<(StreamHandle, Claim)> {
        let key: Key = (r.source.clone(), r.uri.clone());
        if let Some(found) = self.attach_running(&key, intent) {
            return Ok(found);
        }
        let media = self.media.snapshot();
        let local = crate::resolver::is_local_source(&r.source) && !media.contains_key(&r.source);
        let cached = self.cache.cached_path(&r.source, &r.uri).filter(|p| !truncated(p, r.duration_ms)).or_else(|| local.then(|| PathBuf::from(crate::resolver::local_path_from_uri(&r.uri))));
        if let Some(path) = cached {
            let shared = Shared::complete(key, path, self.bus.clone(), self.cache.clone(), intent == Intent::Play).map_err(|e| Error::Other(format!("open {}: {e}", r.uri)))?;
            return Ok((StreamHandle { shared, intent }, Claim(None)));
        }
        if intent == Intent::Peek {
            return Err(Error::NotFound);
        }
        let provider = media
            .get(&r.source)
            .or_else(|| media.get(&SourceId::from("http")))
            .cloned()
            .ok_or_else(|| Error::NoSource(r.uri.clone()))?;
        let (file, path) = self.cache.new_stream_file().map_err(|e| Error::Other(format!("stream file: {e}")))?;
        let state = State { claims: 1, ..State::new(file, path, true, StreamState::Connecting) };
        let shared = Arc::new(Shared { key: key.clone(), state: Mutex::new(state), cond: Condvar::new(), bus: self.bus.clone(), cache: self.cache.clone() });
        {
            let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
            running.retain(|_, w| w.strong_count() > 0);
            if let Some(existing) = running.get(&key).and_then(Weak::upgrade).and_then(|s| attach(s, intent)) {
                return Ok(existing);
            }
            running.insert(key.clone(), Arc::downgrade(&shared));
        }
        shared.bus.send(CoreEvent::Stream { key, state: StreamState::Connecting });
        let claim = Claim(Some(shared.clone()));
        let handle = StreamHandle { shared: shared.clone(), intent };
        let running = self.running.clone();
        let rendition = r.clone();
        std::thread::Builder::new()
            .name("stream-fetch".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| produce(&shared, provider.as_ref(), &rendition))).is_err() {
                    let mut st = shared.lock();
                    shared.set_phase(&mut st, StreamState::Failed("stream thread panicked".into()));
                }
                let mut running = running.lock().unwrap_or_else(|e| e.into_inner());
                if running.get(&shared.key).is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&shared))) {
                    running.remove(&shared.key);
                }
            })
            .map_err(|e| Error::Other(format!("spawn stream thread: {e}")))?;
        Ok((handle, claim))
    }

    /// Whether a download of `key` is running right now (something a `Peek` could join).
    pub fn is_running(&self, key: &Key) -> bool {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).get(key).is_some_and(|w| w.strong_count() > 0)
    }

    fn attach_running(&self, key: &Key, intent: Intent) -> Option<(StreamHandle, Claim)> {
        let shared = self.running.lock().unwrap_or_else(|e| e.into_inner()).get(key).and_then(Weak::upgrade)?;
        attach(shared, intent)
    }
}

/// Joins a running stream; `None` if it already failed or was cancelled.
fn attach(shared: Arc<Shared>, intent: Intent) -> Option<(StreamHandle, Claim)> {
    let claim = {
        let mut st = shared.lock();
        if matches!(st.phase, StreamState::Failed(_) | StreamState::Cancelled) {
            return None;
        }
        if intent == Intent::Peek {
            Claim(None)
        } else {
            st.claims += 1;
            st.released = None;
            Claim(Some(shared.clone()))
        }
    };
    Some((StreamHandle { shared, intent }, claim))
}

/// The stream thread: runs the provider, and while the download had started and is still wanted, re-runs
/// it after a failure with backoff; the sparse file keeps its ranges, so only the gaps are refetched.
fn produce(shared: &Arc<Shared>, provider: &dyn MediaProvider, r: &Rendition) {
    let (mut failures, mut have, mut kind) = (0, 0, None);
    loop {
        let writer = StreamWriter { shared: shared.clone(), done: false };
        if !writer.alive() {
            return;
        }
        run_once(shared, provider, r, writer, &mut kind);
        let mut st = shared.lock();
        if st.phase.is_terminal() {
            return;
        }
        let why = st.failure.take().unwrap_or_default();
        let got = downloaded(&st.ranges);
        if got > have {
            (have, failures) = (got, 0);
        }
        failures += 1;
        if have == 0 || failures > RESUMES {
            shared.set_phase(&mut st, StreamState::Failed(why));
            return;
        }
        let wait = (RESUME_BACKOFF * 2u32.pow(failures - 1)).min(RESUME_BACKOFF_MAX);
        log::debug!("stream {} {}: {why}; resuming in {wait:?} ({failures}/{RESUMES}, {have} bytes on disk)", shared.key.0, shared.key.1);
        shared.set_phase(&mut st, StreamState::Buffering);
        drop(st);
        sleep_while_alive(shared, Instant::now() + wait);
    }
}

/// One run: asks the provider, then runs what it returned; a resume must get the same kind of media as the first run.
fn run_once(shared: &Arc<Shared>, provider: &dyn MediaProvider, r: &Rendition, writer: StreamWriter, kind: &mut Option<Discriminant<Media>>) {
    let media = match provider.open(r, &|| writer.alive()) {
        Ok(m) => m,
        Err(e) => return writer.fail(format!("open: {e}")),
    };
    if *kind.get_or_insert(std::mem::discriminant(&media)) != std::mem::discriminant(&media) {
        return writer.fail("the source changed media kind".into());
    }
    let fetching = || shared.set_phase(&mut shared.lock(), StreamState::Fetching);
    match media {
        Media::Path(p) => {
            adopt_local(shared, writer, p);
        }
        Media::Url(url) => match RangeReader::open(&url, HttpOptions::default(), writer.hole_from(0).map_or(0, |h| h.start)) {
            Ok(reader) => {
                fetching();
                fill_from_seekable(reader, writer);
            }
            Err(e) => writer.fail(format!("open {url}: {e}")),
        },
        Media::Stream(run) => {
            fetching();
            run(writer);
        }
    }
}

/// A provider handed over a finished local file: read it in place, link it into the cache, done.
fn adopt_local(shared: &Arc<Shared>, mut writer: StreamWriter, path: PathBuf) {
    let opened = File::open(&path).and_then(|f| f.metadata().map(|m| (f, m.len())));
    let (file, len) = match opened {
        Ok(v) => v,
        Err(e) => return writer.fail(format!("open {}: {e}", path.display())),
    };
    if let Err(e) = shared.cache.link_local(&shared.key.0, &shared.key.1, &path) {
        log::debug!("stream {} {}: couldn't link into media cache: {e}", shared.key.0, shared.key.1);
    }
    writer.done = true;
    let mut st = shared.lock();
    if st.phase.is_terminal() {
        return;
    }
    let old = std::mem::replace(&mut st.path, path);
    if st.owns_file {
        let _ = std::fs::remove_file(old);
    }
    st.owns_file = false;
    st.file = Arc::new(file);
    st.ranges = whole(len);
    st.len = Some(len);
    st.jumpable = true;
    shared.set_phase(&mut st, StreamState::Done);
}
