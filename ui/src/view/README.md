# View component model

`MedleyView` (`../view.rs`) is the only cursive `View`. It owns every component and the
`SessionHandle`; the files here are its components plus `impl MedleyView` blocks grouped by concern.

## Component API

A component is a plain struct owning only its own UI state — never the session or a sibling.

- State: a `ListState {cursor, offset}` (`scroll.rs`) for anything with a row cursor, a bare scroll
  offset otherwise (`HelpModal`, `LogPane`). A modal is an `Option<T>` field that is `Some` only while
  open; opening constructs it, closing drops it. `StatusLine` and `TabBar` hold no state: they are
  built per use from a session snapshot.
- `draw(&self, printer, data…)`: gets a `Printer` already `windowed` to the component's rect (the
  whole screen for a modal) and its data as arguments (`&[Playlist]`, `focused`, …).
- `on_event(&mut self, event, size, data…)` returns an outcome for the owner to act on: `ListEvent`
  (`Close`/`Activate`/`Clicked`/`Moved`/`Unhandled`) for list modals, `bool` "close" for `HelpModal`.
  `click(x, width)` on the one-row bars returns `Option<Command>` (`StatusLine`) or
  `Option<TabBarHit>` (`TabBar`). Components never return `EventResult` or dispatch.
- One layout function per component, called by both draw and hit-test: `StatusLine::layout`,
  `TabBar::layout`, `WarningsModal::list_rect`, `modal_list_rect(size, list_top)`,
  `HelpModal::view_h`, `PaneLayout::split`.
- `relayout(resized, size, len…)`, called from `MedleyView::required_size` (the one `&mut self` hook
  that knows the screen size): re-follow the cursor on resize, else clamp the offset to the data length.
- Shared: `ListState` + `Nav` (key/wheel → `(up, step)`), `Marquee` (one scroll clock for the tab bar
  and status line), `text.rs`, `draw_row_list` (`rows.rs`) for every track list.

## App interaction

- Reads: `with_session(|s| …)` locks, extracts owned values, unlocks. The mutex is non-reentrant:
  never nest, never call a locking `MedleyView` method inside the closure — methods that need session
  data take `s: &Session` instead. The main frame in `draw` (`frame.rs`) builds a `Frame`: a
  `CachedFrame` (rows, titles, settings entries, warning count, …) memoized on `FrameKey`
  (`Session::revision` plus every UI input that shapes it — screen, list identity, filter query,
  offsets/heights, open panes, `want_settings`, cursor, editing state) and shared out of the cache
  behind an `Arc`, plus this frame's live per-tick data (playback position/duration, the bpm tag) read
  fresh every time. Per-tick data (playback position, the `Marquee` clock, Vis levels, the Log pane)
  is deliberately never keyed on `revision` — it's read fresh or kept in its own small cache instead.
  Anything that changes UI-visible session state and doesn't already go through `dispatch`/`on_event`
  must bump `revision` itself, or the screen goes stale until the next keypress forces a cache miss.
  An input event is not itself such a change: `on_event` only bumps `revision` when it actually clears
  something (`clear_membership_feedback` checks the slot before touching), so a keypress or mouse move
  that changes nothing else causes no rebuild — off-thread writes (a plugin/scan/player thread) must
  still send an event, since nothing else will notice their mutation.
  - `Session` actually keeps two counters (`core/src/app.rs`). `revision` bumps on every UI-visible
    mutation, including a `TrackUpdated` attribute-only patch (BPM landing, a cache fill, …) and
    per-`Player` event — the frame stays keyed on it, so a scanned attribute still shows up on a
    visible row next frame. `list_revision` bumps only when list membership/order/identity or a
    displayed name actually changes (search results, queue/history, playlist create/add/remove/
    membership, remote pages landing, hotkey/playlist-name changes) and is left untouched by a plain
    attribute patch. `LocalFilter::cache`, `MedleyView::follow_sig` (`follow_scan`'s dedupe key) and
    the Help/playlist-picker snapshots below key on `list_revision` instead of `revision`, so a
    library scan's flood of `TrackUpdated`s doesn't force a full re-filter/re-follow/rebuild per
    track — only the frame (and thus the visible rows) redraws.
  - A row's cached-track marker (`Row::source`, `Session::is_track_cached`) only ever changes off a
    `TrackUpdated`/`Materialized` event, so every `MediaCache` write site (the player's streamed
    downloads, a scan plugin's own fetch, Spotify's background materialize-to-cache copy) must send
    one once the write actually lands — a cache fill with no matching event leaves the marker stale
    until something unrelated bumps `revision`.
  - `follow_scan` runs from `required_size` (every layout pass), not from `build_cached_frame` — a
    frame-cache hit must not skip it, since the visible list/cursor it feeds `ScanDriver::follow_view`
    can change (focus, a docked pane's cursor) without anything `FrameKey` is keyed on changing.
  - `Session::remote_playlists` is a pure read of whatever's landed; the fetch itself is kicked by
    `Session::ensure_remote_playlists` from two points, never from the getter or the view: once at
    startup (`app/src/main.rs`, so Help and playlist hotkeys can name remote playlists on any screen)
    and on `CoreEvent::PluginStatusChanged` (a source logging in later still gets loaded). A landed
    fetch, clean or failed, is final for the session unless the source reported a partial page.
- Writes: `run(cmd)` → `Session::dispatch` → `EventResult` (consumed, quit, or a `popup`).
  `with_session_mut` is for settings calls (`bind_hotkey`, `set_source_enabled`). Slow plugin work
  runs on a spawned thread and reports through the `Bus`.
- Inbound: nothing is pushed into the view. `app/src/main.rs` loops `siv.step()` → `bus.drain()` →
  `Session::on_event` → `siv.refresh()` when dirty; a bus send wakes `step()` through cursive's
  `cb_sink`. The next `draw` re-reads the session. `set_fps(BASELINE_FPS)` is the idle redraw floor
  (clock, marquee, title flush); `vis_fps_cb` raises it to `vis::FPS` while the Vis pane is open and
  must never go below the floor. `Event::Refresh` is ignored by `on_event`.

## Routing in `MedleyView::on_event`

1. Clear one-keypress feedback (not on mouse hold/release).
2. `on_edit_event`: an active text field (`Editing`) captures everything.
3. Exclusive layers, first match wins: fullscreen pane → warnings → playlist picker → hotkey capture
   → hotkey menu → help. `draw` checks in the order fullscreen pane, warnings, hotkeys, picker, help.
4. Fixed-row mouse: row 0 → `TabBar::click`; bottom-2 → warnings button; bottom → `StatusLine::click`.
5. Rect mouse: `handle_mouse` (`panes.main_rect`), then `handle_pane_mouse` per `panes.rects`; a click
   sets `focus`.
6. Keys: `Tab` cycles `focus_order()` (`Main`, each docked pane, `Warnings` when any plugin warns);
   nav keys go to the focused list or pane; everything else goes through `keybindings::map` /
   `hotkey_toggle` → `handle_action`.
7. `clamp_scroll()`.

## Between components

Components do not know each other. Anything crossing a boundary goes through `MedleyView`: it reads
one component's outcome and mutates another (`ListEvent::Activate` from the picker →
`run(AddToPlaylist)`; closing a modal → `fallback_focus()`). `MedleyView` hands a component its data
and rect; a component never reaches into `lists`, `panes`, `focus` or a sibling, never locks the
session in `draw`/`on_event`, never stores session data past one call (`PlaylistPicker::track` is the
deliberate exception; `HelpModal`'s lines and `PlaylistPicker`'s playlist list are snapshots too, but
self-heal — see below). Interior mutability in `draw` is limited to clocks and caches: `Marquee`, the
Log pane's incremental `WrapCache`, and `Memo<K, V>` (`memo.rs`) — the one-entry cache every keyed
cache here is built on (`get_or_build(key, || value)` clones the value out, so values are `Arc`s;
`Memo<K>::changed(key)` is the value-less "did the key move" gate). Each memo is a field of whatever
owns the cached thing, never a global. The memos: `LocalFilter::cache` — keyed on `list_revision`,
not a source length, so a same-length content swap still recomputes, and holding matched ids rather than `Track`s so a `TrackUpdated` attrs patch can't
go stale inside it — `MedleyView::follow_sig` — ditto, dedupes `ScanDriver::follow_view` reports — and
`MedleyView::frame_cache`, the `FrameKey`-memoized `CachedFrame`, keyed on `revision`).

A modal that snapshots session data while it stays open (`HelpModal`, `PlaylistPicker`) opens empty
with a `built: Memo<u64>` stamp and is filled from `MedleyView::required_size` — never per-draw, and
always before its first draw — whenever `Session::list_revision` differs from the stamp
(`MedleyView::refresh_help`/`refresh_playlist_picker`); both replace their content in place rather than
resetting it — `HelpModal::refresh` keeps `scroll` (re-clamped to the new line count) and
`PlaylistPicker::refresh` re-clamps its cursor onto the same playlist id (or the new length if that
playlist is gone) and re-follows it into view.

## Adding a component

1. New file here; a struct with only its own state; `Option<T>` on `MedleyView` if it is a modal.
2. One private layout fn; `draw` and `on_event`/`click` both call it.
3. `draw(&self, printer, data…)`; `on_event` returning `ListEvent` or a small outcome enum;
   `relayout` if it has a cursor or offset.
4. In `MedleyView`: a `draw_*`/`on_*_event` pair that fetches the data with one `with_session` and
   acts on the outcome; slot it into the precedence chains of `draw` and `on_event`; call `relayout`
   from `required_size`; open it from `handle_action` or `commit_edit`.
5. Reuse `ListState`, `Nav`, `modal_list_rect`, `text.rs` before writing scroll or width math.
