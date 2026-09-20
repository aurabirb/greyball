# Streaming playback (start before the download finishes)

Brief for agents. Read `AGENTS.md` first. Three stages, one agent each, in order. Line numbers are
approximate; function names are the anchor. The Rust here is a sketch of shape, not final code.
Refactors, trait changes and new event types are approved; legacy code is deleted, not kept as a
fallback (this is a pre-release project). A design review has been folded in.

## Motivation: what the user gets (focus on this)
The internals below serve these user-visible features. When a trade-off appears, pick the option
that serves them, not the more elegant internal.
- **Instant playback.** Pressing play on a track that is not cached (a multi-hour SoundCloud set,
  an HTTP file, a Spotify track) starts sound within seconds, not after the whole download.
- **Scrubbing anywhere.** Jumping around a track works while it is still loading wherever the source
  allows it (progressive MP3, Spotify), and never breaks playback where it does not (HLS until done).
- **Listening and analysis at the same time.** The BPM column, the waveform overview and the
  "similar tracks" feature (see `TODO.md`) fill in while a track plays or downloads, from the same
  download, so they cost no extra bandwidth.
- **Visible progress.** The user can tell "buffering", "downloading" and "failed" apart instead of
  staring at a frozen player or a silent skip.
- **Skipping is free.** Mashing next, skipping mid-download or replaying the same track never
  wastes bandwidth, never leaves partial files behind, and never freezes the UI.
- **Nothing gets worse.** Everything that works today keeps working: Spotify reconnect and resume,
  scrubbing, cache-only scans, scan analysis of cached files.
- **New sources are cheap.** A future source (e.g. YouTube) only writes a small provider; the
  simplest returns a CDN link (`Media::Url`) and playback, scrubbing, caching, analysis and status
  come for free. A source with special needs returns a `Reader` or a `Stream` instead.
Priorities for agents, in order: perceived latency and never freezing the UI; clean abandon (no
files, threads or closures left); playback, scrub and analysis working from one download; small,
consistent API. Do not spend effort on things no feature above needs.

## Goals
- Playback starts after the first few seconds have arrived, for every source.
- Scrubbing keeps working, especially on non-HLS tracks (progressive MP3, Spotify Ogg), and
  becomes available earlier than "after the whole download".
- Playback and analysis both work during the download, from one fetch, not two.
- Player and analyzer code is source-agnostic; every source exposes the same surface
  (`MediaProvider::open`). No `Player::open_for_scan`, no per-source players.
- Network blocking lives inside the stream module, is cancellable, and never blocks the UI, the
  `Session` lock or the cache. Skip/stop/replace abandons a download with no leftover files,
  threads or closures; replaying the same track reuses the download.
- Downloading and buffering are visible to callers (UI, scan, others) as status plus transitions.
- Lose no playback functionality that exists today; gain new. YAGNI for the rest (no mixer
  volume; gapless comes back later as a generic "preload" built on this mechanism).

## Where things are today (verify)
- `core/src/traits.rs`: `Media { Path, Url, Reader }`; `MediaProvider::{open, materialize}`; the
  `Player` trait (`accepts`, `load`, `preload`, `seek`, `open_for_scan`, `scan_fetch_paused`, ...).
- `player/src/rodio_player.rs`: `start_load` -> `resolve_and_decode` (background thread) ->
  `open_media`; `open_streaming_url` + `stream_body_to_file` + `StreamState` + `StreamingReader`
  (growing tempfile + waiting reader, `Content-Length` only, sequential fill); `cache_fetched`
  -> `MediaCache::put_file` (COPIES, does not rename); `LoadedTrack.tapped: Decoder<StreamingReader>`;
  `Cmd::Seek` updates the position only if `try_seek` succeeds; the worker `tick` (~500 ms) sends
  `Finished` when the sink empties; `finish_load` sets the snapshot duration from the decoder;
  `player/src/fmp4.rs::duration_patch` scans every top-level atom (waits for the whole file).
- `sources/soundcloud/src/client.rs`: `open_hls` fetches every segment serially (`fetch_into`),
  `tmp.keep()`s (also leaks a file per scan prefetch), returns `Media::Path`; `parse_hls_playlist`
  ignores `#EXTINF`; `Rendition.duration_ms` already carries the duration.
- Spotify (`sources/spotify/src/`): not a `MediaProvider` (`source.rs` header). Playback is
  librespot's `Player` (`player.rs`: `SpotifyPlayer`, `run`, `do_load`, `Link`, `Loaded::resume`,
  `TappedSink`, `spawn_materialize_to_cache`); scan audio is `Player::open_for_scan` ->
  `scan_audio::fetch_scan_audio` (async; its `Full` reader drains the file in `Drop`; prefers the
  cached or smallest bitrate); `AudioFile` is randomly seekable with a known length.
- Scan: `core/src/scan.rs::open_scan_audio` (cache -> `materialize` -> `Player::open_for_scan`),
  `ScanFetchMode::{Full, Partial, CacheOnly}`, `ScanDriver::prioritize`; `core/src/audio_decode.rs::
  {decode_blocks, open_analysis_audio}` (`decode_blocks` builds a seekable decoder and seeks to the
  end first; `open_analysis_audio` has a `read_all` fallback).
- `core/src/event.rs`: `CoreEvent`, `PlayerEvent`; the bus is unbounded and its one consumer
  locks the `Session` per drain; `Session::on_event` (`core/src/app.rs`) calls `touch()` for every
  event except `PlayerEvent::Progress` and `BackgroundFailure`.
- Decoder facts (rodio 0.21.1, symphonia 0.5.5): `Decoder::builder().with_data(r)` without
  `with_byte_len` is non-seekable and starts from the first fragment (needs ftyp+moov first); with a
  byte length isomp4 reads every atom up front and Ogg seeks to the last page (one range request on
  a random-access stream, a full wait on a growing file). A zero `mdhd` duration becomes
  `total_duration = Some(0)` and rodio clamps every seek to 0. A failed backward seek on a
  non-seekable stream surfaces later in `next_packet` and ends the track; a reader-level seek error
  poisons the decoder. `rodio::Decoder` needs `Read + Seek + Send + Sync`.

## Design

### One stream = a sparse file with known ranges
Bytes arrive at any offset, so sequential download, a jump for a scrub, and later backfill are the
same mechanism. Nothing is classified "seekable" or "not"; the reader asks, the producer may
answer late, and a stalled wait is visible as buffering.

```rust
// core/src/stream.rs (sketch)
#[derive(Clone)]
pub struct StreamHandle(Arc<Shared>);   // Shared = Mutex<State> + Condvar; owns the tempfile,
                                        // removed when the last Arc drops (not on cancel)
impl StreamHandle {
    pub fn reader(&self) -> StreamReader;     // Read + Seek + Send + Sync; never keeps the download alive
    pub fn info(&self) -> StreamInfo;         // cheap snapshot, no waiting
    pub fn wait_range(&self, r: Range<u64>, timeout: Duration) -> bool;  // load / analyzer threads only
    pub fn cancel(&self);                     // wakes every waiter; reads then fail (never `Interrupted`)
}
pub struct StreamInfo {
    pub ranges: Vec<Range<u64>>,              // what is on disk
    pub prefix: u64,                          // contiguous bytes from offset 0 (what in-order consumers need)
    pub len: Option<u64>,
    pub jumpable: bool,                       // the producer honors `want`s (may turn true later)
    pub state: Connecting | Fetching | Buffering | Done | Failed(String) | Cancelled,
}

pub struct StreamWriter { /* Arc<Shared> */ }
impl StreamWriter {
    pub fn write_at(&mut self, offset: u64, chunk: &[u8]) -> Result<(), Cancelled>;
    pub fn set_len(&self, n: u64);            // also preallocates (sparse)
    pub fn set_jumpable(&self, yes: bool);
    pub fn next_want(&mut self) -> Option<u64>; // where a reader is stalled; poll between chunks
    pub fn finish(self);   pub fn fail(self, why: String);
}
```
- `StreamReader::read` at a missing offset records a `want`, waits on the condvar (wakes on data,
  cancel, fail), and returns what is available; reads inside a filled range never wait. `seek` is
  plain positioning (`End` needs a known length). Only the playback reader (`Intent::Play`) posts
  `want`s; a `Fetch` or `Peek` (analyzer) reader that hits a hole just waits for the range, so it
  can never pull the producer away from where the listener is.
- Helper in core, used by anything that is a `Read + Seek`: `fill_from_seekable(src, writer)` —
  fills from 0, jumps when `next_want()` says so, then backfills the gaps, done when the ranges
  cover `len`. Spotify's `AudioFile` and the shared HTTP reader below both use it. HLS writes
  sequentially with `write_at` and does not set `jumpable`.
- Shared HTTP helper: `core::http::RangeReader::open(url, options)`, a `Read + Seek` over `Range`
  GETs of <=256 KiB, so a cancel takes effect within one request, with sequential fill when the
  server has no `Accept-Ranges`. `options` carries what a source may need (headers, client,
  retry/refresh hook for signed URLs); `Default` is what a plain CDN link needs. It is the one HTTP
  implementation: the engine uses it for `Media::Url`, and a source with special needs imports it
  to build its own `Media::Reader`. Built by generalizing `core/src/http_fetch.rs` (`start_get`, the
  shared client) and the player's `stream_body_to_file`; the whole-body helpers are deleted once
  nothing uses them.
- Decoder mode follows the handle: `len` known and `jumpable`, or `Done` -> built with a byte
  length (seekable, as today); otherwise built without (non-seekable, starts from the first
  fragment/frame). Scrub on a jumpable stream: the decoder seeks, the reader stalls on the gap,
  `want` is served, the state shows `Buffering`. On a non-jumpable, unfinished stream the player
  ignores `Cmd::Seek` (silent, one debug line) instead of asking the decoder (which can poison).

### Engine: one entry point, dedupe, intents, cancel
```rust
pub struct StreamEngine { /* HashMap<(SourceId,String), Weak<Shared>> + MediaCache + media map */ }
impl StreamEngine {
    pub fn open(&self, r: &Rendition, intent: Intent) -> Result<(StreamHandle, Claim)>; // never blocks the caller for network
    pub fn status(&self, key: &Key) -> Option<StreamInfo>;
}
pub enum Intent { Play, Fetch, Peek }   // Play: grace period after release; Fetch: runs to Done (background
                                        // cache fill); Peek: no claim, an analyzer reading a running stream
```
- Already in `MediaCache` -> a complete stream over the cached file; no fetch.
- Same `(source, uri)` running -> the same handle (a replay, or an analyzer, attaches; no second
  fetch — this is what Spotify's scan code avoided). The registry lock is held only for the map
  lookup, never during a provider `open` or a fetch.
- `Claim` is RAII. When the last `Play`/`Fetch` claim drops, the producer stops after `GRACE` (one
  constant) unless re-claimed; it checks between chunks, so there is no timer thread. `Peek`
  readers hold `Weak` only, so an analyzer can never keep a skipped download alive.
- Providers stay simple: `open(&Rendition) -> Media` where
  `Media { Path(PathBuf), Url(String), Reader(Box<dyn ReadSeek + Send>), Stream(Box<dyn FnOnce(StreamWriter) + Send>) }`.
  Four ways to hand over audio, cheapest first, so simple sources stay minimal and reuse the
  default machinery:
  - `Url`: "here is a CDN link, you play it" — the engine opens it with a default `RangeReader` and
    `fill_from_seekable`. The HTTP source and SoundCloud's progressive path stay exactly as they
    are today.
  - `Path`: an existing local file (complete stream, nothing to fetch).
  - `Reader`: a `Read + Seek` the source built itself (Spotify's `AudioFile`; an HTTP source that
    needs custom headers or URL refresh builds its own `RangeReader`) — engine runs
    `fill_from_seekable`.
  - `Stream`: a closure that appends pieces itself (HLS segments), run on a core-owned thread.
  Provider `open` does its cheap setup synchronously (HLS: resolve + parse the playlist), so setup errors return
  `Err` (SoundCloud then falls back to its progressive transcoding, as today), and it may return a
  retryable error that the engine retries with backoff until the user's claim is gone.
- Cache: tempfiles live in the cache directory. New `MediaCache::persist_file` (rename into place,
  sniff the extension, take the per-key lock) is called on `Done` by the producer thread only (it
  writes the redb index, so never from the UI or audio thread). One `Fetching -> Committing`
  transition under the `Shared` lock makes cancel-vs-commit race-free. A startup sweep removes stray
  temp files left by a crash.
- Failed chunk/segment: retry 3 times with backoff, then `fail`.

### What callers see
- `StreamInfo` (above) is a cheap snapshot: state, ranges, length, jumpable. Progress is read live
  (like `PlayerEvent::Progress`), never pushed per chunk.
- One coarse `CoreEvent::Stream { key, state }` on state transitions only (connecting, fetching,
  buffering on/off, done, failed, cancelled), excluded from `touch()`. `Done` means "persisted in
  `MediaCache`" (also sent when a stream opens from an existing cache hit) and replaces
  `PlayerEvent::Materialized`: the engine sends it, and `Session::on_player_event`'s prioritize hook
  (`core/src/app.rs`) moves to the `Stream` handler.
- `MediaProvider::materialize` (whole-body fetch for the scan prefetch) is replaced by
  `engine.open(r, Intent::Fetch)`: the caller gets a handle and a `Claim`, reads through
  `handle.reader()` (or waits for `Done`), and the cache fill happens by itself on `Done`.

#### Event sequences
Typical first play of an uncached track (existing events marked *):
1. `PlayerEvent::Loading`* -> `Stream{Connecting}` -> provider `open` -> `Stream{Fetching}`.
2. Start threshold reached -> decoder built -> `PlayerEvent::Playing`* (Session calls `prioritize`*).
3. While playing: `PlayerEvent::Progress`* per tick; `Stream{Buffering}` then `Stream{Fetching}`
   only on an underrun (player status shows/clears "buffering"); retries are debug logs only.
4. All bytes in -> producer persists to `MediaCache` -> `Stream{Done}` (analysis and the UI react).
5. End: `PlayerEvent::Finished`*; skip/stop: claim dropped, `Stream{Cancelled}` after `GRACE` if not
   done, tempfile removed, `PlayerEvent::Stopped`* / the next `Loading`*.
6. A permanent failure: `Stream{Failed}` + `PlayerEvent::LoadFailed`* (or `BackgroundFailure`*).
Cached track: `Loading`* -> `Stream{Done}` -> `Playing`*, no fetch.

Analysis of the playing track (BPM): `Playing`* -> `ScanDriver::prioritize`* -> plugin `needs` ->
`engine.open(r, Peek)` returns the running stream's reader (no second fetch) -> the plugin decodes
progressively, waiting on `wait_range`, no events while it works -> `Catalog::patch` ->
`CoreEvent::TrackUpdated`* (UI shows the bpm). A stream that is cancelled or fails under a reader
makes the plugin return `Retry` (never the permanent `Skip`) so it is tried again later.
Background walk of an uncached track: `engine.open(r, Fetch)` -> `Stream{Connecting, Fetching}` ->
analysis during download as above -> `Stream{Done}` (cache filled) -> `TrackUpdated`*. The UI can
show a "downloading" mark from `engine.status`.
- `RodioPlayer`'s tick folds buffering/download state into the player status it already publishes
  (add the field; `PlayerStatus`/`PlayerEvent` may change), and `ui/src/view/status_line.rs` shows
  "buffering" / percent. A row "downloading" mark can follow later from `engine.status`.
- The scan driver reacts to transitions and reads ranges; nothing polls the network.

### Player (`player/src/rodio_player.rs`)
- `open_media` -> `engine.open(r, Intent::Play)`; load thread: `wait_range(0..START_BYTES)` (or
  `Done`), then build the decoder from `handle.reader()` (mode per above). `LoadedTrack.tapped`
  becomes `Decoder<StreamReader>`; delete `StreamingReader`, `StreamState`, `open_streaming_url`,
  `stream_body_to_file`, `cache_fetched`.
- Duration: `Rendition.duration_ms` when the decoder reports 0/`None`; `duration_patch` only when
  the stream is `Done`.
- `tick`: `Buffering` when the reader is stalled (or `ranges` ahead of the decode position fall
  under `LOW_WATER`): pause with a "buffering" state and resume at the start threshold; `Failed`
  -> `LoadFailed`/`BackgroundFailure`, not `Finished`.
- Stop/Load of another key drops the claim (grace). Same key reuses the handle.
- A read may still block on the audio thread; the start threshold, low-water pause and retries make
  that rare. A decode-ahead ring buffer thread is deliberately deferred.

### Analyzers
- `open_scan_audio` collapses to `engine.open(r, Intent::Fetch|Peek)`; the reader is an independent
  cursor over the same file. `decode_blocks`/`SourceAdapter` get a progressive, non-seekable
  variant that waits on `wait_range` for the next bytes; `open_analysis_audio` loses the
  `read_all` fallback. BPM (60 s prefix) starts as soon as the prefix exists; the waveform consumes
  progressively (the "progressive waveform" TODO item). `Player::open_for_scan`,
  `scan_fetch_paused` and `MediaProvider::materialize` are removed.
- Analyzers consume the contiguous prefix (`StreamInfo.prefix`) as it grows. After a scrub jump the
  file has islands beyond the gap; an in-order analyzer simply waits at the gap until the backfill
  closes it. Analyzing arbitrary chunks as they land (out of order) is out of scope: a decoder needs
  framing and setup state (mp3/Ogg resync at frame/page boundaries, fMP4 needs the init segment plus
  a whole fragment, known to the source), so it would need format-aware chunk decoding in the
  analyzer layer plus a plugin-facing "new chunk" API. The `ranges` list and independent reader
  cursors are the extension point; nothing in the stream layer would change.

### Spotify becomes a normal `MediaProvider` (deleting `SpotifyPlayer`)
- `SpotifyMediaProvider::open` returns `Media::Reader` over the decrypted, header-skipped Ogg
  (`AudioFile`: random-access, known length -> scrubs during download via `fill_from_seekable`),
  at the best bitrate (320 needs Premium; fall back down), never the drain-on-drop reader.
- It waits (cancellably, internally) for a live session instead of returning `Unsupported`.
- The plugin keeps the connection: `Link` (session, backoff, `generation`, `died_streak`,
  `SESSION_HEALTHY_AFTER`, the runtime `Handle`) is unchanged and is the provider's session source.
- Carry-over checklist (do not lose these behaviors):
  - Held load: a load requested while the AP link is down waits, then starts -> provider waits for
    a live session, cancelled by the claim.
  - Generation retry: an `Unavailable` after the link changed under a load is retried, not
    reported as `LoadFailed` -> retryable provider error while `link.generation` moved.
  - Wedged session: 2 consecutive load failures -> `session.shutdown()` recycle -> the provider
    reports open failures to `Link`.
  - Player death / resume (`Loaded::resume`: reload the same track at the current position, keeping
    paused state) -> generic "resume after failure": the engine reopens the key (the same tempfile
    keeps its ranges, the producer refills gaps), the player rebuilds the decoder at its position.
    Write this as its own TODO item after the implementation; it also fixes network drops for
    every source.
  - The playing track surviving an AP death (audio comes from the CDN; verify `AudioFile` reads
    continue after the session dies, e.g. via the local proxy used before).
  - Preload (`Cmd::Preload`, `TimeToPreloadNextTrack` -> `PlayerEvent::PreloadHint`, gapless):
    later generic feature — `engine.open(next, Play)` early; `RodioPlayer` keeps `Player::preload`.
  - Events (Playing/Paused/Stopped/Finished/LoadFailed, position, `Stop` ack race) come from the
    rodio path; volume is the rodio sink volume; the visualizer tap is rodio's `Tapped`;
    `spawn_materialize_to_cache`/`is_materialized` are replaced by the engine's cache-on-`Done`.

## Before and after: worked examples
"Old" is how the code works today (from reading it; verify anything you rely on). "New" is this
design.

### 1. Play an uncached progressive MP3 (HTTP source, SoundCloud progressive)
- Old: the provider returns `Media::Url`; `open_media` -> `open_streaming_url` calls
  `core::start_get`. With a `Content-Length` it preallocates a tempfile, spawns
  `stream_body_to_file`, and returns a `StreamingReader`; the decoder is built with the byte length.
  Without a `Content-Length` the whole body downloads first. Bytes arrive strictly in order. On
  completion `cache_fetched` copies the file into `MediaCache` (`put_file`) and `Materialized` fires.
- New: the provider still returns `Media::Url(link)` (no source code change). `engine.open(r,
  Play)` opens it with the default `RangeReader` and runs `fill_from_seekable` on a core thread;
  the load thread waits for the first bytes and builds the decoder; on `Done` the file is renamed
  into `MediaCache` and `Stream{Done}` fires. No `Content-Length` needed for the range mode.

### 2. Scrub in that MP3 while it is still downloading
- Old: `try_seek` succeeds; the next `read` blocks on the condvar until the sequential download
  reaches that offset. Scrubbing to 90% of a 60 MB file waits for the first 90%.
- New: the decoder seeks, the reader stalls on the gap and records a `want`; the producer jumps
  there with a `Range` request, the state shows `Buffering`, playback resumes as soon as that
  region is on disk; the rest backfills afterwards.

### 3. Play an uncached SoundCloud HLS track (a long set)
- Old: `open_hls` fetches the playlist, then the init segment and every media segment one blocking
  request at a time, appends to a tempfile, `keep()`s it, and returns `Media::Path`; `open_media`
  `link_local`s it. Sound starts only after the last segment; a multi-hour set waits for hundreds
  of requests. The header duration is 0 until `duration_patch` runs on the finished file.
- New: `open_hls` parses the playlist synchronously and returns `Media::Stream`; the closure appends
  segments as they arrive. Sound starts after the first segment; the decoder is non-seekable while
  the stream is unfinished; the duration comes from `Rendition.duration_ms`; a scrub click is
  ignored until `Done`, after which the file (renamed into the cache) seeks with `duration_patch`.
  No stray `/tmp` file.

### 4. Skip to the next track mid-download
- Old (verify): the sink is dropped, but the download thread has no cancel path and runs to the end
  of the body; the tempfile lives until its handle drops. Nothing tells the UI or scan anything.
- New: the `Play` claim is released; after `GRACE` the producer stops between chunks, the state
  becomes `Cancelled` (`Stream{Cancelled}`), and the tempfile is removed with the last handle.
  Analyzer `Peek` readers get an error, which the plugin maps to `Retry`.

### 5. Press play on the same uncached track three times in a row
- Old: each load misses `MediaCache` (the file is only cached after a full download) and starts a
  fresh download.
- New: `engine.open` finds the running stream for the key and returns the same handle (a new
  `Claim`); still one fetch. Going A -> B -> A within `GRACE` re-claims A's download.

### 6. The playing track gets its BPM and waveform
- Old: `Playing` -> `prioritize`; the priority attempt is `CacheOnly`, so it fails until the track
  is fully cached; `Materialized` re-prioritizes it and the plugin decodes the finished file.
  Non-playing tracks go through `MediaProvider::materialize` (a whole GET) or, for Spotify,
  `Player::open_for_scan` (a second fetch of the same audio).
- New: `Playing` -> `prioritize` -> `engine.open(r, Peek)` returns a reader on the running stream;
  BPM starts as soon as its 60 s prefix is on disk, the waveform consumes progressively; both finish
  with one `Catalog::patch` -> `TrackUpdated`. Background walks use `engine.open(r, Fetch)`.

### 7. Play a Spotify track
- Old: `SpotifyPlayer` hands the URI to librespot's `Player`, which fetches, decrypts, decodes and
  outputs by itself; `Link` handles the session; `spawn_materialize_to_cache` later copies the
  decrypted Ogg into `MediaCache`; scan uses a separate `open_for_scan` fetch.
- New: `SpotifyMediaProvider::open` returns `Media::Reader` over the decrypted Ogg; `RodioPlayer`
  plays it like any other source; scrubbing jumps through `AudioFile` seeks; the cache fills on
  `Done`; `Link` is unchanged and supplies the session; analyzers `Peek` the same stream.

### 8. The network drops mid-track
- Old: a read error ends the decoder, the sink empties, `tick` sends `Finished`, and the queue
  advances as if the track had ended normally (verify).
- New: the producer retries the chunk 3 times, then `fail`s; the state is `Failed`, and the player
  reports `LoadFailed`/`BackgroundFailure` instead of `Finished`. (The generic "resume after
  failure" is a follow-up TODO item.)

### 9. What the user sees while a track loads
- Old: nothing between pressing play and sound; for HLS a long silent wait; no buffering state.
- New: `Loading`, then `Stream{Connecting/Fetching}`; the status line shows "buffering" or a
  download percent; an underrun pauses with "buffering" and resumes by itself.

## Stages
1. **Remove old, write the new infrastructure.** First a short spike (no repo changes): synthetic
   fMP4 with `ffmpeg` (`-movflags frag_keyframe+empty_moov+default_base_moof`), truncated, into a
   non-seekable `rodio::Decoder` (start, `total_duration`, `try_seek`, read `Err` mid-decode; a
   seekable one blocks), then a real stitched SoundCloud file; confirm before building. Then: the
   stream module, engine, `Media::Stream`, `MediaCache::persist_file` + sweep, events/status, the
   player integration and its status-line display, `core::http::RangeReader` (default options for
   `Media::Url`, importable for sources that need more), the `Url`/`Reader`/`Path` wrapping, the
   SoundCloud HLS producer (sequential, retries, `Rendition.duration_ms`), and deletion of every
   old path listed above (`Media::Url` stays; the HTTP source and SoundCloud progressive keep
   returning it). Sources that stop compiling or working (Spotify, scan, anything using
   `open_for_scan`/`materialize`) may be stubbed to fail loudly until stage 2. Land in several
   commits; verify on real SoundCloud (long set starts in seconds; skip mid-download leaves
   nothing; replaying one track does one fetch; buffering shows; scrolling stays smooth; scrub on a
   progressive track during download works; a seek click on an unfinished HLS track is ignored and
   works after `Done`).
2. **Port the broken sources.** Spotify (provider, delete `SpotifyPlayer`, the carry-over checklist
   above; test playback, scrub, skip, reconnect by cutting the AP socket, cache fill), Soulseek
   (`Media::Path` after slskd finishes stays; investigate whether slskd's partial file can be read
   safely, do not assume), local files, HTTP.
3. **Port the analyzers.** Scan driver on `engine.open`, progressive decode, BPM and waveform
   during download for every source, removal of the last `open_for_scan`/`materialize` remnants.

## Rules for every stage
- No tests (`AGENTS.md`). Verify in the running app (tmux) at `RUST_LOG=debug`; SoundCloud tests
  are allowed freely.
- The owner's media cache, index and track store are read-only for verification.
- Log at `debug`; every failure surfaces as `LoadFailed`/`BackgroundFailure`, never a hang.
- Keep the interface small; leave out what the stage's tests do not need.
