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
- [ ] Rapid skips advance the playlist pointer immediately, but while the newly selected track is
  loading or unavailable, the currently-playing title (and the rest of the now-playing readout) must
  keep reflecting the audio that is actually playing, and switch only when the new track's audio
  starts. Decide what the readout shows if the new track fails to load.
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
- [ ] Spotify has stopped recording listening history — investigate why (was working before; unclear
  which change, if any, broke it, or whether it's an account/API-side change).
- [ ] Check whether the background media scan is polling/ticking at a needlessly high rate and wasting
  CPU when idle. Design an algorithm that cuts down how often it checks while staying responsive —
  e.g. back off the poll interval the longer nothing's changed, waking immediately (not waiting out a
  slow interval) on an actual triggering event instead of polling for one.
- [ ] Streaming playback stage 2 (`docs/streaming-playback.md`), remainder: Soulseek (investigate whether
  slskd's partial file can be read safely), local files and the HTTP source checked in the running app.
- [ ] Streaming playback stage 3 (`docs/streaming-playback.md`): port the analyzers. `core::scan::
  open_scan_audio` returns `Error::Unsupported` for an uncached track (the scan of a track that is not in
  `MediaCache` fails until then); move it and `ScanDriver::prioritize` onto `StreamEngine::open(r,
  Intent::Fetch | Peek)`, add the progressive non-seekable `decode_blocks` variant, drop
  `Player::open_for_scan`/`scan_fetch_paused`, and make BPM and waveform work during the download.
- [ ] Spotify downloads that overlap (a skipped track still fetching during `GRACE` while the next one
  starts) sometimes stall near the end until librespot's 8 s read timeout; `SpotifyMediaProvider`'s reader
  reopens and finishes, but a stall while the playing track is the one behind shows as buffering. Find out
  whether librespot's fetch loop or the CDN causes it, and consider cutting a released Spotify fetch at once.
- [ ] A Spotify load requested while the link is down waits at most `START_TIMEOUT` (60 s, in
  `RodioPlayer`) before `LoadFailed`; the old player held it until the link came back.
- [ ] Resume after failure: when a stream fails mid-track (network drop), reopen the key (the same temp
  file keeps its ranges, the producer refills the gaps) and rebuild the decoder at the current position
  instead of `LoadFailed`; also covers Spotify's `Loaded::resume`.
- [ ] Gapless playback as a generic preload: `StreamEngine::open(next, Intent::Play)` shortly before the
  current track ends; `RodioPlayer` keeps `Player::preload`.
- [ ] A "downloading" mark on rows from the engine's stream status.
- [ ] There is no way to delete (or rename) a local playlist. Add `:deleteplaylist <name>` (confirm
  dialog; drops its hotkey binding; windows showing it back out to the top level) and
  `:renameplaylist <old> <new>`, both through `Catalog`'s playlist write path so `playlists_gen`
  bumps; rows in the item table (`ui/src/items.rs`). The owner's database holds scratch playlists
  from agent test runs (`alpha`, `beta`, `gamma` twice each, `tmp1`, `tmp2`, `shuffletest`,
  `zz-scratch*`) waiting for this.
### Features
- [ ] Audit the codebase for keys written as literal strings (`` ` ``, `P`, `M`, `+`, …) in Help text,
  hints, notices, doc strings and prompts instead of being resolved to the key currently bound to
  that action; make those resolve through the bindings (as `Chrome`'s `*_key` fields do) or list what
  can't.
- [ ] Add a "copy shared link" shortcut, default `y` (a rebindable `BuiltinAction` with a Help row):
  ask the item's source for its shareable web URL — not the raw medley/source URI — copy it to the
  clipboard and log it. Add a `Source` method for this (default: unsupported, with a notice saying so).
- [ ] Waveform overview (top bar, `WaveformPlugin` in `sources/waveform`): build the envelope
  progressively for a track still downloading — the playing track's bytes already land in a growing
  stream file (`core::stream`), so decode what has arrived with `core::audio_decode::decode_blocks`, publish the buckets filled so far and let the
  widget draw left to right as the download advances (undownloaded columns blank, each bucket's
  share of the track from the known duration); persist only once complete. SoundCloud tracks carry a
  ready-made `waveform_url` in the API response — use it there instead of decoding if it's cheap to
  wire.
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
- [ ] Make soundcloud provide explore page playlist in the playlists view
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
- [ ] Make sure the local filter (`/`) works in every panel that shows a track list — Now Playing,
  Playlists (and an open playlist), Search, History, Queue, and the similar-tracks panel above — and in
  Log, in every placement (tab, docked, floating), filtering that window's own list through the same
  `TrackList` path (Log through its existing filter). `/` on a window that has its own filter filters
  that window; it jumps to the Search window only from a window with nothing to filter (e.g. Settings).
  List the panels where it does nothing or acts on the wrong window, then fix them.

- [ ] Fewer steps in the Soulseek (slskd) setup, and a proper setup dialog for every plugin.
  Today a plugin's setup is a run of one-line prompts inside the warnings panel
  (`Editing::PluginSetup`, `ui/src/view/input.rs`; `Modal::Warnings`, `ui/src/view/modal.rs`,
  `ui/src/view/warnings.rs`; `Plugin::setup_prompt`/`setup`, `core/src/plugin.rs`; Soulseek's `Step`
  plan, `sources/soulseek/src/plugin.rs`).
  - Run every plugin's setup in a normal, closable modal (not the warnings panel), with no blue
    background on the prompts.
  - The modal is a chat log: every question and every answer the user gave is logged in order
    (mask password answers), scrollable. New warnings that arrive while it is open are tapped into
    and pasted into the log as they come.
  - The user can abandon at any time (Esc or a Close button), cancelling any setup step still running.
  - When setup finishes, do not close the modal on its own: show a "waiting for the source to be
    ready" line and close only when the source update reports ready (the plugin's `wiring()`
    applied and its health OK); on a failure keep it open with the reason.
  - Soulseek: the first question is "path to a local slskd directory, or press Enter to set slskd
    up". A path takes the existing-slskd flow, using what can be read from that directory
    (`slskd.yml`, `downloads/`) and asking only for what is missing. Enter (nothing) attempts the
    Docker way as today, with the normal prompts (folder, Soulseek login) and their defaults.
  - Keep the flow sensible and configurable but not overwhelming for non-technical users: plain
    language, one question at a time, the default shown in each prompt, questions that can be
    detected skipped, a short summary of what will happen before the final step, re-runnable from
    Settings, advanced options (custom host, manual login) only when asked for.

### Album support
Design: `docs/collections.md`.
- [ ] Opened collection title line: add `subtitle: Option<String>` to `BrowsePage` (filled by the
  source, e.g. "Album · 2019 · 12 tracks") and show it on the list window's existing title line, with
  `release_label(track_count)` (single ≤3, EP 4–7, else album) once the list is loaded. Also make the
  Playlists window's title unit follow the kind filter ("56 albums", not "56 playlists").

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
