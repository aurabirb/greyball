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
- [ ] (Low priority) The Search window doesn't behave the same when docked (or floating) as it does
  as a tab. Reproduce and list the differences first — candidates from the code: `/` and `:search`
  reach it through `show(id)` + the shell's search text field (`Editing::Search`,
  `ui/src/view/input.rs`), which was written for "Search is the active tab": where the query input
  is drawn and which window gets focus after Enter; results arriving while the docked window isn't
  focused; the `searching` flag in `Ctx`/the list frame key and the loading mark; Enter/`q`/playlist
  hotkeys acting on `active_list()` rather than the docked Search list; the leading-digit handling
  in the search field; Esc clearing results vs. closing; a second Search instance not existing, so
  `/` from another list jumps to the docked one. Make the window the unit: everything Search does
  as a tab it does in any placement, through the same `TrackList` paths.
- [ ] Remove the "closed" step from the placement key's cycle (`Windows::next_placement`,
  `ui/src/view/window.rs`; `cycle_placement`, `ui/src/view/panes.rs`): a startup tab's companion
  cycles docked → screen → float → docked and never ends in closed, because any non-tabbed window
  closes on Esc. `M` on the tab still opens its companion docked. The `[M] <next>` status hint stops
  ever saying "close", and the README's companion cycle text follows.
- [ ] Rework the status-row hints of every window (`TrackList::idle` in `ui/src/view/track_list.rs`,
  `HelpPane::idle`, the other windows' idle text). Every key shown comes from the effective bindings
  through the status context, never a hard-coded letter; a key the user has unbound is omitted
  together with its hint; the backspace key is always written `[Bksp]` (never `[Backspace]`, in
  hints and in Help rows alike). Exact texts:
  - Main Playlists tab, top level: the contextual assign OR clear hint — `[any key] assign` when the
    selected playlist has no key, `[Bksp] clear` when it already has one — then the switch key
    (`` [`] `` from `SwitchPlaylists`), and nothing else (no like, no `[M]`, no help).
  - Main Playlists tab, inside a playlist (its track list): `[l] like`, the queue key (`[w] queue`
    with the real binding), and `[<all playlist keys>] send to playlist` where the bracket holds
    every key currently assigned to a playlist as a compact run (e.g. `[abcdx]`; it is fine to
    truncate with an ellipsis when it does not fit). No `` [`] ``, no `[M]`.
  - Popout playlist window (`playlist-keys`), top level: `[any key] assign` / `[Bksp] clear` (same
    contextual rule), then `[Esc] close` and `[M] <next>`. Inside a playlist: `[<all playlist keys>]
    playlist` (the same run of assigned keys), `[Esc] close`, `[M] <next>`.
  - Now Playing (the first tab): `[?] help`, `[p/n] prev/next`, the queue keys (`[w/e] queue`, with
    the real bindings — the defaults are Enqueue `q`, Wedge `w`, ClearQueue `E`, so confirm which
    the owner wants shown), and `[P] cycle layout` only when something is docked.
  - Queue tab: mention the clear-queue key (`[E] clear queue`, the real `ClearQueue` binding).
  - Sweep the remaining windows (Search, History, Log, Settings; Vis stays blank) into the same
    `[key] action` style showing the few keys that matter in each.
- [ ] Unify like and unlike into one key called "like" that works exactly like a playlist hotkey:
  it toggles the selected track's membership in Liked Songs (`Session::set_liked` through the same
  pending/settle path as `ViewCache::set_remote_membership`, with the italic pending dot), so a
  liked track pressed again is removed. Remove the separate `BuiltinAction::Unlike` (default `L`),
  its Help row and its own confirm; the unlike direction asks for confirmation the way the
  playlist-key removal item below describes (one shared confirm path). `Like` stays `l`.
- [ ] Open the Help window as a TAB when `?` (`OpenHelp`, `:help`) is pressed, and let it follow the
  normal placement cycle afterwards (tabbed → docked → screen → float → tabbed, like any
  non-companion window), instead of opening as a float. `?` on the open tab returns to where the
  user came from; Esc rules follow the existing window rules (a tab does not close on Esc).
  `screen::HELP`'s `Home::Float` and everything that special-cases Help floating (its status hint,
  README) follow.
### Bugs
- [ ] The waveform never shows up: the top bar (`TabBar::draw`, `ui/src/view/tab_bar.rs`) should draw it
  in the second row between the player controls and the title on every wide enough terminal, and
  drop it entirely on small widths (no shrunken stub). Investigate why nothing is drawn: no
  envelope stored yet for the playing track (`WaveformPlugin`, `sources/waveform`; scan paused or
  cache-only mode `B`, the now-playing priority path, tracks that are streamed and never scanned),
  the available width between transport and title being too small because the title takes
  priority (at 120 columns the widget was 8 cells), or a draw/layout bug. Decide the layout rule
  (e.g. a minimum width below which it disappears, the title truncating to leave the waveform a
  reasonable share) and make it show for the currently playing track, including a track that is
  still being analysed (draw nothing until buckets exist).
- [ ] The Vis window leaves an empty row at its bottom that the owner does not want — likely the
  now-blank status row every window reserves (`Window::shows_status()` is false for Vis, but the row
  is still reserved). Give Vis's picture that row (the reserved row and the body must not depend on
  the row's content, so a window with no status text should not reserve one at all) without
  breaking the corner-slot declarations for other windows.
- [ ] If playback still sticks on a track's last second: `Session::on_player_event` now warns
  `player: ignoring Finished for <source> <uri>: not the current track` whenever an end-of-track
  event is dropped, so a stuck track with no such line in the Log pane means the player never sent
  `Finished` at all — capture `~/.local/state/medley/medley.log` under `RUST_LOG=debug` for that
  track (source, cached vs streamed, last `Progress` values) before changing `RodioPlayer::tick` or
  the Spotify player's `EndOfTrack` handling.
- [ ] A second `:s` started while the first is still streaming mixes both result sets:
  `CoreEvent::SearchHit(TrackId)` carries no search generation, so late hits from the superseded
  query are pushed into the new list (`Session::on_event` → `push_result`, `core/src/app.rs`). Tag
  hits and `SearchDone` with the search they belong to and drop the ones that are not current.
- [ ] Confirm Log-pane wheel scrolling is responsive under `RUST_LOG=debug` with a large log; if not,
  check wheel events queuing up behind draws (coalesce consecutive scroll events) and whether the
  Log arm of `Window::on_event` (`ui/src/view/window.rs`) swallows or mis-routes wheel events.
- [ ] Spotify: a real-world librespot AP death ("Connection to server closed.", upstream
  [#1151](https://github.com/librespot-org/librespot/issues/1151)/
  [#1486](https://github.com/librespot-org/librespot/issues/1486)) hasn't been observed against the
  background reconnect (`Link` in `sources/spotify/src/player.rs`), only deaths induced by cutting the
  AP socket through a local proxy. When one shows up in `medley.log`, confirm playback carried on
  ("reconnecting in the background" → "session connected", no `Stopped` in between), then delete this.
- [ ] Media keys still don't work on macOS, and the OS "Now Playing" status/widget never gets updated. Investigate whether this needs some form of app registration/packaging (e.g. macOS media-remote/`MPNowPlayingInfoCenter`/`MPRemoteCommandCenter` integration typically requires a proper `.app` bundle with an `Info.plist`/bundle identifier, not a bare CLI binary) — figure out and document the actual OS requirements needed to make this work, then implement whatever's missing.
- [ ] Remote playlist hotkey toggles (`ViewCache::set_remote_membership`, `core/src/view_cache.rs`)
  have only been exercised against a throwaway fake source, never a real Spotify playlist. On first
  real use, confirm in `medley.log` that one press sends exactly one add/remove, the letter goes
  italic then settles, and the open playlist gains/loses the row — then delete this.
- [ ] The screen's rightmost column (seen on macOS) holds stale cells and shows garbage after a window
  resize. Suspects, to check in this order: (1) cells nothing repaints — `draw_row_list`
  (`ui/src/view/rows.rs`) pads the title row only to `content_w` (width minus the scrollbar gutter), so
  its last cell is never written, and `draw_list_body` paints no row text below `rows.len()`; tab
  bar and status line may have the same off-by-one against `printer.size.x` — every row
  `draw` owns should be written edge to edge each frame (or the view cleared first); (2) width
  disagreement with the terminal — `pad`/`truncate`/`five_col` measure with `unicode-width`, and a
  glyph macOS Terminal/iTerm renders wider or narrower than that (emoji, variation selectors, CJK,
  ambiguous-width box/transport glyphs like `━╍⏸`) pushes a line into or short of the last column,
  leaving leftovers cursive's diffing never rewrites; (3) resize handling — `Event::WindowResize`
  should force a full clear + redraw (`Cursive::clear`), and `last_screen_size`/`MedleyView::placed`
  must be refreshed before the first post-resize draw. Repro on macOS by resizing with a long list
  and wide-glyph titles on screen.
- [ ] Playback has been seen to skip to the next track while the UI was stuck busy (e.g. during heavy
  Help scrolling, since fixed). Confirm in `medley.log` which path issued the advance and make it
  robust to a busy UI regardless — likely the auto-advance path misreading a lock/CPU stall as an
  underrun/end-of-track.
- [ ] Spotify has stopped recording listening history — investigate why (was working before; unclear
  which change, if any, broke it, or whether it's an account/API-side change).
- [ ] Check whether the background media scan is polling/ticking at a needlessly high rate and wasting
  CPU when idle. Design an algorithm that cuts down how often it checks while staying responsive —
  e.g. back off the poll interval the longer nothing's changed, waking immediately (not waiting out a
  slow interval) on an actual triggering event instead of polling for one.
- [ ] Question (analysis first, related to the duplicate-occurrence item below): when a track that
  occurs several times in a remote playlist is removed (hotkey toggle off, `ViewCache::
  set_remote_membership`, `core/src/view_cache.rs` ~551; `Source::remove_from_playlist`,
  `core/src/traits.rs`), every occurrence of it dims and then disappears. Answer, from the code:
  are the removal requests sent in parallel or in series, and how many per press? Does the source
  actually remove all occurrences from the playlist? Spotify's `remove_playlist_track`
  (`sources/spotify/src/webapi.rs` ~538) is documented as removing every occurrence with one
  `DELETE /v1/playlists/{id}/tracks` by URI — verify that claim, and check the other sources
  (soundcloud, local playlists) and the local-catalog removal path. Check whether the pending marker
  is keyed by track identity rather than by position (which is why all rows dim), whether the settle
  step removes rows by identity (all occurrences) or by index, and whether a later refetch can bring
  back occurrences the server did not remove. Write the answers down, then decide whether removal
  should target one occurrence (by position/`snapshot_id`) or all, and make the dimming and the
  settle consistent with that.
  A row is identified by (list, absolute index) — see `Session::playing_row`.
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
- [ ] There is no way to delete (or rename) a local playlist. Add `:deleteplaylist <name>` (confirm
  dialog; drops its hotkey binding; windows showing it back out to the top level) and
  `:renameplaylist <old> <new>`, both through `Catalog`'s playlist write path so `playlists_gen`
  bumps; rows in the item table (`ui/src/items.rs`). The owner's database holds scratch playlists
  from agent test runs (`alpha`, `beta`, `gamma` twice each, `tmp1`, `tmp2`, `shuffletest`,
  `zz-scratch*`) waiting for this.
### Features
- [ ] Confirm before a playlist hotkey removes a track: when pressing a playlist's key on the selected
  track would REMOVE it from that playlist (the track is already a member, so the toggle is a
  removal — local playlists and remote ones through `ViewCache::set_remote_membership`,
  `core/src/view_cache.rs`, the hotkey path in `ui/src/keybindings.rs`/`Session`), ask for
  confirmation first, exactly like the unlike key does (`Unlike`'s "Confirms first." flow — find
  its confirm dialog and reuse it, worded for the playlist: `Remove <track> from <playlist>?`).
  Adding needs no confirmation. Enter/`y` confirms, Esc/`n` cancels, and nothing is sent before
  confirmation (no pending marker, no request). One shared confirm path for both.
  Position-aware removal: when the playlist the pressed key belongs to is the one open in the focused
  list, the key removes only the highlighted OCCURRENCE (by its position in that playlist), not every
  instance of the track; the confirmation names it (`Remove <track> (row N) from <playlist>?`) and only
  that row dims and disappears. From any other view (a different list, Search, History) there is no
  position, so the key keeps meaning "remove the track from that playlist" and the confirmation says
  how many occurrences go when there are several. This needs a removal request that targets a
  position: local playlists remove by index; Spotify's `DELETE /v1/playlists/{id}/tracks`
  (`remove_playlist_track`, `sources/spotify/src/webapi.rs` ~538) currently removes every occurrence
  of a URI, so use its `positions` field with the playlist's `snapshot_id` (and
  `Source::remove_from_playlist` in `core/src/traits.rs` grows a position argument); the settle step
  and the pending dimming must be keyed by position too. Fix together with the duplicate-occurrence
  items (the playing-position bug and the removal question), which share the position-aware row
  identity.
- [ ] Let the bottom status line's scrubber grow leftward into spare width instead of staying a fixed
  `BAR_WIDTH` (24, `ui/src/view/status_line.rs`). Today `StatusLine::layout` reserves the fixed bar and
  hands every spare column to the title field (`name_w`), which `pad`s a short title with
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
- [ ] Waveform overview (top bar, `WaveformPlugin` in `sources/waveform`): build the envelope
  progressively for a track still downloading — the playing track's bytes already land in a growing
  file (`StreamingReader`/`open_streaming_url`, `player/src/rodio_player.rs`), so decode what has
  arrived with `core::audio_decode::decode_blocks`, publish the buckets filled so far and let the
  widget draw left to right as the download advances (undownloaded columns blank, each bucket's
  share of the track from the known duration); persist only once complete. SoundCloud tracks carry a
  ready-made `waveform_url` in the API response — use it there instead of decoding if it's cheap to
  wire.
- [ ] With Help as the active tab nothing but a tab digit, `?` or the mouse leaves it: it consumes Tab
  for its sections and only a float or `screen` window closes on Esc. Decide whether a tabbed or docked
  Help should give Tab back to the shell's focus cycle.
- [ ] `core::config::PaneMode` (`panes.mode` in `config.toml`: `screen`, `embedded`, `float`) only seeds
  the five pane windows' placement for a default layout; merge it into `ui::screen::Placement` so the
  config takes `tabbed` too and there is one enum and one vocabulary.
- [ ] A `Screen`-placed window keeps every key but Esc and the placement key, so a fullscreen list has
  no `/`, `q`, `:` or number keys. Let the shell keys through, or decide that fullscreen stays modal.
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
- [ ] Remember the last-playing track across restarts and select it on startup. Persist it in
  `state.toml` (`app/src/main.rs`'s `save_state`/load path, next to volume and hotkeys): the track id
  plus the context it was playing from (screen and playlist — local id or remote `(source, node)`),
  updated when the playing track changes, not only on quit. On launch, if that track still resolves,
  open the context it came from and put the cursor on it the way `click_row` does
  (`self.cursor[screen] = idx; self.clamp_scroll()`), waiting for a remote playlist to paginate far
  enough if needed; if the context is gone, fall back to wherever the track can be found (library/
  Liked Songs), else do nothing. Select only — don't start playback.
- [ ] Add a hard-redraw key (a `BuiltinAction` with a default key and a Help row in `ui/src/items.rs`,
  rebindable like the other built-ins; no `:` command, e.g. Ctrl-L if the key table can carry a
  control key, else a plain letter) that clears the whole screen (`Cursive::clear`, so nothing stale
  survives a garbled terminal) and makes every UI element refresh: the frame snapshot and per-window
  layout memos are invalidated (`Session::revision` bump or an explicit "everything dirty" flag),
  `last_screen_size` is re-read from the terminal, and the next draw rebuilds rows, tab bar, hint and
  status lines from scratch. The window-resize handler needs exactly this too (see the stale
  rightmost-column bug), so share one function between the two.
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
- [ ] Let the bottom status row (the last screen row: `{prev} {playpause} {next}  {title}
  {curtime} {scrubber} {totaltime}  {bpm} {shuffle}` — the `StatusLine` widget in
  `ui/src/view/status_line.rs`, drawn at the end of `draw` and hit-tested in `on_event`) be
  switched on or off from the Settings UI (a checkbox there, persisted like the other settings —
  not config-file-only). Its role is the compatibility-mode player interface — the plain fallback
  that works on any terminal/font, while the richer player UI lives in the top bar (title
  scrubber, waveform) — so its scrubber switches from the Unicode `━╍` bar to ASCII:
  `progress_bar` draws e.g. `=` for the played part and `-` for the rest (keep `-` for unknown
  duration; settle the exact characters in a real-terminal screenshot), removing the ambiguous-width
  glyphs the stale-rightmost-column bug above suspects. The playback control glyphs (`PREV_ICON`/
  `player_action_glyph`/`NEXT_ICON`) stay as they are for now. When off, the row isn't reserved at all: `BOTTOM_BAR_ROWS` stops being a
  constant 1 and becomes 0, feeding `split`, `required_size`/
  `MedleyView::placed`, `list_h()` and the mouse row math, so the list gains the row; nothing else may
  assume the status row exists (check the warnings button, which draws through `MedleyView::slots`, and
  the command line). Toggling applies immediately, without a restart.
- [ ] Building on the status-row widget above: let it be placed either up top next to the
  tabs (replacing the redundant track-controls row that's currently up there) or down at the bottom,
  leaving only the command/help row at the bottom when it's moved up. Switchable via a toggle in the
  Settings UI (wired up there, not config-file-only).

### Audits / cleanup tasks
- [ ] Check whether pausing the background scan with `B` (`ToggleScan`/`scan.set_paused`) actually
  inhibits *future and queued* track analysis/download, or only pauses whatever's in flight right now
  — i.e. does newly-added/queued work still get analyzed/downloaded while paused, or does it correctly
  stay queued until resumed?
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
