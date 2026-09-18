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
- [ ] Remote playlist hotkey toggles (`ViewCache::toggle_remote_membership`, `core/src/view_cache.rs`)
  have only been exercised against a throwaway fake source, never a real Spotify playlist. On first
  real use, confirm in `medley.log` that one press sends exactly one add/remove, the letter goes
  italic then settles, and the open playlist gains/loses the row — then delete this.
- [ ] Like/unlike (`Session::set_liked`, `core/src/app.rs`) never updates the cached Liked Songs list,
  so an open Liked Songs view keeps a just-unliked row (and lacks a just-liked one) until restart,
  and no list shows a track's liked state at all. Route it through the same pending/settle path as
  `ViewCache::toggle_remote_membership` (a source needs to say where an add lands — Spotify puts new
  likes first, not last) and add a liked marker to the shared row builder (`tracks_to_rows`,
  `ui/src/view.rs`).
- [ ] A playlist hotkey can be bound to a key a raw handler in `MedleyView::on_event`
  (`ui/src/view.rs`) consumes first — seen with `x` (M3U export): the binding succeeds but the key
  never toggles. Refuse such keys in `bind_hotkey` like built-ins, or move those raw keys into
  `BuiltinAction` so the one table covers them.
- [ ] The screen's rightmost column (seen on macOS) holds stale cells and shows garbage after a window
  resize. Suspects, to check in this order: (1) cells nothing repaints — `draw_row_list`
  (`ui/src/view.rs`) pads the title row only to `content_w` (width minus the scrollbar gutter), so
  its last cell is never written, and `draw_list_body` paints no row text below `rows.len()`; tab
  bar, hint line and status line may have the same off-by-one against `printer.size.x` — every row
  `draw` owns should be written edge to edge each frame (or the view cleared first); (2) width
  disagreement with the terminal — `pad`/`truncate`/`five_col` measure with `unicode-width`, and a
  glyph macOS Terminal/iTerm renders wider or narrower than that (emoji, variation selectors, CJK,
  ambiguous-width box/transport glyphs like `━╍⏸`) pushes a line into or short of the last column,
  leaving leftovers cursive's diffing never rewrites; (3) resize handling — `Event::WindowResize`
  should force a full clear + redraw (`Cursive::clear`), and `last_screen_size`/`last_main_rect`
  must be refreshed before the first post-resize draw. Repro on macOS by resizing with a long list
  and wide-glyph titles on screen.
- [ ] Opening Help (`?`/`:help`) and scrolling far makes the app very busy and can lock it up, and
  playback sometimes skips to the next track while it's stuck. Likely cause: `help_lines`
  (`ui/src/view.rs`) rebuilds the whole help text from scratch on every `draw_help` frame AND on
  every scroll event (`jump_help`), each time taking the session lock twice and calling
  `s.playlists()`, `s.hotkeys()` and `self.top_rows(s)` — which walks every source's remote playlists
  (and can kick `ViewCache` fetches) — just to label hotkeys. Held-down/wheel scrolling queues events
  faster than that can run, so the UI thread spins while holding the session lock; the skip is
  probably the player/auto-advance path starved of that lock (or of CPU) long enough to read as an
  underrun/end-of-track — confirm in `medley.log` which path issued the advance and make it robust
  to a busy UI regardless. Fix: build the help lines once in `open_help` (rebuild only on a hotkeys/
  playlists-changed event or resize), keep `draw_help`/`jump_help` to slicing the cached lines, and
  coalesce queued scroll events before redrawing. Check the other fullscreen modals built the same
  way (`draw_warnings`, `draw_hotkey_menu`, the playlist picker) for the same per-frame rebuild.
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
- [ ] Make the top bar's now-playing title (the right-aligned `marquee` text `draw_tab_bar` draws in
  row 0, `ui/src/view.rs`) double as a scrubber. Additive only — nothing is replaced or removed: the
  bottom status line keeps its `━╍` bar, times and click handling as they are. Draw: leave the title
  exactly as wide as it renders today (the `scroll_title` result — no padding, no layout change) and
  underline (`Effect::Underline`, applied once — combining an effect twice toggles it off) the
  first `text_width * position / duration` cells of the visible text, splitting by display width on
  grapheme boundaries so a wide glyph is either fully underlined or not. The underline is positional
  over the visible text, so it stays put while a too-long title scrolls underneath it. Click: a left
  press on row 0 inside the drawn title's span `(start, text_width)` runs
  `Command::Seek(target - position)` with `frac = (x - start) / text_width`, the same math as the
  status line's scrubber branch in `on_event`; compute the span from one function shared by
  `draw_tab_bar` and the hit-test (like `transport_layout`/`transport_at_x`) so they can't drift.
  Unknown duration → no underline, clicks no-op. Check whether a click on that title already does
  something and keep it reachable. No helpers beyond what this feature itself calls.
- [ ] Let the bottom status line's scrubber grow leftward into spare width instead of staying a fixed
  `STATUS_BAR_WIDTH` (24, `ui/src/view.rs`). Today `status_line_layout` reserves the fixed bar and
  hands every spare column to the title field (`name_field_width`), which `pad`s a short title with
  blanks — those blank columns are the "available space on the left". New split: the title field
  gets only what the title needs (its display width, capped by what's left), and the scrubber takes
  the rest, up to a max of 80 columns; the right-hand block (`{totaltime}  {bpm} {shuffle}`) stays
  pinned where it is, so the bar's right edge doesn't move. No minimum width: the bar shrinks all the
  way to 0 as the terminal narrows (drop the constant rather than keeping it as a floor). When the
  width is too small to fit everything, the total time (and its separating space) disappears first,
  before the bar or title are squeezed further. Decide the bar width and whether the total time is
  shown inside `status_line_layout` and return them from it, so `draw` (`progress_bar(.., bar_w)`,
  the `format!` of the row) and `on_event`'s scrubber hit-test/seek math (`frac = (x - start) /
  width`) read the same numbers — both call sites currently pass `bar: STATUS_BAR_WIDTH` and
  `totaltime.width()` in; guard the seek math against a 0-width bar.
- [ ] Draw a two-row mirrored ("thick") waveform overview of the entire playing track — the whole
  song's amplitude envelope left to right, like SoundCloud's, not a live oscilloscope — in the top
  bar (`draw_tab_bar` in `ui/src/view.rs`), in the empty stretch between the tabs/transport cluster
  on the left and the right-aligned now-playing title, built from `▁▂▃▄▅▆▇█`. Thickness comes from
  inverting the palette on the second row: the upper row prints level `a` (1–8) normally, so the bar
  rises from that row's bottom edge; the lower row prints the complementary glyph `8 - a` with
  `Effect::Reverse`, whose inverted cell shows `a` eighths hanging down from that row's top edge —
  the two halves meet at the row boundary into one symmetric shape. E.g. upper
  `▅▄▃▃▃▂▂▂▆▅▄▃▃▂▂▂▇▆▄▃` over lower (reversed) `▃▄▅▅▅▆▆▆▂▃▄▅▅▆▆▆▁▂▄▅`. Edge levels: `a = 8` is `█`
  over a reversed space; `a = 0` is a plain space in both rows (don't print a reversed `█`). Reverse
  must swap the same fg/bg pair the upper row draws with, so both halves come out the same color —
  check it in a real-terminal screenshot (AGENTS.md), since `capture-pane` text can't show it. Rows:
  the top bar is one row today (`TAB_BAR_ROWS = 1`), so this needs a second one — grow the top bar
  to 2 rows (`TAB_BAR_ROWS` feeds the pane-band math in `split`/`shift` and the mouse row offsets;
  grep every use) with tabs, transport and title staying on row 0 and only the widget using row 1.
  Data: a per-track envelope computed once, not the live `AudioTap` — decode the whole file to PCM
  and reduce it to a fixed number of peak (or RMS) buckets (a few hundred `u8`s, normalized to the
  track's own max so quiet masters still fill the height), persisted with the track's other scan
  metadata so it's computed once per track. The scan-plugin seam already does this shape of work:
  `BpmPlugin` (`sources/bpm/src/lib.rs`) decodes via `core::audio_decode::decode_stereo_prefix`
  (first minute only — the waveform needs the whole track, so pass no frame cap / stream the decode
  in blocks rather than holding all PCM in memory) and stores its result as generic scan metadata;
  add the waveform as a sibling plugin on that seam. For a track that's still downloading, build it
  progressively instead of waiting for the file: the playing track's bytes already land in a growing
  file (`StreamingReader`/`open_streaming_url`, `player/src/rodio_player.rs`), so decode what has
  arrived, publish the buckets filled so far, and let the widget draw left to right as the download
  advances (undownloaded columns stay blank); each bucket's share of the track comes from the known
  duration. Persist only once complete. The now-playing track takes priority over background scans. SoundCloud tracks carry a ready-made `waveform_url` in the
  API response — use it there instead of decoding if it's cheap to wire. No envelope yet (not
  scanned, uncached stream, scan paused) → draw nothing. Render: resample the stored buckets to the
  widget's column count (max per column), map to 0–8. Draw the played part (columns left of
  `position / duration`) in the title/highlight color and the rest dimmed, so it reads as progress.
  Layout: the widget spans `transport_end + gap .. title_start - gap`, computed after the title
  (the title keeps priority and its current width; the widget just fills what's left and vanishes
  when fewer than a few columns remain, and on the collapsed-tabs layout if there's no room) — it
  must not shift the tabs, transport buttons, or title, nor their click targets. Redraw: it only
  changes on track change, new buckets arriving during a download, resize, and the played/unplayed boundary creeping
  along — `BASELINE_FPS` is plenty; don't raise the fps for it, and cache the resampled column
  levels per (track, width) rather than recomputing each frame.
- [ ] Remember the last-playing track across restarts and select it on startup. Persist it in
  `state.toml` (`app/src/main.rs`'s `save_state`/load path, next to volume and hotkeys): the track id
  plus the context it was playing from (screen and playlist — local id or remote `(source, node)`),
  updated when the playing track changes, not only on quit. On launch, if that track still resolves,
  open the context it came from and put the cursor on it the way `click_row` does
  (`self.cursor[screen] = idx; self.clamp_scroll()`), waiting for a remote playlist to paginate far
  enough if needed; if the context is gone, fall back to wherever the track can be found (library/
  Liked Songs), else do nothing. Select only — don't start playback.
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
