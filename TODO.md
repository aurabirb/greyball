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

- [ ] After a restart the Playlists window shows only `[spotify] Liked Songs` — the user's Spotify
  playlists are missing (they used to list). Reproduce with the real Spotify login and read
  `medley.log` for the `GET /v1/me/playlists` call made at startup (status, item count, any
  401/429/timeout) and what `ViewCache` did with the result. Suspects, in order: (1) the top-level
  fetch is now kicked once right at startup (`Session::ensure_remote_playlists`, called from
  `app/src/main.rs` after `set_hotkeys`, and on `CoreEvent::PluginStatusChanged`) — possibly before
  the Spotify token has been refreshed/the source is fully wired, so the first fetch fails or comes
  back with only the synthetic Liked Songs row, and `ViewCache::ensure_remote_playlists`
  (`core/src/view_cache.rs`) then freezes the entry (`partial = false` whether the fetch landed
  `Ok` or `Err`; every later kick returns early) — the known "failed fetch is final for the
  session" bug above; nothing re-kicks when the Playlists window is opened any more; (2) the
  source's `browse` root returning Liked Songs first with the real playlists on a later page that
  the walk never requests or drops (`BrowsePage::partial`, `set_folders` replacing instead of
  extending); (3) a persisted/cached folder list being preferred over the fresh one. Fix the
  cause, and make it self-healing regardless: keep a failed or suspiciously short top-level fetch
  retriable, retry when the Playlists window is shown and when the source's health returns to
  `Ok`, with a floor between attempts. Hotkeys bound to remote playlists that aren't in the root
  list (the user's `a` binding) should still resolve a name in the Playlists/Help windows.
- [ ] Move the assigned key on Playlists top-level rows from the left gutter (the tags column before
  the name) to the right-hand hotkeys column, where track rows show their playlist letters
  (`Column::Hotkeys` in `render_cell`, `ui/src/view/rows.rs`; the top-level rows are built in
  `ui/src/view/track_list.rs` from `TrackList::top()`), same alignment and style as on track rows so
  the two read as one column when switching between a playlist's tracks and the playlist list. Applies
  to every Playlists-kind window (the tab and `playlist-keys`). Check it in a real-terminal
  screenshot of the `playlist-keys` float at its default size: the key must stay visible when the
  name is truncated in a narrow rect (the key column keeps its width; the name gives way).
- [ ] Floating windows get a real top border, in the normal text colour. Today `draw_float_frame`
  (`ui/src/view/panes.rs`) draws left, right and bottom lines plus corners but no top line — the
  window's own title row sits on the box's top edge (`float_body` starts at the frame's top row) —
  and colours the whole border `ColorStyle::title_primary()` (red) when focused. Change: draw the
  top `─` line like the other three sides and move the window body one row down inside it
  (`float_body` insets the top by 1 like the sides; `float_rect`'s minimum height grows by one), so
  the title row sits under the border as the first inner row; border always in
  `ColorStyle::primary()` (normal text colour), focused or not — focus stays visible through the
  title's existing `[title]`/highlight styling, not the border. Hit-testing keeps using the same
  `Placed.frame`/body rects as drawing (a press on the top border focuses/raises, like the other
  sides). Same two rules for the remaining fullscreen modals' frame (`draw_modal_frame`,
  `ui/src/view/modal.rs`) if they are ever drawn boxed; share one box-drawing helper between the
  two rather than keeping two. Padding: a floating window's content gets one blank column between
  the side borders and the text (left and right) and one blank row above the bottom border, so text
  never touches the box — most visible in the Help/hotkey window, whose command lane starts at the
  border and whose right-aligned shortcut lane ends on it. Do it in `float_body` (one inset for
  every floating window; windows still don't know they float) rather than per window; the Help
  window's lane widths and the lists' scrollbar gutter then derive from the padded rect. Judge the
  result in a real-terminal screenshot with two cascaded floats (one of them Help) over a list and
  over a docked pane.
- [ ] A focused floating window must own the keyboard: every key press goes to it and nothing falls
  through to the window or tab beneath. Seen: with the Help/hotkey window (or `playlist-keys`)
  floating and focused, pressing a playlist hotkey letter went through to the track list underneath
  and toggled that list's selected track in the playlist. Cause: `route` (`ui/src/view.rs`) offers a
  key to the focused window and, when the window returns `Ignored`, hands it to the shell
  (`on_shell_key` → `keybindings::map`), where playlist hotkeys, `q` enqueue, `+`, Space, seek keys
  etc. act on `active_list()` — the hidden list. New rule: while a `float` (or `screen`) window has
  focus, a key it ignores is dropped, except the small set of shell keys that are about windows
  themselves — the placement key (`M`), backtick (`toggle-playlist-keys`), `?` (toggle Help), `:`
  (command line), Tab/Shift-Tab focus cycling where the window doesn't use them, and Esc (close) —
  derive that set from the key table (`ui/src/items.rs`/`keybindings`), not a hand-copied list. A
  floating LIST window keeps acting on its OWN selection for track keys (playlist hotkeys, `q`,
  `+`, Enter): those must target the focused list, never the tab beneath — check `active_list()`
  resolves to the focused floating list. Docked windows keep today's behaviour (keys fall through
  to the shell, acting on the active list). Supersedes the earlier decision that shell/number keys
  fall through a focused float. No repro needed.
- [ ] Give the Help/hotkey window its own status bar: the last inner row of the window (above the
  bottom border/padding, inside its rect in every placement) shows the key instructions and the
  binding feedback, instead of borrowing the shell's hint row underneath the float. Idle: the
  instructions for the row under the cursor (`[Enter] rebind   [Backspace] default   [Tab] next
  section   [Esc] close`, or why this row can't be bound). Capturing: `press a key for <name> —
  [Esc] cancel`. After a bind attempt: the result — bound, moved from, refused (`'x' is a fixed
  key`, `already used by built-in …`), restored default — styled as a warning when it is a
  refusal, staying until the next key press in the window. Today these come from `Window::hint()`
  and `WindowOutcome::Flash` → the shell's single feedback slot (`ui/src/view/help.rs`,
  `notice.rs`, `hint_line`); route the Help window's own bind/refusal messages to its status bar
  (the window returns the outcome, the shell hands the formatted text back, or the window formats
  it itself — whichever keeps `notice.rs` the one place that words messages) and keep the shell
  slot for everything else. The window's layout memo reserves the row (list height = body − 1);
  draw and click hit-test share that layout. If the generic float frame later grows a footer slot
  any window can fill (`playlist-keys` has the same need: `[key] assign   [Backspace] clear`), build
  it once there rather than per window.

### Bugs
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
- [ ] Going to the previous track can fail with `symphonia error: Decoder channel closed` — the
  same track plays fine when clicked/Enter-ed in a list, but not when reached by going back from a
  different track. Reproduce and take the exact sequence from `medley.log` (which source/rendition
  both tracks were, cached vs streamed, which player — `RodioPlayer` or the Spotify player — handled
  each). The error text isn't in this repo, so first find which layer emits it (rodio's symphonia
  decoder, librespot's decoder thread, or a `Media::Reader`/`StreamingReader` whose feeding side was
  dropped). Where the two paths differ: `Command::Previous` (`core/src/app.rs`) takes the id from
  `queue.previous_from_history()`, wedges the current track back onto the queue front
  (`queue.play_next`) and calls `play_track(id, false)`, while a click goes through the list's
  play-in-context path — compare what each does before `load` (stop/teardown of the outgoing
  player, switching between players when the two tracks belong to different sources, reuse of an
  already-open reader/decoder or cached `Media` for a track that was played moments ago, the
  `generation` handling in `player/src/rodio_player.rs`, `pending_cache_fallback`). Suspects: the
  previous track's stream/decoder being torn down by the outgoing track's stop AFTER the new load
  started (a stale stop or a dropped channel racing the new load), or a history entry resolving to
  a rendition whose reader was already consumed/closed. Likely related to the rapid-skip bug
  above — fix them together if the cause is shared.
- [ ] A failed fetch of a source's top-level playlist list is final for the session:
  `ViewCache::ensure_remote_playlists` (`core/src/view_cache.rs`) sets `partial = false` whether the
  fetch landed `Ok` or `Err`, and every later kick returns early on `!entry.partial` — so starting
  offline (or a transient 5xx) leaves the Playlists screen without that source's playlists until
  restart. Keep a failed entry retriable (don't clear `partial` on `Err`, or track a failed state)
  and retry when the Playlists screen is opened and when the source's plugin health returns to
  `Ok` (`CoreEvent::PluginStatusChanged`), with a floor between attempts so a dead endpoint isn't
  hammered.
- [ ] Decided behaviour fixes, pass 2 (owner decisions; one pass):
  - After a direct key assign on a Playlists top-level row the cursor stays at the same row INDEX
    (the next playlist slides under it in the keyed-first `playlist-keys` window, so `a` `b` `c`
    binds three playlists in a row) instead of following the bound row; the flash names what was
    bound. `TrackList::select` keeps serving `:newplaylist`, `show_top` and Esc-back-out.
  - Direct assign stays enabled on the Playlists tab and the `playlist-keys` window alike, but a
    key assignment that would OVERWRITE something — the key is already on another playlist, or the
    target playlist already has a different key that would be replaced — first shows a confirm
    dialog naming both sides ("Move 'z' from X to Y?" / "Replace Y's key 'q' with 'z'?"); Enter/y
    confirms, Esc/n cancels, nothing changes until confirmed. Same for a rebind captured in the
    Help window. Binding a free key to an unkeyed playlist stays immediate. Reuse the unlike-confirm
    dialog pattern (`confirm_unlike`) rather than a new modal kind.
  Settled, no work: placement vocabulary stays `tabbed/embedded/screen/float` everywhere incl. the
  flash; float slots by id rank; `M` acts on the focused window; Esc closes a float only when it is
  focused; bare `:panes <mode>` moves every non-tabbed window except `playlist-keys`; backtick on an
  open unfocused `playlist-keys` focuses it first; the assigned key sits in the left gutter; a
  failed Enter-to-play stays a warnings row; a failed plugin setup shows popup + health row; press
  anywhere focuses a window; Esc in a filtered sub-playlist clears the filter first; `>`/`<` and
  arrow seeks stay fixed second keys; all commands live in the one Commands section.
- [ ] There is no way to delete (or rename) a local playlist. Add `:deleteplaylist <name>` (confirm
  dialog; drops its hotkey binding; windows showing it back out to the top level) and
  `:renameplaylist <old> <new>`, both through `Catalog`'s playlist write path so `playlists_gen`
  bumps; rows in the item table (`ui/src/items.rs`). The owner's database holds scratch playlists
  from agent test runs (`alpha`, `beta`, `gamma` twice each, `tmp1`, `tmp2`, `shuffletest`,
  `zz-scratch*`) waiting for this.
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
- [ ] Make almost every Help row bindable. Rows of `ui/src/items.rs` with `Key::Builtin` already are
  (Enter in the Help window captures a key, Backspace restores the default). Still without a key:
  the `:`-commands that take no argument (`log`, `settings`, `vis`, `queue`, `history`, `hist`, `link`,
  `unlink`, and `open` without its optional argument) — their rows answer Enter with "this command
  has no key" — and the actions `keybindings::fixed` hard-wires that are not structural: Space
  play/pause and `x` export. Give each a `core::BuiltinAction` (ids are additions to `state.toml`'s
  `builtin:<id>`), which means `BuiltinAction::ALL`'s default key becomes `Option<char>` (a command
  row starts with none; `effective_target_at`, `builtin_at`, `effective_hotkey` and `default_key`
  follow), `keybindings::map` runs the action, and `command::parse` can return the item's built-in for
  a no-argument command instead of one arm per word. Remove each from `fixed` as it becomes a built-in
  so `keybindings::taken` keeps knowing every taken key. Commands that TAKE an argument are bindable
  too: pressing the bound key opens the command line with the command's long name and a trailing
  space typed in (`:search ▏`, `:add-to-playlist ▏`), cursor ready for the argument, Enter runs it
  and Esc cancels — the path `+` already uses for `newplaylist ` (`Action::NewPlaylistPrompt`);
  generalize that one action to "prompt for item N" instead of adding one per command. A command
  with an OPTIONAL argument (`open`) prompts too; the user presses Enter on the empty argument to
  run it bare. Decided: `>`/`<` and the arrows seek stay fixed second keys for next/previous/seek
  (the rows say so in their detail line). `o` becomes the default key for `:open`; a persisted playlist
  binding on a new default key is dropped at load by `Session::set_hotkeys`, with a warnings row.
- [ ] The Help window breaks a command cell only at spaces, so in a lane narrower than an alias cluster
  (`:add-to-playlist/:add`, under about 22 columns: a float on a 60-column terminal, a side dock)
  the cluster is cut mid-word. Let the command cell break after a `/`. In a very wide rect the command
  lane grows to two fifths of the width for `:panes`' long argument list and pushes every description
  right; consider a tighter cap.
- [ ] With Help as the active tab nothing but a tab digit, `?` or the mouse leaves it: it consumes Tab
  for its sections and only a float or `screen` window closes on Esc. Decide whether a tabbed or docked
  Help should give Tab back to the shell's focus cycle.
- [ ] `core::config::PaneMode` (`panes.mode` in `config.toml`: `screen`, `embedded`, `float`) only seeds
  the five pane windows' placement for a default layout; merge it into `ui::screen::Placement` so the
  config takes `tabbed` too and there is one enum and one vocabulary.
- [ ] A `Screen`-placed window keeps every key but Esc and the placement key, so a fullscreen list has
  no `/`, `q`, `:` or number keys. Draw the hint row under a fullscreen window and let the shell keys
  through, or decide that fullscreen stays modal.
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
  `MedleyView::placed`, `list_h()` and the mouse row math, so the list gains the row; nothing else may
  assume the status row exists (check the warnings button, the row-count readout and the command
  line, which sit on the hint row above it). Toggling applies immediately, without a restart.
- [ ] Building on the status-row widget above: let it be placed either up top next to the
  tabs (replacing the redundant track-controls row that's currently up there) or down at the bottom,
  leaving only the command/help row at the bottom when it's moved up. Switchable via a toggle in the
  Settings UI (wired up there, not config-file-only).
- [ ] Hide the sources column on narrow terminal sizes.

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
