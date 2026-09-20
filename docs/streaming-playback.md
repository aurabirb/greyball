# Streaming playback (start before the download finishes)

Brief for agents. Read `AGENTS.md` first. Phases run one agent at a time, in order. Facts marked
"verify" come from a quick read of the code, not from a test.

## Goal
- A track plays after its first few seconds of audio have arrived, for every source that fetches
  its audio over the network, at SoundCloud's 160 kbps AAC HLS quality.
- The app announces "more of this track's audio is available" so a scan plugin can analyze a
  partial track while it loads.
- Losing scrub until the download completes is accepted.

## Why playback waits today (verify)
- `SoundCloudClient::open_hls` (`sources/soundcloud/src/client.rs`) fetches every segment serially,
  then returns `Media::Path`. Progressive audio returns `Media::Url`, which `open_streaming_url` /
  `StreamingReader` (`player/src/rodio_player.rs`) already streams.
- Even with a growing file, symphonia's MP4 reader scans every top-level atom when the reader is
  seekable, so decoder construction would block until the download ends. The player passes a byte
  length to `rodio::Decoder`, which forces seekable mode. The streaming path must give the decoder a
  non-seekable stream (the HLS init segment supplies the moov box) and accept the lost scrub.
- Nothing is known about how rodio behaves for a non-seekable fMP4 (`try_seek`, `total_duration`).
  Phase 0 tests it.

## Design
- **One shared mechanism in `core`.**
  - A progressive audio handle: a growing tempfile plus shared state (bytes available, done,
    failed, cancelled; Mutex + Condvar), generalizing `StreamState` / `StreamingReader`.
  - The reader blocks only on bytes that have not arrived yet, and returns as soon as it is
    cancelled.
  - Total length is optional. A source may also supply a duration and a time-to-offset map (HLS
    `#EXTINF` durations are in the playlist and are currently ignored).
- **New `Media` variant** (for example `Media::Stream`) returned by a `MediaProvider`. Changing
  `Media` or `MediaProvider` is fine. Do NOT change the `Player` trait; if that turns out to be
  necessary, stop and ask the user.
- **Progress event.**
  - A new `CoreEvent` (name it to fit the siblings; `PlayerEvent::Progress` already means
    playback position) carrying `source`, `uri`, bytes (and milliseconds when known) available,
    and a done flag.
  - Send it per chunk or segment, throttled so it cannot flood the bus. `Materialized` stays the
    "fully cached" signal.
  - Sending must never block or take the `Session` lock.
- **Analyzers.**
  - Phase 5 only: let `ScanPlugin`s consume the partial audio through the event plus the growing
    file.
  - Start with BPM, which only decodes a prefix (`ANALYSIS_SECONDS`). The waveform (needs the
    whole file) can follow the "progressive waveform" item in `TODO.md`.
- **Cache.** A streaming download is put into `MediaCache` only when it completes, atomically
  (temp file + persist, as `put_file` does). A cancelled or failed download leaves nothing behind.
- **Fail-safe.** If streaming setup fails, fall back to the existing whole-file path. Keep that
  path but isolate it in one module so it can be deleted later.

## Robustness rules (every phase)
- Never block the UI thread, the session lock, the cache, or the bus. All fetching runs on worker
  threads.
- **Abandon cleanly.** Skipping, stopping or replacing a track cancels its download: the worker
  stops between segments (and aborts an in-flight request via short timeouts), the tempfile is
  deleted, no thread is left waiting, no callback keeps the reader alive. Ownership is RAII: the
  handle's `Drop` cancels and cleans up.
- **Not wasteful.**
  - If the track is already in `MediaCache`, do not stream.
  - Keep a registry keyed by `(source, uri)` of in-flight downloads. A second `load` of the same
    track attaches to the existing download instead of starting another.
  - When the last consumer goes away, cancel after a short grace period, so a user who replays the
    same track repeatedly reuses the download; after the grace period, delete the partial file.
  - Keep concurrency small (a fixed worker pool per download is enough).
- Every failure surfaces as `LoadFailed` (or the existing equivalent), never a hang.
- Log at `debug` so behavior is checkable in `~/.local/state/medley/medley.log`.

## Phases
0. **Experiment** (no repo changes). With `ffmpeg` (installed), build a synthetic fragmented MP4
   (`-movflags frag_keyframe+empty_moov+default_base_moof`), truncate it, and check that
   `rodio::Decoder` in non-seekable mode starts, what `total_duration` and `try_seek` do, how it
   ends at the truncation, and that a seekable decoder blocks. Then repeat on a real SoundCloud
   HLS stitched file. Report findings. The user allows extensive real SoundCloud testing.
1. **Core mechanism:** progressive handle, reader, registry, `Media::Stream`, the progress event.
2. **SoundCloud HLS:** `open_hls` becomes a background segment fetcher feeding the handle (small
   worker pool, duration and offset map from `#EXTINF`), with the fallback to the old path.
3. **Player integration** (`player/src/rodio_player.rs`): a `Media::Stream` arm, non-seekable
   decoder, cancel on skip/stop/replace, cache only on completion, seek is a no-op or clamped
   until done.
4. **Generalize** in the same agent as phase 3 (the other sources can be tested later): the
   no-`Content-Length` and `Media::Reader` fallbacks, and HTTP.
5. **Analyzer consumer:** BPM on a partial track.

## Sources after this work (verify)
- **SoundCloud (HLS and progressive), HTTP (also without `Content-Length`), any `Media::Reader`
  provider:** stream.
- **Local files** (`Media::Path` to an existing file): already instant; nothing to stream.
- **Spotify:** librespot streams natively and caches in the background
  (`spawn_materialize_to_cache`); keep as is. Optionally emit the progress event there later.
- **Soulseek:** `SoulseekSource::open` waits for slskd to finish the transfer before returning
  `Media::Path`. It keeps the old whole-file path unless slskd's partial file can be read safely.
  Investigate, do not assume.
- **The scan prefetch** (`MediaProvider::materialize`, its default whole-body GET) keeps the old
  path.
