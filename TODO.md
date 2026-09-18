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

### Bugs
- [ ] Spotify: a real-world librespot AP death ("Connection to server closed.", upstream
  [#1151](https://github.com/librespot-org/librespot/issues/1151)/
  [#1486](https://github.com/librespot-org/librespot/issues/1486)) hasn't been observed against the
  background reconnect (`Link` in `sources/spotify/src/player.rs`), only deaths induced by cutting the
  AP socket through a local proxy. When one shows up in `medley.log`, confirm playback carried on
  ("reconnecting in the background" → "session connected", no `Stopped` in between), then delete this.
- [ ] Media keys still don't work on macOS, and the OS "Now Playing" status/widget never gets updated. Investigate whether this needs some form of app registration/packaging (e.g. macOS media-remote/`MPNowPlayingInfoCenter`/`MPRemoteCommandCenter` integration typically requires a proper `.app` bundle with an `Info.plist`/bundle identifier, not a bare CLI binary) — figure out and document the actual OS requirements needed to make this work, then implement whatever's missing.
- [ ] UI stutter when first opening the Playlists screen on a large library — reported still happening
  after commit `58c6960` (which fixed a real but apparently-not-the-only per-row `MediaCache` redb-
  transaction cost) and reproduces specifically when `:log` isn't already open. `git log --stat` over
  the last ~10 commits touching this area found nothing else suspicious besides `media_cache.rs`
  itself — the more likely remaining culprit lives in older (2026-09-16) code, unrelated to any of
  today's commits: `ui::view::tracks_to_rows` calls `hotkey_playlist_membership` (`ui/src/view.rs`)
  fresh on every `draw()` call (i.e. every frame at whatever fps is set, not once per screen-open),
  and for every hotkey-bound *remote* playlist target that isn't fully paginated yet, that calls
  `Session::remote_playlist_track_ids` → `ViewCache::ensure_remote_playlist_tracks` (`core/src/
  view_cache.rs`), which spawns a background thread to fetch the next page whenever the target's
  cache entry isn't already `browsing`. Net effect: for N hotkey-bound remote playlists that are all
  still loading, this can fire N near-simultaneous background fetches, and — since nothing memoizes
  `hotkey_playlist_membership` or event-gates it — the very next `draw()` after each page lands kicks
  the next one immediately, unthrottled. This matches the separately-reported "~10 Spotify API
  requests almost at the same second" behavior below, and plausibly explains "only when :log isn't
  open" as pure timing (by the time you've looked at :log first, those background pagination fetches
  have often already finished and gone quiet) rather than any real causal link to the Log pane itself
  — unconfirmed, needs verifying against the reporter's actual large-library machine, which this
  session doesn't have access to. Fix direction: memoize `hotkey_playlist_membership` (or the whole
  row-building path) instead of recomputing from scratch every frame, invalidating only on an actual
  membership-changing event — this would also benefit the playlist-hotkey-toggle staleness bug below,
  since both read the same `ViewCache` state.
- [ ] Playlist hotkey toggle (add/remove a track via a playlist-bound hotkey) doesn't refresh the
  playlist view or the track list's hotkeys column afterward. Root cause: `Session::
  toggle_remote_playlist_membership` (`core/src/app.rs`, ~line 1931) calls `Source::add_to_playlist`/
  `remove_from_playlist` on a background thread and, on success, only sets `membership_feedback` (the
  status-bar message) — it never invalidates or updates `ViewCache`'s cached `remote_playlist_tracks`
  entry for that `(source, node)`, which is the same cache both the open playlist's own row list and
  the hotkeys column (`hotkey_playlist_membership`) read from, so neither reflects the change until an
  unrelated full re-fetch happens to occur. Toggling should already flip add↔remove based on current
  membership (it does — see `is_member` in that function) so that part just needs its result to
  actually reach the view; the ask is confirming/fixing that specifically. Also: hotkey binding today
  doesn't exclude a source's synthetic "Liked Songs" node (`Source::is_synthetic`/`is_synthetic_playlist`)
  from being assigned a hotkey at all, so a hotkey *can* currently be bound to Liked Songs and toggling
  it would call `remove_from_playlist` against it like any other playlist — add an explicit guard so a
  playlist-hotkey toggle can never remove from a Liked Songs node, regardless of what it's bound to.
- [ ] A playlist hotkey pressed repeatedly on the same track adds it to the playlist again each time
  instead of toggling add↔remove (remote playlists included). Likely cause: `Session::
  toggle_remote_playlist_membership` (`core/src/app.rs`) decides add-vs-remove from `is_member`, read
  out of `ViewCache`'s `remote_playlist_tracks` — which a successful add/remove never updates (the
  bug above) and which is empty/partial while the playlist is still paginating — and it ignores
  in-flight work, so a second press before the first request lands also reads "not a member". Fix
  direction: keep one membership state per `(source, node, track)` — `Member`/`NotMember` plus
  `PendingAdd`/`PendingRemove` — that the toggle reads and writes: a press flips it optimistically
  to the pending state, the background result settles it (or rolls it back with an error) and
  updates the cached track list, and a press while pending is ignored or queued rather than sent
  again. `pending_remote_adds` only covers adds and only feeds the open playlist's placeholder row —
  fold it into this. Show the pending state in the track row: the playlist's letter in the hotkeys
  column renders italic until the request settles. Check the local (`HotkeyTarget::Local`) path
  toggles correctly too.
- [ ] Adding a track to a remote playlist (e.g. via hotkey from the Queue) shows it in the open
  playlist only while the request is pending — then it vanishes instead of becoming a real row, and
  nothing that depends on the playlist's contents updates. Cause: the pending row is a
  `plain_row("…  (adding…)")` that `rows()`'s `open_remote` branch (`ui/src/view.rs`) appends from
  `Session::pending_remote_adds`; when the background add in `toggle_remote_playlist_membership`
  (`core/src/app.rs`) succeeds it just drops that pending entry and never inserts the track into
  `ViewCache`'s `remote_playlist_tracks` (or re-fetches it). Fix with the membership state machine
  above: on success, insert/remove the track in the cached list (and bump its total), then emit one
  "playlist contents changed `(source, node)`" message that everything derived from it reacts to —
  the open playlist's rows and title count, the hotkeys column on every list, the top-level
  Playlists counts, the m3u8 export once it exists. On failure, remove the pending row and surface
  the error. The pending row should be a normal track row (shared row widget, italic/dim while
  pending), not a bare text placeholder.
- [ ] Track rendering isn't consistent across lists: the same track should render identically (tags,
  hotkeys/playlist column, liked state, current marker) and update at the same moment on every tab
  and docked pane. Seen: pressing a playlist hotkey while in the Queue doesn't update the playlist
  column there. Every list already goes through `tracks_to_rows` (`ui/src/view.rs`), so the likely
  cause is the stale `ViewCache` membership data from the hotkey-toggle bug above rather than a
  Queue-specific path — fix that first, then audit each screen's `rows()` branch (Now Playing, Search,
  Playlists, Queue, History, filtered lists, docked panes) for any per-screen difference in what a
  row shows or when it refreshes, and remove it. End state: one reusable track-row widget
  (React-component style — a pure function of a track's state: identity, tags, liked, playlist
  membership incl. pending, current marker) used by every list, redrawn because a message/event
  said that state changed (membership settled, like toggled, playback moved) — never by each screen
  recomputing or polling it per frame.
- [ ] Spotify has stopped recording listening history — investigate why (was working before; unclear
  which change, if any, broke it, or whether it's an account/API-side change).
- [ ] Check whether the background media scan is polling/ticking at a needlessly high rate and wasting
  CPU when idle. Design an algorithm that cuts down how often it checks while staying responsive —
  e.g. back off the poll interval the longer nothing's changed, waking immediately (not waiting out a
  slow interval) on an actual triggering event instead of polling for one.
- [ ] A track that appears multiple times in a playlist shows up as playing on every occurrence while
  it plays — only the one occurrence actually being played (by position in the context, not by track
  identity) should be marked.
  Suspected fix: `tracks_to_rows` (`ui/src/view.rs`) sets `Row::current` from
  `t.is_current(s.now_playing_id())` — pure track identity. Expose the playing position from core (a
  `Session::playing_context_index()` reading `PlaybackContext::index`, plus which list it refers to —
  `remote`/playlist id — so it only applies when the list on screen IS the playing context) and have
  `rows()` pass each row's absolute index (`offset + i`) down; mark `current` only when identity
  matches AND, for the playing context's own list, the index matches. Lists that aren't the playing
  context (Search, History, another playlist) keep identity matching, but mark only the first
  occurrence. A track played from the manual queue has no context index — fall back to identity.
- [ ] Playing a long uncached SoundCloud track waits for the whole download before playback starts
  (repro: "OZORA Festival - Galactic Explorers @ Ozora Festival 2023 | Ozora Stage", a multi-hour set).
  Likely cause: with `[soundcloud] hls` on, `open_hls` (`sources/soundcloud/src/client.rs`) fetches the
  init segment + every media segment into a tempfile and only then returns `Media::Path` — the
  progressive path returns `Media::Url`, which `player/src/rodio_player.rs`'s `open_streaming_url`/
  `StreamingReader` already streams (unless the response has no `Content-Length`, which falls back to
  a fully-blocking download — check which case this track hits in the log). Fix direction: fetch HLS
  segments on a background thread into a growing file and hand the player a reader that blocks on
  not-yet-fetched bytes like `StreamingReader` does (the fMP4 total size isn't known up front, so the
  preallocate-by-`Content-Length` trick needs adapting — e.g. sum segment sizes via HEAD/byte-range
  info, or let the reader treat EOF-before-done as "wait"), prioritizing the segment under the seek
  position; same treatment for the no-`Content-Length` and `Media::Reader` blocking fallbacks.
- [ ] Scrolling the Log pane is very slow or unresponsive. Likely cause (unconfirmed — profile or
  log event→draw latency first): `log_render_lines` (`ui/src/view.rs`) clones the entire log snapshot
  (`Vec<String>`) on every call, and both `draw` and `clamp_pane_scroll` (run on every scroll event)
  then `wrap()` every line of it just to get a total wrapped height — O(whole log) per frame and per
  wheel tick, which grows unbounded over a session and is worst under `RUST_LOG=debug`. Fix direction:
  borrow instead of cloning, cache wrapped line counts per (line, width) and only wrap newly appended
  lines / the visible window, invalidating on resize. Also rule out an event-side cause: wheel events
  queuing up behind slow draws (coalesce consecutive scroll events before redrawing), and the
  Log-pane mouse handling at `handle_mouse`'s `pane == Pane::Log` Press/Hold/Release branch swallowing
  or mis-routing wheel events.
- [ ] A playlist hotkey persisted in `state.toml` can shadow a built-in key (seen: `s` no longer
  toggles shuffle). `bind_hotkey` (`core/src/app.rs`) refuses to bind over a built-in, but
  `Session::set_hotkeys` loads the persisted map unchecked, and `effective_target_at` lets an explicit
  binding win over a built-in's default — so a binding made before a built-in claimed that key (or a
  hand-edited file) silently steals it. Suspected fix: validate in `set_hotkeys` — drop any
  non-built-in binding whose key is a built-in's effective key (its default unless that built-in is
  remapped elsewhere), push a warning naming the dropped binding, and let the next save persist the
  cleaned map.
### Features
- [ ] The seek keys (`,` `.` and Left/Right) should also refocus the list view on the currently
  playing track.
  Suspected fix: all four end in `self.run(Command::Seek(±5000))` (`ui/src/view.rs` ~line 4159 for
  Left/Right; `,`/`.` via `keybindings.rs`'s `BuiltinAction::SeekForward/SeekBack`) — in `run`, after a
  successful `Command::Seek`, find the playing track's index in the current screen's list (the
  position-aware lookup from the duplicate-marker bug above; `visible_track_ids` for identity
  otherwise) and move the cursor there the way `click_row` does (`self.cursor[screen] = idx;
  self.clamp_scroll();`, which scrolls it into view) — no-op when the playing track isn't in the list
  on screen. Don't do it for mouse scrubber seeks.
- [ ] Wire `[soundcloud] hls` (prefer higher-bitrate HLS over 128kbps progressive) up in the Settings
  UI as a checkbox next to the existing SoundCloud settings — the config flag exists and is honored,
  just not yet exposed there.
- [ ] Make soundcloud provide explore page playlist in the playlists view
- [ ] On soulseek setup page, it should ask the user if they want to set up slskd with docker if it is unavailable, and if the user types yes there should be a docker command with directory and everything set up so that medley can find it, the default folder should be ~/Documents/slskd. if the user skips or types something else we just ask the host, username and password for the slskd instance. the detected slskd status should show up in settings
- [ ] Ability to include spotify playlists in search results, maybe on the playlists tab initially
- [ ] Create playlist files (m3u8) when the playlist cache updates automatically, this basically creates playlist sync feature for the user. It should be in a Documents directory so the user doesnt have to adjust it (but it should be possible in settings). Each entry should point at the track's path in the media cache — ask the media cache to resolve/convert a track to its assumed on-disk location there (even if it hasn't actually been downloaded/cached yet) — so the written m3u8 files are actually playable.
- [ ] Add a YouTube source/plugin (alongside the existing Spotify/SoundCloud/HTTP/local sources), wired into Search like the others.
- [ ] Move the default media-cache directory to `~/Downloads/medley`, and make it adjustable from
  Settings. Store each entry's path relative to that root directory (not absolute) so moving/renaming
  the whole library directory is discovered transparently, with nothing pointing at the old path.
- [ ] Add a single-character spinner somewhere visible to indicate when a network request is in
  progress, for cases like opening a playlist that first tries a request, gets a 403, then tries a
  bunch of fallbacks before succeeding or failing — right now there's no visual indication anything
  is happening during that stretch.
- [ ] Turn the bottom status/hint row into its own module that can be placed either up top next to the
  tabs (replacing the redundant track-controls row that's currently up there) or down at the bottom,
  leaving only the command/help row at the bottom when it's moved up. Switchable via a toggle in the
  Settings UI (wired up there, not config-file-only).
- [ ] Hide the sources column on narrow terminal sizes.

### Audits / cleanup tasks
- [ ] Review how plugin/source failures are surfaced to the user and make the channel match the
  failure's nature, instead of whatever each call site currently happens to do:
  - A failure directly caused by user input (e.g. liking a track fails) should show an error modal.
  - A failure that just means the action is blocked/not applicable right now (e.g. today's "no
    liked-songs source for this track" case) should surface in the command status bar, same as the
    current like/unlike feedback.
  - A failure in background work (e.g. a scan/plugin probe failing on its own, not in response to a
    keypress) should be non-actionable and added to the warnings list instead, clearing on restart —
    not popped as a modal or shoved into the status bar.
  Audit existing call sites (`set_liked`/`Command::Like`/`Unlike` in `core/src/app.rs`, plugin
  `probe()`/`setup()` failures, the new plugin-command seam's `run_command` error path, scan/BPM plugin
  errors) against these three categories and fix whichever ones use the wrong channel.
- [ ] Check whether pausing the background scan with `B` (`ToggleScan`/`scan.set_paused`) actually
  inhibits *future and queued* track analysis/download, or only pauses whatever's in flight right now
  — i.e. does newly-added/queued work still get analyzed/downloaded while paused, or does it correctly
  stay queued until resumed?
- [ ] Find functionality that exists in the codebase but isn't currently bound to a key or command, and wire it up so it's reachable.
- [ ] Run an agent to collect and remove any placeholders of any kind. Write it in the memory to never write placeholders of any kind. Check the 
  todo for infra that is stubbed for unimplemented parts and remove it. Remove any reference for
  future features by moving them on the main todo list. never keep done items on the todo list.

- [ ] Split `ui/src/view.rs` (~4,700 lines: one `MedleyView` struct with ~180 lines of fields, a
  ~1,900-line `impl MedleyView`, a ~1,200-line `impl View` holding all of `draw`/`on_event`, and ~65
  free functions) into modules under `ui/src/view/`. Suggested seams, following the file's own
  `// ----` section markers and free-function clusters: `rows.rs` (`Row`/`Cell`/`Column`,
  `tracks_to_rows`, `render_cell`, `column_layout`, `draw_row_list`/`draw_list_body`, `list_title`),
  `status_line.rs` (`StatusLineLayout`/`StatusLineWidths`, `status_line_layout`, transport glyphs,
  `progress_bar`, plus the draw and mouse hit-test halves that must stay in sync — make them share one
  layout call instead of mirroring it), `panes.rs` (`Pane` layout/`split`, `draw_pane`, log scroll/pin,
  Queue/History docked panes), `settings.rs` (`SettingsEntry` and its draw/edit handling),
  `hotkeys.rs` (hotkey menu, capture modal, `bind_captured_key`), `playlists.rs` (`TopRow`,
  `RememberedPlaylist`, open playlist/remote navigation), `filter.rs` (`FilterCache`/`FilterRank`,
  local filter), `mouse.rs` (`handle_mouse`, `click_row`, double-click), `input.rs` (`Editing`,
  command line, `commit_edit`, key dispatch). Do it as pure moves first (per AGENTS.md: `sed`/`awk`,
  not retyping; one module per commit, build + clippy clean each time), keeping `MedleyView` one
  struct with `impl` blocks spread across the modules; only afterwards group its fields into
  per-concern sub-structs (`LogState`, `HotkeyUi`, `PlaylistNav`, …). Delete any `#[cfg(test)]` blocks
  encountered and trim multi-line doc comments to one line while moving. Best done before the
  architecture review below, so that review works on navigable files.
- [ ] Run a code and architecture review: make sure the program uses messages and reactive patterns
  to communicate between, and render, independent parts of the app (no part reaching into another's
  state or recomputing/polling per frame what an event should drive), and fix what doesn't. Then write
  a small (~4-8 KB) set of guides for further agents to follow, covering e.g. the app's architecture
  invariants and ways of working such as checking for excessive comments or inefficient/verbose
  implementations before pushing work.
- [ ] Run an agent to reduce code duplication and DRY violations, along with any
  violations of the user policies.
