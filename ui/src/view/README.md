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
  data take `s: &Session` instead. The main frame in `draw` takes one lock, pulls a tuple of rows,
  titles, `StatusLine::snapshot(s)` and counts, then renders unlocked.
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
deliberate exception). Interior mutability in `draw` is limited to clocks and caches (`Marquee`,
`LocalFilter::cache`).

## Adding a component

1. New file here; a struct with only its own state; `Option<T>` on `MedleyView` if it is a modal.
2. One private layout fn; `draw` and `on_event`/`click` both call it.
3. `draw(&self, printer, data…)`; `on_event` returning `ListEvent` or a small outcome enum;
   `relayout` if it has a cursor or offset.
4. In `MedleyView`: a `draw_*`/`on_*_event` pair that fetches the data with one `with_session` and
   acts on the outcome; slot it into the precedence chains of `draw` and `on_event`; call `relayout`
   from `required_size`; open it from `handle_action` or `commit_edit`.
5. Reuse `ListState`, `Nav`, `modal_list_rect`, `text.rs` before writing scroll or width math.
