# View component model

`MedleyView` (`../view.rs`) is the only cursive `View` and is only the shell: screen layout (tab bar,
window rects from `PaneLayout::split`, hint row, status row), focus, the open `Modal`, command dispatch
(`run`) and the session locks. Everything that shows content is a window instance or a modal; the
files here are those components plus `impl MedleyView` blocks grouped by concern.

## Windows

- A `Window` (`window.rs`) is one instance of a component plus the `Rect` the shell last laid it out
  in — the one rect `draw`, hit-testing and `relayout` share. Its body is a `TrackList`
  (`track_list.rs`, every list kind: Now Playing, Playlists, Search, History, Queue), the `LogPane`,
  the `SettingsPane` or `Vis`.
- `Windows` holds every instance by `WindowId`: `Tab(Screen)` for the five tabs, `Pane(Pane)` for the
  five dockable panes. The Queue tab and the Queue pane are two instances of one kind, each with its own
  cursor, scroll window, filter and memos; nothing in a component may assume it is the only instance of
  its kind, that it is fullscreen-wide, or that it starts at column 0.
- Placement is where the shell shows an instance, not a property of it: the active tab (`screen`) in
  the main rect, `PaneLayout::open` panes docked around it, a `PaneMode::Screen` pane fullscreen as
  `Modal::Pane`. A Queue/History pane in `Screen` mode switches to its tab instead of layering.
- `Window` API, all of it called by the shell only:
  - `relayout(rect, s)` from `MedleyView::layout`: stores the rect; a list clamps its cursor into the
    list and re-follows it when the height changed.
  - `frame(ctx) -> WindowFrame`: the window's session-derived draw data, taken under the frame's one
    lock and memoized inside the component (`ListFrame` rows, Settings entries; Log and Vis are `Live`).
  - `draw(printer, focused, frame)`: windows the shell's printer to its own rect. The active tab's
    window is drawn unfocused: only docked windows show the `[title]` focus marker.
  - `on_event(event, ctx) -> WindowOutcome` (`Ignored`, `Consumed`, `Run(Command)`,
    `ToggleSetting(row)`): a mouse event outside its rect and any key it has no use for is `Ignored`,
    so the shell can offer it to the next window. Components never return `EventResult` or dispatch.
  - `Ctx` is what the shell hands a window under a lock: `&Session`, the live pane layout config and
    `searching`.
- `TrackList` owns its kind, `ListState`, where a Playlists window is (`Open`: top level, a local
  playlist, a remote one), its `/`-filter query and two memos. `reset_for_new_list` is the only way
  `Open` changes and `set_query` the only way the filter does; both reset the selection and bump
  `view_gen`, the list-identity part of every key below. Nav keys go through `ListState::on_event`
  (`Nav::of`); the wheel scrolls the window without the cursor (`ListState::scroll`); Enter or a
  double-click plays the row as `Command::PlayContext`, or opens the top-level playlist under the
  cursor; Esc backs out of an open playlist.

## Memos

`Memo<K, V>` (`memo.rs`) is the one-entry cache everything keyed here is built on
(`get_or_build(key, || value)` clones the value out, so values are `Arc`s; `Memo<K>::changed(key)` is
the value-less "did the key move" gate). Each memo is a field of whatever owns the cached thing, so a
second window never evicts the first's. Interior mutability in `draw` is limited to these, the `Marquee`
clock and the Log pane's `WrapCache`. When adding one, walk every `self.`/argument read under the
build closure against its key.

| memo | key | reads |
| --- | --- | --- |
| `TrackList::matches` (ranked filter ids) | `list_gen`, `view_gen` | the whole list, `query`, `open` |
| `TrackList::frame` (`ListFrame`) | `revision`, `view_gen`, offset, body height, `searching` | visible rows (attrs, now-playing, hotkey letters, pending marks), title, total |
| `SettingsPane::entries` | `revision`, pane layout config | config, volume, scan mode |
| `MedleyView::chrome` (`Chrome`) | `revision` | status core, warning count, help key |
| `MedleyView::follow_sig` | window id, `list_gen`, `view_gen`, cursor | — (gates `ScanDriver::follow_view`) |
| `HelpModal::built` | hotkeys, playlists and remote-playlists generations | hotkeys, playlist names (plugin commands are fixed at startup) |
| `PlaylistPicker::built` | playlists generation | the playlists |

`TrackList::list_gen` is the generation of the list on screen. Generations are held by the type that
owns the data and bumped next to the write in one private method, so no mutation site has to judge
what kind of change it made: `Queue` (`queue_gen`, `history_gen`), `ViewCache` (`results_gen`, one per
remote playlist in `edit_tracks`, one per source's folder list in `set_folders`), `Catalog`
(`playlists_gen` in `save_playlist`, the one write path for a user playlist, and `removed_gen` for a
track id that stops existing, which every `list_gen` adds in), `Hotkeys`, and `Session::context_gen`
for the Now Playing list. A cache keys on the generation of exactly what it shows; `revision` stays
the catch-all "something changed, redraw" that rows key on, because a row also shows attributes, the
now-playing mark, cached markers and hotkey letters.

The cursor is in no rows key: the selection highlight is applied at draw time, so moving within the
visible window rebuilds nothing, and a keypress in one window never rebuilds another's rows. Per-tick
data (playback position, the bpm tag, the `Marquee` clock, Vis levels, the Log pane) is never memoized
on `revision` — it is read fresh or kept in its own small cache.

## App interaction

- Reads: `with_session(|s| …)` locks, extracts owned values, unlocks. The mutex is non-reentrant:
  never nest, never call a locking `MedleyView` method inside the closure — methods that need session
  data take `s: &Session` (or a `Ctx`) instead. Where a window must be mutated under the lock
  (`send`, `layout`, `relayout_modal`) the shell locks a clone of the handle so `self` stays
  free. One lock per frame: `MedleyView::frame` collects every visible window's `WindowFrame`, the
  `Chrome` and the live `StatusLine`, then `draw` renders without the guard.
- No effects in `draw` or getters. Effects run on change from `MedleyView::layout`, which hands every
  visible window its rect under one lock and runs from `required_size` and again after every input
  event — cursive drains buffered type-ahead through `on_event` before its next layout pass, so a tab
  switched to or a pane docked in the same batch must already have its rect. `follow_scan` feeds the
  scan walk the active list — the focused window's, else the active tab's — from there, deduped by
  `follow_sig`, so a memo hit in `draw` can never skip it.
- `Session::revision` bumps on every UI-visible mutation, including an attribute-only `TrackUpdated`
  patch and every non-`Progress` player event; rows key on it so a scanned attribute shows up next
  frame. Anything that changes UI-visible session state outside `dispatch`/`on_event` must bump it, and
  an off-thread writer must send a `CoreEvent`, since nothing else will notice its mutation. An input
  event is not itself such a change and takes no mutable lock of its own, so a keypress that changes
  nothing rebuilds nothing.
- A library scan's flood of `TrackUpdated`s moves `revision` only, so it redraws the visible rows but
  never re-filters, re-follows or rebuilds Help; nor does a remote page landing re-filter another list.
- A row's cached-track marker (`Row::source`, `Session::is_track_cached`) only ever changes off a
  `TrackUpdated`/`Materialized` event, so every `MediaCache` write site must send one once the write
  lands.
- `Session::remote_playlists` is a pure read of whatever's landed; the fetch itself is kicked by
  `Session::ensure_remote_playlists` from two points, never from the getter or the view: once at
  startup (`app/src/main.rs`) and on `CoreEvent::PluginStatusChanged`. A landed fetch, clean or failed,
  is final for the session unless the source reported a partial page.
- Writes: `run(cmd)` → `Session::dispatch` → a `Dispatch` saying what happened (`Queued(n)`,
  `ShuffleSet(on)`, `Refused(why)`, …); `run` never re-reads the session to find out.
  `with_session_mut` is for settings calls (`bind_hotkey`, `set_source_enabled`). Slow work runs on a
  spawned thread and reports as a `CoreEvent` carrying its result (`MembershipResult`,
  `PluginCommandResult`); core keeps no message for the UI to poll.
- Messages: `Notice` (`notice.rs`) is the one place that decides where a `Dispatch` or a `CoreEvent`
  is shown — `Flash` on the hint row or a `Popup` dialog — and `MedleyView::notify` the one way to
  show it. The hint row has one slot, `MedleyView::feedback`, cleared by the next input event (not a
  mouse hold/release); the hotkey menu's footer shows the same slot. A window or modal never writes
  it: it returns an outcome and the shell notifies.
- Inbound: `app/src/main.rs` loops `siv.step()` → `bus.drain()` → `Session::on_event` →
  `ui::deliver(events)` → `siv.refresh()` when dirty; a bus send wakes `step()` through cursive's
  `cb_sink`. `deliver` is the only push into the view (the root is a `NamedView`, reached by
  `on_root`, which dialog buttons also use to `run` a command); everything else is re-read from the
  session by the next `draw`. `set_fps(BASELINE_FPS)` is the idle redraw floor
  (clock, marquee, title flush); `vis_fps_cb` raises it to `vis::FPS` while the Vis window is shown
  and must never go below the floor. `Event::Refresh` is ignored by `on_event`.

## Routing in `MedleyView::route`

1. Clear the `feedback` slot (not on mouse hold/release).
2. `on_edit_event`: an active text field (`Editing`) captures everything. A `/`-filter being typed is
   written through to the active list's `set_query` on every keystroke.
3. The open `Modal` (`modal.rs`), if any, takes every event: `MedleyView::modal` is one
   `Option<Modal>` (`Warnings`, `Picker`, `HotkeyMenu`, `HotkeyCapture`, `Help`, `Pane`), so there is
   no precedence to order — a modal swallows all input, hence nothing can open a second one.
   `draw_modal`/`on_modal_event`/`relayout_modal` are the only matches over it; a modal's `on_event`
   returns a `ModalOutcome` (`Stay`, `Close`, `Run(Command)`, `Setup(row)`, `Bind`/`Unbind`).
   `Modal::Pane` forwards keys to its window and drops mouse events.
4. Fixed-row mouse: row 0 → `TabBar::click`; bottom-2 → the warnings modal (anywhere on the row);
   bottom → `StatusLine::click`.
5. Any other mouse event: `send` offers it to each visible window; the one it lands in takes focus.
6. Keys: Enter on the focused warnings button opens the modal, any other key moves focus off it. Then
   `send` offers the key to the focused window, then the active tab's; what both ignore goes to
   `on_shell_key` (`Tab` cycles `focus_order()`, seek, `:`, `x`, backtick on a playlist, and
   `keybindings::map` / `hotkey_toggle` → `handle_action` with the active list's selection).
7. After `route` returns, `on_event` runs `layout()` and, for anything but a mouse event (a wheel
   scroll must stay put), `clamp_scroll()` re-follows the cursor in the active tab's and the focused
   window.

## Modals

A modal is constructed with its data at the open site (`HelpModal::new(s)`, `PlaylistPicker::new(id,
s)`): cursive drains every buffered input event through `on_event` before any layout pass, so
type-ahead must never meet an empty modal. One that snapshots session data also carries a `built`
stamp and re-reads in place from `relayout_modal` — never `draw` — only when the stamp moves while it
is open: Help keeps its scroll, the picker keeps its cursor on the same playlist. `draw_modal_frame(
printer, rect, title, footer)` draws the title bar and footer hint and returns the body printer;
`modal_body`/`modal_list` are the layout both draw and hit-test use, from the modal's `Rect`.

## Adding a component

1. New file here; a struct with only its own UI state — never the session or a sibling. A
   `ListState {cursor, offset}` (`scroll.rs`) for anything with a row cursor, a bare offset otherwise.
2. One layout fn from its `Rect`; `draw` and `on_event` both call it.
3. A window: a `Body` variant and an arm in each `Window` method. A modal: a `Modal` variant and an
   arm in each of `draw_modal`, `on_modal_event` and `relayout_modal`, opened with its data from
   `handle_action`.
4. Session data arrives as arguments (`Ctx`, `&Session`, a frame); outcomes go back as
   `WindowOutcome`/`ModalOutcome`. Anything crossing a component boundary goes through the shell.
5. Reuse `ListState`, `Nav`, `draw_row_list`, `draw_modal_frame`/`modal_list`, `Marquee`, `text.rs`
   before writing scroll or width math. `Screen` (`../screen.rs`) names the tabs and list kinds and
   owns every mapping over them.
