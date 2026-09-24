## Working agreement

- One `general-purpose` agent per task, run sequentially (never in
  parallel, never forked) — each gets a full self-contained brief since it
  starts with no memory of this conversation.
- Each task: read AGENTS.md, then read the relevant code fresh, implement, then `cargo build
  --workspace --all-features`, `cargo clippy --workspace --all-features --all-targets` — both
  clean — before committing (never amending) and `git push origin main`. No tests, ever — see
  AGENTS.md.
- Never stash or undo changes not made by you; multiple agents are running in parallel.
- Genuine user-visible ambiguities get reported back rather than guessed.

## TODOs:

### Owner's list — do these first, in this order
### Bugs
- [ ] SoundCloud scrubbing doesn't work — the top-bar waveform stays entirely white the whole time a
  SoundCloud track plays, even though it's actively playing (per the two-axis waveform model,
  `draw_waveform`, `ui/src/view/tab_bar.rs:243-262`: color is purely `x < played`, and `played` is
  `0` for the whole track whenever `self.status.duration_ms` is `0`). Owner's hypothesis: the
  detected track duration needs to be filled from SoundCloud's API metadata. Already traced, to save
  the next pass some time — the naive "duration is never set" theory doesn't fully hold, so the real
  bug is narrower than that and needs live confirmation, not another guess:
  - `ApiTrack::into_track` (`sources/soundcloud/src/client.rs:1223-1224`) already does read a
    duration from the API (`full_duration` if present and `> 0`, else `duration` — the latter is
    *only* the 30s snippet length on snipped tracks, per its own field comment) and builds the
    track's one `Rendition` with it via `Rendition::fresh(..., full, ...)`.
  - `Track::fresh` (`core/src/types.rs:150-163`) copies `rendition.duration_ms` straight into
    `Track.duration_ms` at construction — so a freshly-imported SoundCloud track's catalog record
    should generally have a real nonzero duration already, contradicting "duration is never set" as
    a blanket explanation.
  - `player/src/rodio_player.rs:542`: `duration_ms = if decoded_ms > 0 { decoded_ms } else {
    r.duration_ms }` — the live player's status falls back to the *catalog* rendition's duration
    when the decoder can't determine one from the stream itself (plausible for a streamed SoundCloud
    transcoding). This should also produce a nonzero `status.duration_ms` if the catalog duration is
    actually present at this point.
  What's NOT yet confirmed, needed before implementing anything: live-check (via
  `:log`/`medley.log` at debug, or briefly instrumenting) whether a real SoundCloud track's
  `Track.duration_ms` in the store is actually nonzero at play time — if it is, the bug is
  downstream of both fallbacks above (something clearing/never-reaching `r.duration_ms` between
  catalog and the live `PlayerStatus`/`StatusLine`, or `r` at open-time not being the catalog's own
  rendition); if it's genuinely `0` in the store, check whether `full_duration` is actually present
  in SoundCloud's real API responses for the tracks being played (the client only ever requests it
  as an optional field — maybe it's absent for a class of tracks this wasn't tested against) and
  whether the `duration` (snippet-only) fallback is silently firing/also `0` for those. Don't guess
  at a fix without first confirming which of these it actually is.
- [ ] A `NoAudio`-erroring track (probe failure, missing track, corrupt/zero sample rate) on a
  non-seekable first decode attempt can stall a scan for up to `FINISH_TIMEOUT` (30 min) before
  being skipped — `decode_blocks` (`core/src/audio_decode.rs:57-75`) treats any `NoAudio` there as
  "maybe needs the full file" and waits for the stream to finish before retrying seekably, even when
  the actual cause (corrupt container, missing track) will never be fixed by waiting. Fail fast
  instead of waiting the full timeout for causes that are provably not "just needs more data".
- [ ] Spotify sign-in: the setup dialog cannot show the OAuth URL and Esc cannot release the listener's port, because `librespot-oauth` prints the URL with `println!` and blocks; a custom PKCE flow or a fork is needed so the dialog can show the URL and cancel the wait.
- [ ] Spotify `recently-played` still confirmed not working — this time with real, rigorous evidence
  (raw HTTP responses quoted, two independent end-to-end runs, up to ~14 minutes elapsed each,
  against the exact PUT sequence `connect_state.rs` (`18aafad`) actually ships), so this isn't a
  validation-quality problem anymore, it's a real architectural finding:
  - `/v1/me/player/currently-playing` **genuinely works** — directly observed, full real track body,
    `is_playing: true`, `progress_ms` advancing. This part of the fix is confirmed good.
  - `/v1/me/player/recently-played` **does not update**, confirmed negative — identical response
    (same 5 pre-existing real tracks, same millisecond timestamps) before and after every test PUT
    sequence, across two separate runs with different variations (bare `NEW_DEVICE`→
    `PLAYER_STATE_CHANGED`→`/inactive`, and a track1→track2 transition with `prev_tracks` populated
    on the second PUT, mimicking real-client track-completion reporting). `active_device_id` matched
    our device every time; no error/warning field ever appeared; not a silent-adoption failure like
    the original bug.
  - **Web Playback SDK scope is a dead end for this approach, confirmed by reading the code**:
    `device_info()` in `connect_state.rs` hardcodes `client_id: MUSIC_CLIENT_ID` — the connect-state
    PUT never goes through `WEBAPI_CLIENT_ID` ("blueball") at all, so a scope added to blueball's
    dashboard registration can't affect it; separately `WEBAPI_SCOPES` doesn't request `streaming`
    anyway, so no token would carry that grant even if it did matter. Don't pursue this angle further
    for the PUT-based approach — it would only be relevant to a wholly different feature (embedding
    the actual JS Web Playback SDK in a browser/webview), a much larger undertaking, not something
    `connect_state.rs` touches.
  - **Hypothesis now CONFIRMED with real evidence — owner captured the real `open.spotify.com` web
    player's actual network traffic on a real track play and found three request types
    `connect_state.rs` sends none of.** This is a completely different API surface than the
    `connect-state/v1/devices/<id>` PutStateRequest medley currently emulates, and is very plausibly
    the actual missing piece for `recently-played`:
    1. `POST https://<region>-spclient.spotify.com/connect-state/v1/player/command/from/<device_id>/to/<device_id>`
       — the actual **play command**, not a state PUT. Body: `{"command":{"context":{"uri":
       "spotify:playlist:...","url":"context://...","metadata":{}},"play_origin":{...},"options":
       {"license":"tft","skip_to":{"track_uid":...,"track_index":...,"track_uri":...},
       "player_options_override":{}},"logging_params":{"page_instance_ids":[...],
       "interaction_ids":[...],"command_id":"..."},"endpoint":"play"}}`. This is spirc/remote-command
       territory — exactly what `connect_state.rs`'s own doc comment says it deliberately does NOT
       implement ("no remote-command handling"). Worth checking whether `recently-played` actually
       needs this specific command to have been the thing that *started* playback (i.e. Spotify's
       backend only trusts a play session it itself dispatched via this endpoint), which would be a
       much bigger architectural implication than just adding a missing PUT field.
    2. `POST https://<region>-spclient.spotify.com/melody/v1/msg/batch` carrying a
       `track_stream_verification` message: `{"play_track":"spotify:track:...","playback_id":"...",
       "ms_played":N,"ms_nominal_played":N,"session_id":"...","sequence_id":0,
       "next_playback_id":"...","playback_service":"track-playback"}` — sent once, early in
       playback (~15s in, in the captured example) — this looks exactly like "confirm this account
       actually listened to this track for N ms," which is a very plausible candidate for the real
       signal behind `recently-played`, distinct from Connect Cluster state entirely. The same batch
       also carried a `jssdk_playback_stats` message (CDN url, codec, bitrate, latency breakdown,
       `reason_start`/`reason_end`, etc.) — likely just telemetry, lower priority to replicate.
    3. A second `melody/v1/msg/batch` call carrying `media_playback_event_v2`: a batch of periodic
       heartbeat events (`et: 10`, incrementing `sn` sequence numbers, `ct` client timestamps ~30s
       apart, `ps.pp` = playback position in ms, `ps.vo` = volume, `ps.br` = bitrate, device/os
       context) — a recurring "still playing, here's the position" heartbeat, again via `melody`, not
       `connect-state`.
    **Next step**: extend the standalone research script (same raw-evidence discipline as before —
    quote real responses, don't narrate success) to also send `melody/v1/msg/batch` calls matching
    these two message types (`track_stream_verification` + periodic `media_playback_event_v2`)
    alongside the existing PutStateRequest sequence, and test whether that's what actually makes
    `recently-played` populate. If it does, `connect_state.rs` needs a second reporting path added
    for this (a `melody` client, separate from the existing `spclient` connect-state one) — a real
    but bounded addition, not the wholesale architecture rewrite the earlier
    audio-delivery-through-Spotify hypothesis implied. Don't implement the `endpoint":"play"` remote
    command path (lead #1) without first testing whether `recently-played` works without it — that
    one has much bigger scope implications (accepting/dispatching commands, which `connect_state.rs`
    currently and deliberately never does) and should only be pursued if leads #2/#3 alone don't work.
  - Not yet fixed, unrelated to the above: the on-disk `webapi_tokens.json` bearer was missing
    `user-read-recently-played` scope — appears to have been fixed already (a re-login happened at
    some point; the redo validation's token had the scope and got clean 200s from `recently-played`,
    not 403s), but confirm this stays true, it's not itself the blocker above.
- [ ] Rapid skips advance the playlist pointer immediately, but while the newly selected track is
  loading or unavailable, the currently-playing title (and the rest of the now-playing readout) must
  keep reflecting the audio that is actually playing, and switch only when the new track's audio
  starts. Decide what the readout shows if the new track fails to load. The player's snapshot already
  describes the playing track until the swap; the Session side (`Loading`/`Playing` handlers set
  `shown.now_playing` and call `set_status`, `self.progress`, the queue pointer, `core/src/app.rs`)
  still switches at once; `Progress` is identity-guarded, so the bar sits at 0 / the new track's catalog length until the swap (the swap reports the new track's decoded length, also when it starts paused); `Seek` during a pending load moves the previous track without touching the bar.
- [ ] Media keys still don't work on macOS, and the OS "Now Playing" status/widget never gets updated. Investigate whether this needs some form of app registration/packaging (e.g. macOS media-remote/`MPNowPlayingInfoCenter`/`MPRemoteCommandCenter` integration typically requires a proper `.app` bundle with an `Info.plist`/bundle identifier, not a bare CLI binary) — figure out and document the actual OS requirements needed to make this work, then implement whatever's missing.
- [ ] The screen's rightmost column (seen on macOS) holds stale cells and shows garbage after a window
  resize. Suspects, to check in this order: (1) cells nothing repaints — `draw_row_list`
  (`ui/src/view/rows.rs`) pads the title row only to `content_w` (width minus the scrollbar gutter), so
  its last cell is never written, and `draw_list_body` paints no row text below `rows.len()`; tab
  bar and status line may have the same off-by-one against `printer.size.x` — every row
  `draw` owns should be written edge to edge each frame (or the view cleared first); (2) width
  disagreement with the terminal — `pad`/`truncate`/`five_col` measure with `unicode-width`, and a
  glyph macOS Terminal/iTerm renders wider or narrower than that (emoji, variation selectors, CJK,
  ambiguous-width box/transport glyphs like `━╍⏸ϟ♥⚠`) pushes a line into or short of the last column,
  leaving leftovers cursive's diffing never rewrites; (3) resize handling — `Event::WindowResize`
  should force a full clear + redraw (`Cursive::clear`), and `last_screen_size`/`MedleyView::placed`
  must be refreshed before the first post-resize draw. Repro on macOS by resizing with a long list
  and wide-glyph titles on screen.
- [ ] Soulseek: play before slskd finishes the transfer. `SoulseekSource::open` waits for the whole download
  and returns `Media::Path`. slskd writes the partial file under `<folder>/incomplete/<user>/<remote dirs>/`;
  following it with a `Media::Stream` producer needs its exact incomplete file name and resume behavior
  verified against a connected slskd (its Soulseek server connection was down, so nothing was downloaded).
- [ ] Spotify downloads that overlap (a skipped track still fetching during `GRACE` while the next one
  starts) sometimes stall near the end until librespot's 8 s read timeout; `SpotifyMediaProvider`'s reader
  reopens and finishes, but a stall while the playing track is the one behind shows as buffering. Find out
  whether librespot's fetch loop or the CDN causes it, and consider cutting a released Spotify fetch at once.
- [ ] Rapid/repeated scrubbing (seeking) causes an ALSA underrun: `ALSA lib pcm.c:8787:(snd_pcm_recover)
  [error.pcm] underrun occurred`, seen alongside a burst of Spotify stream reads all hitting `Deadline
  expired before operation could complete { wait timeout exceeded }` and dropping `Fetching -> Buffering
  -> Cancelled` for several tracks at once while a priority track scan stalls. Find the actual audio
  glitch/dropout this produces during heavy scrubbing and fix the underrun, not just the log noise.
- [ ] Gapless playback as a generic preload: `StreamEngine::open(next, Intent::Play)` shortly before the
  current track ends; `RodioPlayer` keeps `Player::preload`.
- [ ] A "downloading" mark on rows from the engine's stream status.
- [ ] There is no way to delete (or rename) a local playlist. Add `:deleteplaylist <name>` (confirm
  dialog; drops its hotkey binding; windows showing it back out to the top level) and
  `:renameplaylist <old> <new>`, both through `Catalog`'s playlist write path so `playlists_gen`
  bumps; rows in the item table (`ui/src/items.rs`). The owner's database holds scratch playlists
  from agent test runs (`alpha`, `beta`, `gamma` twice each, `tmp1`, `tmp2`, `shuffletest`,
  `zz-scratch*`, `scantest`, `FilterBugTestXYZ`) waiting for this.
### Features
- [ ] Sparse-fragment waveform decode: `WaveformPlugin::decode`
  (`sources/waveform/src/lib.rs`) currently linearly decodes the entire track to build the
  400-bucket envelope (`core::waveform::BUCKETS`), and measured decode is ~99%+ of its total cost
  (RMS math itself is negligible — confirmed by direct instrumentation, not just code reading).
  Since the output is only ever 400 coarse
  buckets, decoding a short window (e.g. 1-2s) every N seconds and using each window's RMS to stand
  in for its bucket range instead of a full linear decode should cut CPU roughly in proportion to
  how much of the track is skipped, likely without a visually meaningful difference in the resulting
  bar chart. The seek infrastructure for this already exists: `core::audio_decode`'s `StreamSource`
  tracks `seekable`, true once a stream is fully local/cached — which waveform scanning already
  requires before it decodes at all — so `symphonia`'s `format.seek()` is available for the cached
  case that actually matters here. Before implementing: check how cheap seeking actually is for this
  library's real formats in practice, especially MP3 VBR without a seek table (may need some resync
  scanning per seek, so it's not literally free per-jump, just much cheaper than decoding everything
  in between) — if seek cost eats too much of the savings for a common format, the fragment
  size/count needs tuning accordingly. Only applies to the locally-decoded `WaveformPlugin` —
  SoundCloud's own API-based waveform plugin already avoids decoding entirely and is unaffected.
- [ ] Make the `:vis` pane draw a beat indicator from the beats anticipated by the BPM analyzer
  (`BpmPlugin`, `sources/bpm/src/lib.rs`; the pane is `ui/src/vis.rs`). Today the analyzer only stores a
  tempo (`attrs["bpm"]`); a beat indicator also needs the beat phase (the time of a beat, so the grid
  can be projected forward from the playback position) — have the analyzer produce it, and prefer
  receiving it as events from the live analysis (the stream engine lets it run while the track
  downloads) over reading a stored value only. The pane then flashes/pulses on each anticipated beat
  and stays quiet when no tempo is known.
- [ ] SoundCloud playlist creation (`POST /playlists`); needs a create-playlist hook on the `Source` trait first.
- [ ] SoundCloud: show charts and genre explore playlists (their own endpoints) and station shelves from `/mixed-selections` in the Playlists view.
- [ ] `:open` for SoundCloud sets and short links: `soundcloud.com/<user>/sets/<slug>` needs a
  `browse_uri` that resolves the URL through `/resolve` to a playlist id and lists it with
  `SoundcloudSource::playlist_page` (`sources/soundcloud/src/client.rs`); `TrackRef::parse` (`uri.rs`) currently
  rejects 3-segment set paths. `on.soundcloud.com/...` short links need their redirect followed to the
  permalink first.
- [ ] When the selected row and the currently playing track are the same, and the list showing it is
  the playing track's own playlist (i.e. the selection is already "on" now playing), auto-follow the
  selection to the new track when playback advances to the next track in that playlist — same effect
  as pressing `0` (`BuiltinAction::RevealPlaying`, `core/src/app.rs:129`, handled by `reveal_and_show`,
  `ui/src/view/input.rs:220`), just triggered automatically by the track change instead of a keypress.
  Only when both conditions hold; otherwise leave the selection where the user left it.
- [ ] Bracketed paste in the TUI: enable it in terminal setup, add a paste event to the edit buffer
  (`ui/src/view/input.rs`) and strip newlines so a multi-line paste can't run a command.
- [ ] `y` (copy shared link) polish: debounce repeated presses (each spawns a detached thread and a
  network call).
- [ ] Audit the codebase for keys written as literal strings (`` ` ``, `P`, `M`, `+`, …) in Help text,
  hints, notices, doc strings and prompts instead of being resolved to the key currently bound to
  that action; make those resolve through the bindings (as `Chrome`'s `*_key` fields do) or list what
  can't.
- [ ] `core::config::PaneMode` (`panes.mode` in `config.toml`: `screen`, `embedded`, `float`) only seeds
  the five pane windows' placement for a default layout; merge it into `ui::screen::Placement` so the
  config takes `tabbed` too and there is one enum and one vocabulary.
- [ ] Create a playlist from inside the "Add to Playlist" picker: with the picker open (`+` on a
  track — `Action::AddToPlaylistPrompt` → `PlaylistPicker`, `ui/src/view/playlist_picker.rs`),
  pressing `+` again opens a name prompt; Enter creates the playlist (`Command::NewPlaylist`, the
  same path as `:newplaylist`) and returns to the picker with the new playlist in the list and
  selected, so a second Enter adds the track to it; Esc in the prompt returns to the picker
  unchanged. `+` with no track selected already opens the `newplaylist ` command line
  (`Action::NewPlaylistPrompt`, `ui/src/view/input.rs`) — reuse that prompt/`commit_edit` path
  rather than a second text field, with the picker kept open underneath instead of being closed
  by entering edit mode (check the modal precedence chain in `on_event`/`draw`: `editing` is
  handled before the picker). The key is whatever `BuiltinAction::AddToPlaylistOrNew` is
  effectively bound to, not a hard-coded `+`. The picker refreshes its playlist snapshot on a
  `Session::revision` change, so the new row appears without reopening; add the key to the
  picker's footer hint (`[+] new playlist`). An empty picker ("no playlists") gets the same key
  in place of the `:newplaylist <name>` instruction text.
- [ ] Add a hard-redraw key (a `BuiltinAction` with a default key and a Help row in `ui/src/items.rs`,
  rebindable like the other built-ins; no `:` command, e.g. Ctrl-L if the key table can carry a
  control key, else a plain letter) that clears the whole screen (`Cursive::clear`, so nothing stale
  survives a garbled terminal) and makes every UI element refresh: the frame snapshot and per-window
  layout memos are invalidated (`Session::revision` bump or an explicit "everything dirty" flag),
  `last_screen_size` is re-read from the terminal, and the next draw rebuilds rows, tab bar, hint and
  status lines from scratch. The window-resize handler needs exactly this too (see the stale
  rightmost-column bug), so share one function between the two.
- [ ] Create playlist files (m3u8) when the playlist cache updates automatically, this basically creates playlist sync feature for the user. It should be in a Documents directory so the user doesnt have to adjust it (but it should be possible in settings). Each entry should point at the track's path in the media cache — ask the media cache to resolve/convert a track to its assumed on-disk location there (even if it hasn't actually been downloaded/cached yet) — so the written m3u8 files are actually playable.
- [ ] Add a YouTube source/plugin (alongside the existing Spotify/SoundCloud/HTTP/local sources), wired into Search like the others.
- [ ] Move the default media-cache directory to `~/Downloads/medley`, and make it adjustable from
  Settings. Store each entry's path relative to that root directory (not absolute) so moving/renaming
  the whole library directory is discovered transparently, with nothing pointing at the old path.
- [ ] Add a single-character spinner somewhere visible to indicate when a network request is in
  progress, for cases like opening a playlist that first tries a request, gets a 403, then tries a
  bunch of fallbacks before succeeding or failing — right now there's no visual indication anything
  is happening during that stretch.
- [ ] A "similar tracks" panel that filters the already-cached library for tracks similar to a
  chosen one (default: the playing track / the cursor row). For now similarity is just BPM — tracks
  whose `bpm` attr is within a tolerance of the reference (the BPM scan's values live in the track
  attrs, `t.attrs["bpm"]`) — but keep the comparison behind one function so other attributes can be
  added later. Only tracks that are cached count; results sort by distance and open as a normal
  track list window (same paths as Search results). It must work in any placement — docked next to
  the playing track's list is the main use: keep playing, watch the similar tracks follow the
  playing track, and queue the ones you like with the normal queue keys.
- [ ] Research task (read-only, no code): survey the current state of the art for audio genre/"vibe"
  detection and audio embeddings, beyond CLAP (the owner's only current reference point) — what
  models/approaches exist for classifying or comparing tracks by genre, mood, or general "vibe"
  similarity, and which are practical to run locally against an already-cached personal library like
  medley's (model size, licensing, whether inference needs a GPU). Report findings as a simple list;
  this feeds a possible future extension of the "similar tracks" panel above (comparing tracks by
  attributes beyond BPM), not an immediate implementation task.
- [ ] Give the Log pane (`ui/src/view/log.rs`) its own view filter — grep-style substring filtering
  over its lines, same `/`-opens/Enter-locks/Esc-clears interaction as a `TrackList` window's view
  filter, but built separately: Log is a raw line buffer, not backed by `TrackList`, so it needs its
  own filter state and matching, not a generalization of the `TrackList` mechanism.

### Album support
Design: `docs/collections.md`.
- [ ] Opened collection title line: add `subtitle: Option<String>` to `BrowsePage` (filled by the
  source, e.g. "Album · 2019 · 12 tracks") and show it on the list window's existing title line, with
  `release_label(track_count)` (single ≤3, EP 4–7, else album) once the list is loaded. Also make the
  Playlists window's title unit follow the kind filter ("56 albums", not "56 playlists").

### Performance
- [ ] `min_interval`'s network-fetch cooldown is skipped once a track's audio is already locally
  cached (`gated` is `false` when `has_stream` is `true`, `core/src/scan.rs:619-630`) — it only ever
  paced fresh fetches, never decode/CPU cost, so a cached backlog gets no pacing beyond the walk's 1s
  `PASS_SPACING`. Needs a design decision before any code change (not a guess): a naive
  "always apply min_interval" would slow a first-time backfill of an already-cached library to
  `library_size × min_interval` (hours) and delay now-playing analysis by up to 15s behind an
  unrelated walk attempt, since `last_run` is shared per-plugin between the background walk and the
  priority/now-playing worker. A real fix likely needs a separate, smaller CPU-pacing interval
  distinct from the network cooldown, and/or per-worker rather than per-plugin `last_run` tracking.

### Audits / cleanup tasks
- [ ] Find functionality that exists in the codebase but isn't currently bound to a key or command, and wire it up so it's reachable.
- [ ] Run an agent to collect and remove any placeholders of any kind. Write it in the memory to never write placeholders of any kind. Check the 
  todo for infra that is stubbed for unimplemented parts and remove it. Remove any reference for
  future features by moving them on the main todo list. never keep done items on the todo list.

- [ ] UI architecture work order (each step is an item under Features): (1) the status-row widget,
  the title scrubber; (2) the remaining bindable Help rows. The
  `commit_edit`/`Parsed`→`Action` cleanup, `Option<TextField>`, the Vis levels lock and a `split` axis
  helper get no pass of their own — fold them in when those files are touched.
- [ ] Make the UI event-driven instead of re-deriving everything per frame — the program should use
  messages and reactive patterns to communicate between, and render, independent parts of the app (no
  part reaching into another's state or recomputing/polling per frame what an event should drive).
  The component model is in `ui/src/view/README.md`; what breaks it today, in value order:
  - `Session::playlists()` is a store read transaction plus a clone of every playlist's `items`. It
    still runs on every layout pass while a Playlists window sits at its top level
    (`TrackList::relayout` → `len` → `top_rows`, `ui/src/view/track_list.rs`), and on each rows
    rebuild. Give `Session` a playlist-names cache keyed on `Catalog::playlists_gen`.
  - Let `on_event` reuse the frame snapshot's (`ui/src/view/frame.rs`) cheap parts instead of
    re-locking (`TabBar` click, `StatusLine::snapshot` on click, `warn_count`).
  - While the Vis pane is open, the `Vis` worker's `session.lock().unwrap().audio_levels()`
    (`ui/src/vis.rs`) contends with the main session lock at 30 Hz; move audio levels behind their
    own lock/atomic instead of sharing the `Session` mutex.
- [ ] `commit_edit` (`ui/src/view/input.rs`) handles `command::Parsed` through a ladder of `if parsed ==`
  checks, several of which only forward to `handle_action` (`Parsed::History`, `Parsed::Keys`,
  `Parsed::Help`). Map UI-level `Parsed` variants to `Action` and keep one `match`.
- [ ] `ui/src/lib.rs`'s crate doc describes a stateless three-screen view; rewrite it to point at
  `ui/src/view/README.md`. Then write a small (~4-8 KB) set of guides for further agents covering the
  app's architecture invariants and ways of working such as checking for excessive comments or
  inefficient/verbose implementations before pushing work.
- [ ] Run an agent to reduce code duplication and DRY violations, along with any
  violations of the user policies.

### Only if observed
- [ ] (Do NOT start unless the owner has seen playback wedge again after the skip debounce and the
  Spotify session recycle on consecutive load failures; capture `medley.log` at that moment first.)
  Harden the Spotify playback path against a wedged librespot session and skip floods: cancel the
  librespot load already in flight when a newer one supersedes it; a timeout that turns a stuck
  `Loading` state into `LoadFailed` and lets the user retry; a debounce or coalescing inside the
  rodio and Spotify players themselves (the core skip debounce only covers `Command::Next`/`Previous`);
  load-generation tags on Spotify `Playing` events so a superseded load's events are dropped.
