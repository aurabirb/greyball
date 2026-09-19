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
- [ ] Confirm Log-pane wheel scrolling is responsive under `RUST_LOG=debug` with a large log; if not,
  check wheel events queuing up behind draws (coalesce consecutive scroll events) and whether the
  `pane == Pane::Log` branch in `ui/src/view/mouse.rs`'s `handle_pane_mouse` swallows or mis-routes
  wheel events.
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
  `ui/src/view/rows.rs`).
- [ ] A playlist hotkey can be bound to a key a raw handler in `MedleyView::on_event`
  (`ui/src/view.rs`) consumes first — seen with `x` (M3U export): the binding succeeds but the key
  never toggles. Refuse such keys in `bind_hotkey` like built-ins, or move those raw keys into
  `BuiltinAction` so the one table covers them.
- [ ] The screen's rightmost column (seen on macOS) holds stale cells and shows garbage after a window
  resize. Suspects, to check in this order: (1) cells nothing repaints — `draw_row_list`
  (`ui/src/view/rows.rs`) pads the title row only to `content_w` (width minus the scrollbar gutter), so
  its last cell is never written, and `draw_list_body` paints no row text below `rows.len()`; tab
  bar, hint line and status line may have the same off-by-one against `printer.size.x` — every row
  `draw` owns should be written edge to edge each frame (or the view cleared first); (2) width
  disagreement with the terminal — `pad`/`truncate`/`five_col` measure with `unicode-width`, and a
  glyph macOS Terminal/iTerm renders wider or narrower than that (emoji, variation selectors, CJK,
  ambiguous-width box/transport glyphs like `━╍⏸`) pushes a line into or short of the last column,
  leaving leftovers cursive's diffing never rewrites; (3) resize handling — `Event::WindowResize`
  should force a full clear + redraw (`Cursive::clear`), and `last_screen_size`/`PaneLayout::main_rect`
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
- [ ] A track that appears multiple times in a playlist shows up as playing on every occurrence while
  it plays — only the one occurrence actually being played (by position in the context, not by track
  identity) should be marked.
  Suspected fix: `tracks_to_rows` (`ui/src/view/rows.rs`) sets `Row::current` from
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
- [ ] A playlist hotkey persisted in `state.toml` can shadow a built-in key (seen: `s` no longer
  toggles shuffle). `bind_hotkey` (`core/src/app.rs`) refuses to bind over a built-in, but
  `Session::set_hotkeys` loads the persisted map unchecked, and `effective_target_at` lets an explicit
  binding win over a built-in's default — so a binding made before a built-in claimed that key (or a
  hand-edited file) silently steals it. Suspected fix: validate in `set_hotkeys` — drop any
  non-built-in binding whose key is a built-in's effective key (its default unless that built-in is
  remapped elsewhere), push a warning naming the dropped binding, and let the next save persist the
  cleaned map.
- [ ] Skipping tracks very fast (holding/mashing next/prev) makes playback misbehave. Reproduce
  first and write down the exact symptoms from `medley.log` (wrong track playing vs. shown, audio
  of two tracks overlapping, stuck `Loading`, a skipped-over track starting late, extra
  auto-advances, history/scrobble spam, a burst of downloads/streams left running), then fix the
  cause rather than the symptom. Where to look: `Command::Next`/`Previous` → `advance(manual)` →
  `play_next_in_context`/`play_track` (`core/src/app.rs`), each of which starts a real load;
  `RodioPlayer`'s `generation` counter (`player/src/rodio_player.rs`) already drops stale `Loaded`
  results and stale `Finished` events — check the Spotify player (`sources/spotify/src/player.rs`)
  has the equivalent, that stale `PlayerEvent`s (`Finished`/`Stopped`/`Playing` from a superseded
  load) can't reach `on_player_event` and trigger `advance(false)` or overwrite `now_playing`, that
  `pending_cache_fallback` and history recording only fire for the track that actually ends up
  playing, and that superseded resolves/HTTP streams/cache downloads are cancelled instead of
  piling up. Likely shape of the fix: make skips cheap — move the cursor/now-playing immediately
  but debounce the actual load (~150–250 ms after the last skip) so only the final target is
  resolved, with every async result tagged by a load generation and ignored when stale.
### Features
- [ ] Make the top bar's now-playing title (the right-aligned `marquee` text `TabBar::draw` draws in
  row 0, `ui/src/view/tab_bar.rs`) double as a scrubber. Additive only — nothing is replaced or removed: the
  bottom status line keeps its scrubber bar, times and click handling as they are. Draw: leave the title
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
- [ ] Draw a two-row mirrored ("thick") waveform overview of the entire playing track — the whole
  song's amplitude envelope left to right, like SoundCloud's, not a live oscilloscope — in the top
  bar (`TabBar::draw` in `ui/src/view/tab_bar.rs`), in the empty stretch between the tabs/transport cluster
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
- [ ] Playlist hotkey rework — two parts, both built on the existing `HotkeyUi::capture`/
  `bind_captured_key`/`clear_captured_hotkey` path (`ui/src/view/hotkeys.rs`) and `Session::bind_hotkey`
  (`core/src/app.rs`). Purpose: mid track-sorting (often from the Queue or another pane, not the
  Playlists screen) get a quick reminder of which key is on which playlist, move a key to a
  different playlist, or add a playlist, without leaving the current view.
  (1) Direct assign on the Playlists screen: with a playlist row selected (local or remote —
  `selected_hotkey_target`), pressing any single-character key that isn't a built-in's effective key
  (`keybindings::builtin_at`) (re)assigns that key to the selected playlist immediately — no modal,
  no Enter — the counterpart of how a playlist key pressed on a track toggles membership. A key
  already on another playlist moves (report the steal in the status line, as `bind_captured_key`
  does). Keys this screen's raw handlers consume first (`x` export, `:`, Enter, …) stay theirs —
  see the raw-handler hotkey bug above; don't make them assignable.
  (2) `` ` `` from anywhere opens a second instance of the Playlists window, floating — a bordered
  box over the current view, not fullscreen (the floating mode from the per-window mode item
  below, which ships first — reuse it, don't build a second one) — with its
  own state but the same behavior as the tabbed one: own cursor/scroll, own open local/remote
  playlist and drill-in/back navigation, own filter, and every Playlists-screen key working in it,
  including (1)'s direct assign, Backspace clearing the selected playlist's key, `x` export and
  the existing scrolling keys/wheel. No duplication: extract the Playlists window out of
  `MedleyView` into its own type — state (`open_playlist`, `open_remote`, `remembered_playlist`,
  its `cursor`/scroll slot, filter) plus its row building (`top_rows`, `top_row_name`), drawing,
  key/mouse handling and `selected_hotkey_target` — that draws into whatever rect it's given and
  reports actions back, and have `MedleyView` hold two instances (tabbed, floating) of that one
  type; nothing Playlists-specific stays inline in `draw`/`on_event`, and no code path may assume
  there is only one. Shared data (`Session` playlists, `ViewCache`, hotkeys) stays shared, so an
  assign/rename/add in one instance shows in the other on the next draw. Differences are instance
  settings, not forks of the code: the floating instance sorts playlists with an assigned key
  first, then the rest (each group in its usual order), re-sorting after an assign with the cursor
  following its row; on open it preselects the playlist the user came from (an open local/remote
  playlist in the tabbed instance, or the playing context's playlist) at the top level — showing
  the list with keys, not drilled in — else keeps its previous cursor; Esc at its top level (and
  `` ` `` again) closes it, keeping its state for next time. The hotkey column must be visible in
  the list rows (add it to the shared row builder if the Playlists list doesn't show it already).
  The bottom hint line keeps its current role (hint ↔ bind/steal/refusal feedback) with the Enter
  step dropped. Adding a playlist mid-work comes for free through whatever the Playlists window
  already offers (`:newplaylist`); the new row should appear selected in the floating instance.
  Consequences to handle in the same change: `` ` `` is `BuiltinAction::OpenHotkeyMenu`'s default
  key and today opens the fullscreen built-ins remap menu — retarget that built-in to the floating
  Playlists instance; built-in remapping stays reachable through `:keys` (which the merged
  Help/hotkey window below takes over, along with `?`) — update `command::HELP`, the help text and
  the hint texts that mention either; delete the Playlists-screen backtick override
  in `on_event` and the standalone `open_playlist_hotkey_modal`/`draw_playlist_hotkey_modal`, which
  (1) and (2) replace. Do the extraction as its own pure-move commit(s) first (AGENTS.md: `sed`/
  `awk`, build + clippy clean), then add the second instance.
- [ ] Merge Help and the hotkey menu into one floating window opened by `?` (and `:help`/`:keys`)
  — the help screen doubling as the hotkey editor. It replaces both fullscreen modals: delete
  `HelpModal`/`help_lines`/`build_help_lines`/`on_help_event` (`ui/src/view/help.rs`) and
  `HotkeyUi`'s menu half (`menu`/`draw_menu`/`menu_rows` and the menu arm of `on_hotkey_ui_event`,
  `ui/src/view/hotkeys.rs`) rather than keeping either alongside. Floating = the floating window mode from the per-window mode item; reuse it.
  Content: one table of items, each `{command, description, shortcut}`, grouped into titled
  sections in this order: `:commands` first (`command::HELP` plus `Session::plugin_command_help`),
  then movement/navigation, then player controls, then everything else (panes/windows, playlist
  actions, playlist hotkeys, …). One source of truth, no duplication: today the same facts live in
  `command::HELP` (name, description), `keybindings::RAW_KEYS` (key, description),
  `BuiltinAction::label`/`default_key` (`core/src/app.rs`) and the alias table in
  `command::parse` — fold them into one item table (section, command spelling with aliases and
  args, description, the `BuiltinAction` or other bindable target if any) that this window, command
  parsing/alias help and key dispatch all read; an action that is both a `:command` and a key (e.g.
  `:open`/`o`, `:newplaylist`/`+`) is one row with both columns filled, never two rows.
  Layout: three column lanes — command, description, shortcut (right-aligned at the box's right
  edge, showing the live effective key, not the default). If no item in a section has a command,
  that section draws without the command lane and the description takes its width. Descriptions
  may be long or multi-line (first line a short summary, further lines detail); they wrap inside
  the description lane only, never under the command or shortcut lanes:
  ```
  [ Commands ]

  :open/:o [file/url]     open item                                              o
                          longer explanation wrapped to the description lane,
                          continuing on as many lines as it needs.

  :newplaylist/:n <name>  create a new playlist                                  +
                          more detail here.
  ```
  Precompute the layout once per (content, width), not per frame: lane widths per section from the
  widest command/shortcut cell, then each item's wrapped description and so its height in rows,
  with a uniform blank line between items and uniform spacing around section titles; cache the
  resulting flat line list plus each item's first-line index and rebuild only on resize or a
  hotkeys/playlists/plugin-commands change. `draw` and scrolling only slice that cache and never
  take the session lock per frame or per scroll event — this is also the fix for the Help
  scroll-lockup bug above; delete that bug entry when this ships (keep its "confirm which path
  issued the skip" part as its own bug if still unexplained).
  Navigation: the cursor moves item to item — only an item's first line is selectable/highlighted,
  its continuation lines scroll with it but are never a cursor stop; Up/Down/j/k, PgUp/PgDn,
  Home/End and the wheel as in the current menus; Tab/Shift-Tab jump to the next/previous section
  (move the cursor to its first item and scroll its title to the top of the box — just a scroll to
  position, no tab strip); Esc or `?` closes.
  Rebinding: Enter on a row starts the existing one-keystroke capture (`hotkey_capture`/
  `bind_captured_key`), Backspace clears that row's binding back to its default/none
  (`clear_selected_hotkey`), with bind/steal/refusal feedback on the box's bottom hint line as
  today. Almost every row should be bindable: rows backed by a `BuiltinAction` already are; give
  `:commands` that take no argument and the actions now hard-coded as raw keys in `on_event` a
  bindable target too (extending `BuiltinAction`/`HotkeyTarget` — this also settles the raw-handler
  hotkey bug above, since one table then knows every taken key). Rows that can't sensibly be bound
  (commands needing an argument, fixed keys like Esc/Enter/arrows) show their key but refuse Enter
  with a hint. Playlist hotkeys appear as a read-only-or-rebindable section fed from the same
  `s.hotkeys()` data as the floating Playlists window — don't build a second editor for them.
- [ ] Per-window mode toggle, so the layout can be rearranged: every window — the tab screens
  (`TABS` in `ui/src/view/tab_bar.rs`: Now Playing, Playlists, Search, History, Queue) and the panes
  (`command::Pane`: Log, Settings, Vis, Queue, History) alike — can be switched between four modes:
  **tabbed** (a tab in the top bar, shown in the main area when active), **docked** (a slice of the
  main screen beside the primary content — today's `PaneMode::Embedded`, laid out by `split`),
  **screen** (fullscreen over everything, Esc returns — today's `PaneMode::Screen`/`PaneLayout::fullscreen`),
  and **floating** (a bordered box over the current view, not fullscreen — this item owns that
  presentation; the playlist hotkey rework and the merged Help/hotkey window build on it).
  Today the two families are separate mechanisms: tab screens are fixed numbered screens that can
  only be tabs, panes have only `Screen`/`Embedded` (`pane_mode`/`pane_mode_overrides`, `:panes
  <pane> <screen|embedded>`, `toggle_pane`), and Queue/History exist twice — as a tab and as a pane
  bridged by `list_screen_for_pane`. Unify them: one window identity per thing, one four-value mode
  per window (extend `core::config::PaneMode`; no `Screen`/`Embedded` leftovers or aliases), one
  renderer per window that draws into whatever rect its mode hands it, with the tab bar
  (`tab_layout`/`draw_tab_bar`/tab click hit-test, number-key screen switching) built from
  whichever windows are currently tabbed instead of the fixed `TABS` array, and `focus_order`/
  `Focus` covering docked and floating windows. Toggle: a key (and `:panes <window> <mode>`) that
  cycles the focused window's mode tabbed → docked → screen → floating, applied immediately (not
  "next time it's toggled" as `pane_mode_overrides` is now), plus a way to target a window that
  isn't focused/visible. Floating windows need a size/position rule — start simple (centered,
  sized to a fraction of the screen, one at a time on top) rather than mouse drag/resize. Keep at
  least one window tabbed so the main area is never empty. Persist each window's mode (and open/
  closed state for non-tabbed ones) in `state.toml` next to volume and hotkeys (`save_state`,
  `app/src/main.rs`) and show it in Settings in place of the `panes.mode` info line. The
  code this reshapes is `PaneLayout` (`ui/src/view/panes.rs`). Do it before the playlist hotkey
  rework and the merged Help/hotkey window, in three stages, each shippable: (A) one rect-drawn
  component per window — including extracting the Playlists window into its own instantiable type
  — with tabs and panes unified under the three modes that exist today (tabbed/docked/screen), no
  new behavior; (B) the floating mode; (C) the toggle key, `:panes <window> <mode>`, persistence
  and the Settings display.
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
- [ ] Show a liked marker on track rows: a centre dot `·` at the left of the row, in the Tags
  column where the bpm value sits (`Column::Tags` in `render_cell`, `ui/src/view/rows.rs`; the
  `tags` cell of `Row`), for every track that is in a source's Liked Songs — in every list that
  uses the shared row builder (`tracks_to_rows`): library, search, queue, history, playlists.
  Give the dot its own fixed one-cell slot at the start of the tags cell (blank when not liked) so
  bpm values stay aligned whether or not a row is liked, and account for it in `column_layout`'s
  tags width. Liked state needs a cheap lookup: no list knows it today (see the like/unlike bug
  above — `Session::set_liked`/`liked_targets`, `core/src/app.rs`, only talk to the source), so
  keep a set of liked track ids in core, filled from each source's Liked Songs listing as
  `ViewCache` loads it and updated through the same pending/settle path that bug item introduces
  for like/unlike; rows read it under the frame snapshot, and a change bumps `Session::revision`.
  While a like/unlike is pending, draw the dot italic like a pending playlist-hotkey letter. Best
  done together with, or right after, the like/unlike bug fix.
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
  Suspected fix: all four end in `self.run(Command::Seek(±5000))` (`MedleyView::on_event`, `ui/src/view.rs`, for
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
  constant 2 and becomes 1 (hint/command line only), feeding `split`, `required_size`/
  `PaneLayout::main_rect`, `list_h()` and the mouse row math, so the list gains the row; nothing else may
  assume the status row exists (check the warnings button, the row-count readout and the command
  line, which sit on the hint row above it). Toggling applies immediately, without a restart.
- [ ] Building on the status-row widget above: let it be placed either up top next to the
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

- [ ] Make the UI event-driven instead of re-deriving everything per frame — the program should use
  messages and reactive patterns to communicate between, and render, independent parts of the app (no
  part reaching into another's state or recomputing/polling per frame what an event should drive).
  The component model is in `ui/src/view/README.md`; what breaks it today, in value order:
  - `Session::playlists()` is a store read transaction plus a clone of every playlist's `items`; per
    frame it runs in `rows` (Playlists top level), `list_title`/`context_name` (open playlist),
    the playlist picker's `draw`, `top_rows`, and `help_lines`. Give `Session` a playlist-names cache
    invalidated by playlist-mutating commands, or a revision counter the view keys a cache on.
  - Let `on_event` reuse the frame snapshot's (`ui/src/view/frame.rs`) cheap parts instead of
    re-locking (`TabBar` click, `StatusLine::snapshot` on click, `warn_count`).
  - While the Vis pane is open, the `Vis` worker's `session.lock().unwrap().audio_levels()`
    (`ui/src/vis.rs`) contends with the main session lock at 30 Hz; move audio levels behind their
    own lock/atomic instead of sharing the `Session` mutex.
  - Feedback text lives in three places with three lifetimes: `MedleyView::queue_feedback`,
    `HotkeyUi::feedback`, `Session::membership_feedback` (cleared by the UI through a lock on every
    keypress). Fold into one UI-side `Feedback` slot; deliver the async membership result as a
    `CoreEvent` payload rather than a polled `Mutex<Option<String>>`. Same for
    `take_plugin_command_result` in `app/src/main.rs`.
  - `run` (`input.rs`) infers feedback by matching the `Command` before dispatch and diffing
    `queue_len` after it; have `Session::dispatch` return the outcome (`Dispatch::Queued(n)`,
    `ShuffleSet(bool)`, `ScanMode(..)`) so the UI only formats it.
- [ ] Finish the list component in `ui/src/view`: the main list and the docked Queue/History panes
  bypass `ListState::on_event`. `MedleyView::on_event` hand-rolls Up/Down/j/k/J/K/PgUp/PgDn per focus
  kind (eight near-identical arms) instead of `Nav::of` + one `focused_list() -> (screen, view_h)`;
  `handle_mouse` and `handle_pane_mouse` (`mouse.rs`) duplicate each other's wheel/click/row math and
  rect-contains test, differing only in title-row offset; `clamp_cursor` and `bump_pane_cursor`
  (`lists.rs`) are the same clamp. Make a `TrackList` component (a `ListState`, its screen, one
  `body_rect`) whose `on_event` returns `ListEvent`, used for main and docked lists alike. That also
  removes the fixed `lists: [ListState; N_SCREENS]` slot array, the `usize` screen constants (make
  `Screen` an enum) and the single `PlaylistNav` slot that stop two views of one list kind coexisting.
- [ ] Modal plumbing in `ui/src/view`: the exclusive layers are five separate `MedleyView` fields
  checked in two hand-ordered `if` chains that disagree (`draw`: warnings, hotkeys, picker, help;
  `on_event`: warnings, picker, hotkeys, help), each with a `draw_*`/`on_*_event` wrapper repeating
  "fetch data under lock, call component, on close set `None` + `fallback_focus()`". Replace with one
  `modal: Option<Modal>` enum (`Warnings`, `Picker`, `HotkeyMenu`, `HotkeyCapture`, `Help`,
  `Pane(Pane)`) with a single `draw`/`on_event` match and a shared `ModalOutcome {Stay, Close,
  Run(Command)}`; give the modals a shared title/list/footer frame helper (each of `WarningsModal`,
  `PlaylistPicker`, `HotkeyUi::draw_menu`/`draw_capture`, `HelpModal`, `draw_screen_pane` prints its own
  title bar and footer hint); take a `Rect` instead of assuming the full screen from column 0.
- [ ] `commit_edit` (`ui/src/view/input.rs`) handles `command::Parsed` through a ladder of `if parsed ==`
  checks and duplicates `handle_action` (`Parsed::History` vs `Action::Screen(HIST)`, `Parsed::Keys`
  vs `Action::OpenHotkeyMenu`, `Parsed::Help` vs `Action::OpenHelp`). Map UI-level `Parsed` variants to
  `Action` and keep one `match`. "Reset for a new list" (`lists[..].cursor = 0; filter.query = None;
  clamp_scroll()`) is repeated in `activate` (twice), `open_playlist_uri`, the `Esc` arm of `on_event`,
  `Action::Screen` and `Parsed::History`; make it one method.
- [ ] `ui/src/lib.rs`'s crate doc describes a stateless three-screen view; rewrite it to point at
  `ui/src/view/README.md`. Then write a small (~4-8 KB) set of guides for further agents covering the
  app's architecture invariants and ways of working such as checking for excessive comments or
  inefficient/verbose implementations before pushing work.
- [ ] Run an agent to reduce code duplication and DRY violations, along with any
  violations of the user policies.
