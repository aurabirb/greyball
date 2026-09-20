# View component model

`MedleyView` (`../view.rs`) is the only cursive `View` and is only the shell: screen layout (tab bar,
window rects from `MedleyView::placed`, the scrubber line), focus, the open `Modal`, command dispatch
(`run`) and the session locks. Everything that shows content is a window instance or a modal; the
files here are those components plus `impl MedleyView` blocks grouped by concern.

## Windows

- A `Window` (`window.rs`) is one instance of a component plus the `Rect` the shell last laid it out
  in — the one rect `draw`, hit-testing and `relayout` share. Its body is a `TrackList`
  (`track_list.rs`, every list kind: Now Playing, Playlists, Search, History, Queue), the `LogPane`,
  the `SettingsPane`, `Vis` or the `HelpPane`.
- `Windows` is an open store: `WindowId` is an opaque index that encodes neither kind nor placement,
  and `Windows::add(startup, placement)` builds an instance of any `Kind` (`../screen.rs`: `List(ListKind)`,
  `Log`, `Settings`, `Vis`, `Help`; `Kind` stays nested because a `TrackList` matches exhaustively over its five
  `ListKind`s). Startup adds one window per entry of `screen::WINDOWS`, which gives each the name
  `:panes`, `:window` and `state.toml` know it by: the five tabs (`now-playing`, `playlists`, `search`,
  `history-tab`, `queue-tab`), the three panes (`log`, `settings`, `vis`), the tabs' companions (`playing`, `results`,
  `queue`, `history`, and `playlist-keys` for Playlists) and `help`; `playlist-keys` and `help` start floating and closed. A `Startup` entry also says where the window is first placed (`Home`: a tab, a pane
  placed by `Config::panes`, or floating) and carries its instance settings. The Queue
  tab and the Queue pane are two instances of one kind, each with its own cursor, scroll window, filter
  and memos; nothing may assume a window is the only instance of its kind, that it is a list, that it
  is fullscreen-wide, or that it starts at column 0. Nothing looks a window up by kind: `Windows::named`
  resolves a name to the id startup gave it.
- Each window has a `Placement` (`Tabbed`, `Docked`, `Screen`, `Floating`; `tabbed`, `docked`, `screen`,
  `float` wherever the user reads or types one), held in `Windows::placements` apart from the windows
  so a `Ctx` can lend the Settings pane all of them while one window is borrowed mutably;
  `Windows::place` is its one write and bumps `Placements::generation`. `MedleyView::set_placement` is
  the one caller: it keeps `tabs` and `open` in step, keeps a shown window shown and a focused one
  focused. A startup tab never moves: a `Startup::companion_of` window is its companion, an instance of
  its own (cursor, filter, memos) that never joins the tab bar, and every placement asked of the tab
  (`M`, `:panes <tab>`) goes to the companion, opening it if closed. `:panes [<window>] <placement>` goes through it
  (`tabbed` closes a companion, as an explicit command; without a name: every window that is not a tab right now, but for a `Home::Float` one, whose job is
  to float; to `screen` it only closes an open window), and so does the
  `CyclePlacement` key (`M`), which moves the focused window Tabbed → Docked → Screen → Floating and
  flashes the new placement; on a startup tab it opens and focuses its companion docked, on a
  companion it goes Docked → Screen → Floating → Docked and never closes (Esc closes it, as any window that is not tabbed).
- The tab bar is `MedleyView::tabs`: the `Tabbed` windows in startup order, a window moved to `Tabbed`
  appended, never empty. `active` is the one shown; when it moves away its right neighbour takes over.
  After the tabs come the transport buttons (`transport_layout`, shared by drawing and `TabBar::click`): shuffle (red when on), prev, play/pause, next; a click runs `Transport::action`. The buttons drop out together when they don't fit. While a track plays, a `♥` sits one column left of the title (`Layout::heart`, taking 2 columns from the title's room; omitted with under 4 title columns left): dim, red when liked, italic while a like is in flight; a click runs `Action::LikePlaying` (same as `=`: `Command::Like` on the playing track, asking first to unlike).
  `1`-`9` and a tab click select by position (`Action::Tab`) and focus that tab's window, whatever had focus; `tab_names` labels them by kind. `MedleyView::open` lists the open non-tab windows, oldest first, which is both dock
  order and z-order, each with the focus it opened over. `show(id)` brings any window into view (its
  tab, else opened and focused) and is what `/`, `:hist`, `:search` and `:open <playlist link>` use; `toggle_window`
  (`:window <name>`, `:log`, …) opens or closes a non-tab window and switches to a tabbed one.
  `edit_search(id)` then takes a Search window's query input (a tab digit or click on the Search tab, and `/`).
- `playlist-keys` is a second Playlists window, the key overview meant to be opened mid-work: it
  starts floating and closed, and differs from the Playlists tab by two instance settings,
  `Startup::keyed_first` (playlists with a key are listed first, each group in its usual order; the
  window `Windows::keyed_first` finds).
  The `SwitchPlaylists` key (backtick) runs `switch_playlists`: when that window is shown it takes focus (the
  open entry remembers the focus it came from, and the key on the focused one returns there, else to the
  Playlists tab, and closes it only when it floats); else `TrackList::show_top` and `show` — back at its top level, the cursor on the playlist the user
  came from (`Session::playing_playlist`, else the one open in the active list, else in any other window),
  else on the playlist it was left in, else where it was. Its status row shows the
  assign/clear/open hint (`ListFrame::assignable`: a Playlists top level with rows), plus the closing key
  while it has focus. Everything else — placement, `:window`,
  `M`, Esc closing a focused window that is not tabbed, persistence — is what any window has.
- An open playlist's title row shows a `<back` button over the tags column (`rows::BACK_LABEL`, hidden when narrower than the title indent); a left press on it runs `show_top`. Mouse only: not focusable, no key.
- `help` (`help.rs`, `HelpPane`) is the help screen and the key editor in one: the `OpenHelp` key (`?`)
  and `:help` run `toggle_help`: closed, it opens as a tab and takes focus (`MedleyView::help_from` remembers the active tab and focus it came from); `?` on it as a tab removes it from the tab bar and goes back there; in any other placement `?` on it closes it, and when it is open but unfocused focuses it. After that it follows the placement cycle like Log and Settings. Closed, its placement is floating.
  Its content is `../items.rs`, the one table where a `:`-command's two spellings (one long, one
  short, by convention rather than by type), arguments and
  description and a key's description are written: `command::parse` resolves both spellings and builds
  usage errors from it, `items::describe` names a built-in in bind feedback, and a built-in's default
  key stays in `core::BuiltinAction::ALL` (a command row with no default key starts unbound). Structural keys (`keybindings::fixed`, `Nav`, the shell's Tab
  and arrows) are matched as events in code; their rows only describe them. Sections come in
  `Section::ALL` order — Player, Custom playlist hotkeys, Tracks and playlists, Windows, Movement,
  Commands (plugin commands appended) — then the playlists that have a key. `build` lays a section out in two lanes (description, shortcut showing the live effective key; one cell of it, the first, is underlined on every row whose key can be set, an unbound one included), the
  shortcut lane one width shared by every section (so all keys share a column); an item's `:command`
  spelling is the first line of its description block, wrapped with it, and lines after the first are indented two spaces. The result, a flat
  line list plus each row's `first..end` line span and each section's title line, is cached in
  `HelpPane::built`; `draw` and scrolling only slice it. The cursor is a row index and the scroll
  offset a line index: only a row's first line is a cursor stop or highlighted, `follow` keeps the
  whole item in view, and `relayout` re-follows when the layout key or body height moved (a rebind
  re-wraps). Floating or fullscreen, a focused Help window jumps sections with Tab and Shift-Tab (title
  to the top), so focus leaves it by `?`, Esc, a tab digit or the mouse; tabbed or docked, Tab and
  Shift-Tab cycle focus as anywhere, and Esc closes it unless it is tabbed, where it does nothing (its status row words the keys to match). Enter on a row with a bindable
  target stores that target and its name in `capturing`, so what the next character binds is what the
  prompt named whatever rebuilt meanwhile; every key is consumed while capturing, a non-character
  cancels, and `blur` (from `close_window`, `focus_window`, `toggle_help`) or a mouse event drops it.
  Bksp is `Unbind`: a playlist loses its key, a built-in returns to its default, which
  `clear_hotkey` refuses while another binding holds that default. Its idle status row text is the capture prompt, else what the row under the cursor allows or why it has no key.
- Every window but Vis reserves its last row as its status row (`Window::shows_status`, decided by the window's kind; Vis has no row and draws its picture over the whole rect, and offers no corner slot) (`Window::content` is the rect without it, which is what the component lays out, draws and hit-tests in; a click on the row is
  consumed). `Window::draw` fills it with the last bind result (`Notice::Status`, styled as a warning when refused, cleared by the next key or blur), else the flash (`MedleyView::feedback`) when the window has focus, else the component's idle hints, every key from the effective bindings in `Chrome` (an unbound key omits its hint): the main Playlists tab shows `[any key] assign` (or `[Bksp] clear` when the selected playlist already has a key) and `[`] playlist keys` at the top level, and `[l] like   [q/w] queue   [<assigned keys>] send to playlist` inside a playlist, with no `[Esc]` or `[M]`; the playlist-keys window shows the same assign/clear hint or `[<assigned keys>] playlist`; Now Playing `[?] help   [p/n] prev/next   [q/w] queue   [0] show playing` plus `[P] cycle layout` while a window is docked; Queue adds `[E] clear queue`; Search, History, Log and Settings show their few keys; the assigned-keys run is cut with `…` to the width `StatusCtx::keys_run` has left, the other hints first; `[Esc] close` where Esc closes it; then `[M] <next placement>` (the `CyclePlacement` key and `Windows::next_placement`) while it has focus. The row is always one row high and its existence never depends on its text, and what fills it (message, flash, hints, count, the warnings button, the input line) only takes columns inside it: the body's layout, memo keys and hit-test rects depend on the row being reserved, never on its text or on what occupies it, and the row is redrawn from its own inputs each frame. A list also draws its `cursor/total unit` count (`TrackList::count`) at the right end, which the idle hint gives way to and a message does not. The shell lends the row a `StatusCtx` per frame: the focus-only parts, the keys and the cells at the right end the warnings button covers. `Notice::Status` and the flash go to the focused window's row (`MedleyView::status_id`), or the active tab's when the focused window has no status row (Vis).
- Corner slots (`corners.rs`): each surface declares which bottom corners it offers (`screen::Corners`): a window both (`Window::CORNERS`), the scrubber `StatusLine` the right one, a modal none (`Modal::CORNERS`); while the scrubber line is shown, a window docked along the bottom edge (`pane_cfg.side` `Bottom`) offers none. `MedleyView::slots` is the one function that picks, per corner, the scrubber line when it is shown (`MedleyView::status_line`) and offers it, else the nearest offering window's status row, and none under a modal. The status group is drawn through it (`status_widget`: the one-letter analyzer tag (`B` when scanning may fetch, `b` cache-only or off; background green while the playing track is being analyzed, red once it errored), click toggles the scan, always left of the warnings button when there are warnings, at the right end of the slot's row; on the scrubber line the `StatusLine` gets the width left of it), and their click, draw and the warnings button's place in the Tab cycle all read that one result. A key other than Enter, Tab or Shift-Tab pressed on the focused button first focuses its host window (the slot's window, or for the scrubber line the window occupying the corner), so navigation acts on that window's list. The `:command`/filter line (`input_line`; a search query is typed in its window's title row instead) draws in the left slot through `input_rect` when that slot, up to the button, is at least `INPUT_FLOOR` wide, else it takes the whole last screen row (the scrubber line, or with it off the bottom windows' status rows).
  The scrubber line is a Settings checkbox (`Config::status_line`, saved in `state.toml` as `status_line` when it differs from the config; applied live): off, `bar_rows()` is 0 and `panes::split`, `float_rect` and the slot maths reserve no row, so the windows reach the last screen row, the fixed-row bottom click is not tested and the status group lands in the nearest window status row. Its scrubber bar is ASCII (`=` played, `-` rest).
- Key hints are a Settings checkbox (`Config::show_hints`, saved as `show_hints` like `status_line`; applied live via `StatusCtx::hints`): off, a window's idle status row draws no hints, while bind results, flashes, the `:command`/filter line, the warnings button and the right-hand count (`1/136 tracks`) still show. The row is not reclaimed.
- `cache.dir` is a Settings row: Enter opens a text field (`Editing::CacheDir`, prefilled with the `~`-shortened path) and Enter commits through `Session::set_media_cache_dir` (expands `~`, creates the directory, checks it is writable; a failure is a popup). It saves as `media_cache_dir` in `state.toml` only when it differs from the config value and applies at the next launch, when the old cache (directory and its sibling `.redb` index) is moved there before the cache opens (`medley_core::move_cache`, recorded as `media_cache_move_from` in `state.toml` while pending). If the new directory already holds a cache or the move fails, the old cache stays in use and a warning is listed.
- Auto update is a Settings checkbox (`Config::auto_update`, saved as `auto_update` like `status_line`): on, startup downloads a newer release in the background (`Session::check_for_update`, the same install as `:update`), again on the plugin-health cycle about 3 times a day, and flashes that it is ready.
- `placed()` derives every shown window's `Placed` (its rect and the `frame` box it is hit-tested by),
  bottom first: the active tab, the `Docked` ones around it (`panes::split`), then each `Floating`
  one's `float_body` inside its `float_rect` frame — three fifths of the area between the fixed rows,
  recomputed from the screen size, cascaded from the centre by the window's rank by id among the open
  floats, so raising one moves none. Layout, draw and the mouse hit-test all read that one `placed()`.
  An open `Screen` window (`fullscreen()`, the newest) takes the place of the active tab as
  `main_id()`: it covers the tab, the docked windows, the tab bar and the scrubber line, and the floating windows show over it. Whatever falls back to "the main window"
  (focus, the active list, Enter/Esc fall-through) therefore never reaches the
  hidden tab. A fullscreen list lets every shell key through, acting on its own selection (`/` filters it, `:`, `q`, playlist keys,
  `+`, `?`); a digit, like anything that `activate`s a tab, closes the fullscreen window and shows
  that tab. A floating or fullscreen window that is no list owns the keyboard: a key it ignores is dropped but for those `Action::is_window_action` names
  (`:`, `M`, backtick, `?`, a tab digit) and Tab/Shift-Tab, which cycle focus. Esc closes it once it has no use for it, as it does any window that is not tabbed (`Placement::closes_on_esc`).
- `MedleyView::saved_layout` is the `core::Layout` that `app` writes to `state.toml`'s `[layout]` at
  shutdown — tab order, active tab, open windows in order, every non-tab window's placement, dock side
  and stack — and `MedleyView::new` restores it, or none of it unless it places exactly the startup
  windows, every startup tab tabbed and no companion, with at most one open `screen` window. The default
  layout starts on its first tab.
- A floating window sits inside the box the shell draws around it (a border and a blank column each side, the status row directly on the bottom border)
  (`draw_float_frame`), so a window never knows it floats. Opening a `Floating` or `Screen` window
  focuses it; focusing a floating window raises it (`focus_window`). Closing the focused window
  (`close_window`) hands focus to what it opened over if that is still shown, else the next shown
  window below it in `open`, else the active tab.
- `Window` API, all of it called by the shell only:
  - `relayout(rect, s)` from `MedleyView::layout`: stores the rect; a list clamps its cursor into the
    list and re-follows it when the height changed.
  - `frame(ctx) -> WindowFrame`: the window's session-derived draw data, taken under the frame's one
    lock and memoized inside the component (`ListFrame` rows, Settings entries; Log and Vis are `Live`).
  - `draw(printer, focused, frame, over)`: windows the shell's printer to its own rect. The active tab's
    window is drawn unfocused: only docked and floating windows show the `[title]` focus marker.
  - `on_event(event, ctx) -> WindowOutcome` (`Ignored`, `Consumed`, `Run(Command)`,
    `ToggleSetting(row)`, `Bind`/`Unbind`): a mouse event outside its rect and any key it has no use for is `Ignored`,
    so the shell can offer it to the next window. Components never return `EventResult` or dispatch.
  - `blur()` when the window closes or another takes focus, and `set_status(text, refused)`, a message
    for the window's own status row; only Help has state to drop.
  - `Ctx` is what the shell hands a window under a lock: `&Session`, the live pane layout config
    and every window's placement.
- `TrackList` owns its kind, `ListState`, where a Playlists window is (`Open`: top level, a local
  playlist, a remote one), its `/`-filter query and its memos (`matches`, `frame`, `top_all`, `top`, `collections`). `TrackList::top` is the one
  function that orders a Playlists window's top-level rows; the cursor, a click, Enter, the row
  builder and a bound key all index it. `select` names the playlist the next `relayout` puts the
  cursor on, which is how the cursor lands on
  a playlist `:newplaylist` made in every Playlists window at its top level (`Dispatch::PlaylistCreated`),
  on the one `show_top` was given, and returns to the playlist Esc backed out of. A direct assign
  leaves the cursor at its row index, so in the Playlists tab it stays on the bound playlist; in a
  keyed-first window a playlist's first key moves its row up into the keyed group, and `assigned`
  (that playlist and the one listed after it, taken at the keypress) has `relayout` select the
  follower once the key has landed — straight away or after the confirm — so successive keys bind
  successive playlists. `reset_for_new_list` is the only way
  `Open` changes and `set_query` the only way the filter does; both reset the selection and bump
  `view_gen`, the list-identity part of every key below. Nav keys go through `ListState::on_event`
  (`Nav::of`); the wheel scrolls the window without the cursor (`ListState::scroll`); Enter or a
  double-click plays the row as `Command::PlayContext`, or opens the top-level playlist under the
  cursor; Esc clears the filter, then backs out of an open playlist. On a top-level playlist row of
  any Playlists window a character `keybindings::taken` has no objection to — not
  `keybindings::fixed` (the one table `map` reads its structural keys from), not a `Nav` key and not a
  built-in's effective key — is `Bind(target, key)`, and Bksp on a row with a key is
  `Unbind(target)`; every other key stays `Ignored`, so it still does what it does everywhere.
  `MedleyView::bind_hotkey` is the one bind path, for a list's direct assign and a Help row's capture
  alike: it asks the same `taken`, binds at once when nothing is overwritten, and otherwise — the key
  is on another playlist, or the target playlist has a different key — opens `input::confirm`, the
  Yes/No cursive dialog a removal (`run_confirmed`: a playlist key or `l` on a member track) also uses (Enter/`y` confirms, Esc/`n` cancels). The dialog is a
  layer over the shell, so no key reaches a window while it is open, and its closure owns the
  `(target, key)` it named; `commit_bind` runs through `on_root` on a yes. A built-in rebound in Help
  asks only when it takes a playlist's key; a key another built-in answers to stays a refusal. Inside an open playlist a playlist key
  toggles the selected track's membership as in any list; a removal asks first (`Session::removal_prompt`), and from the list showing that playlist unfiltered it removes only the cursor row (`TrackList::open_row`, a position; `PendingRows` dims just that row), elsewhere every occurrence. A filter stays with its window
  across tab switches and closing; only Esc or `reset_for_new_list` clears it.

## Search windows

Every Search window (the `search` tab, the `results` companion) is independent: its query, results, kind filter, opened collection, cursor and `/`-filter are its own. The window holds `TrackList::search`, the id `Session::search(text, replaces)` returned for its last query, and reads that id's `ResultSet` (`core::view_cache`: query, resolved tracks, collections, a generation) through `Session::results(id)`; nothing keeps a "current" search. A new search from the window replaces (forgets) its previous id, and `close_window`/a placement change that hides it (`drop_search`) forgets it too; `Session::forget_search` cancels the source threads (`Search::forget` removes the id from the live set every sink consults) and drops the set, and a hit for a forgotten id is dropped in `on_event`. Blank text only forgets. `Editing::Search(WindowId)` is that window's query input: the shell writes the buffer through to `TrackList::set_input` on every keystroke, and the window draws it in its own title row as `/<text>█` (`typed_title`: the end stays in view via `tail_fit`, and `  (Esc to cancel)` only while everything fits; the corner `input_line` never shows it). Only `/` (`FocusSearch`), a tab digit (`Action::Tab`), and `Action::ToggleWindow`/`Action::ShowWindow` (`:window search`) on a Search window open the input (`edit_search`); `:search <query>`, placements and `:panes` layouts show the window without editing; commit runs `start_search` on that window, which lists the new results from its top (`set_search`) with the kind filter kept. A `/` typed as the first character is dropped, a digit there switches tabs as before. `search_window` picks where a search goes when no Search list is focused (`/` elsewhere, `:search <query>`): a shown Search window, else the tab.

## Kind bar

`kind_bar.rs` draws and hit-tests the title row's kind bar. A static `[f] change filter` label (the `CycleKindFilter` key) sits left of the bar when it fits beside the title without squeezing it. A list has one (`TrackList::has_kind_bar`) at the top level of a Search window (All, Songs, Albums, Playlists) or a Playlists window (All, Albums, Playlists); `is_results` is narrower: a Search top level, whose list mixes tracks and collections. `f` (`cycle_kinds`) or a click on a segment sets `kinds`, bumping `view_gen` with no refetch; if `select` names a row the filter hides, `relayout` resets the filter to All. Album rows never take a hotkey.

## Queue info rows

A Queue window's rows are its tracks, then `Session::queue_info_rows` (`loading <name> …` per pending enqueue, `Continues: …`), drawn with `plain_row` and counted in `len`, so the cursor can rest on them; `visible_track_ids` never holds them, so `selected_track` is `None` there and every action on a track ignores the row. They change without `queue_gen` moving, so the frame is keyed on `revision`, which every job change bumps. `q` on a collection row (`TrackList::selected_collection`: a Search or Playlists row) runs `Command::EnqueueCollection`.

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
| `TrackList::top_all` (ordered top-level rows, unfiltered) | playlists, remote-playlists and hotkeys generations | local and remote playlists and saved albums, which have a key, `keyed_first` |
| `TrackList::top` (top-level rows the kind filter admits) | the `top_all` key, `view_gen` | `top_all`, `kinds` |
| `TrackList::collections` (Search's collection rows) | `list_gen` (the result set's generation), `view_gen` | the window's search collections, `kinds` |
| `TrackList::frame` (`ListFrame`) | `revision`, `view_gen`, offset, body height, the search text being typed | visible rows (attrs, now-playing, hotkey letters, liked marks, pending marks), title, total |
| `SettingsPane::entries` | `revision`, pane layout config, `Placements::generation` | config, volume, scan mode, vis fps, every window's placement |
| `MedleyView::chrome` (`Chrome`) | `revision`, `warnings_revision` | status core, warning count, the help, playlist-keys, placement, like, transport, queue, reveal and layout keys, and the keys assigned to playlists |
| `MedleyView::follow_sig` | window id, `list_gen`, `view_gen`, cursor | — (gates `ScanDriver::follow_view`) |
| `HelpPane::built` (`Built`: lines, rows, sections) | body width, hotkeys, playlists and remote-playlists generations | the item table, effective keys, keyed playlists' names (plugin commands are fixed at startup) |
| `HelpPane::fitted` | the `built` key, body height | — (gates re-following the cursor after a re-wrap or resize) |
| `PlaylistPicker::built` | playlists generation | the playlists |

`TrackList::list_gen` is the generation of the list on screen. Generations are held by the type that
owns the data and bumped next to the write in one private method, so no mutation site has to judge
what kind of change it made: `Queue` (`queue_gen`, `history_gen`), `ViewCache` (one per `ResultSet`, one per
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
  frame. The plain fields the UI shows (`now_playing`, the player state, `volume`, `context`) live in a `Revised<Shown>` (`core/src/revised.rs`): read through `Deref`, written
  only through `write()`, which bumps, so such a write cannot skip it; the plugin health and the background `failures` the warnings button counts live in their own
  `Revised<Warned>` (`Session::warnings_revision`), so a warning never moves `revision` and rebuilds no list rows; the per-tick position and
  duration sit outside it in `Session::progress`. State held elsewhere (hotkeys, config, wiring, scan
  mode) calls `touch()`. Anything that changes UI-visible session state outside `dispatch`/`on_event` must bump it, and
  an off-thread writer must send a `CoreEvent`, since nothing else will notice its mutation. An input
  event is not itself such a change and takes no mutable lock of its own, so a keypress that changes
  nothing rebuilds nothing.
- A library scan's flood of `TrackUpdated`s moves `revision` only, so it redraws the visible rows but
  never re-filters, re-follows or rebuilds Help; nor does a remote page landing re-filter another list.
- A row's cached-track marker (`Row::source`, `Session::is_track_cached`) only ever changes off a
  `TrackUpdated`/`Materialized` event, so every `MediaCache` write site must send one once the write
  lands.
- `Session::remote_playlists` is a pure read of whatever's landed; the fetch itself is kicked by
  `Session::ensure_remote_playlists`, never from the getter or a redraw: at startup (`app/src/main.rs`), on
  `CoreEvent::PluginStatusChanged`, and when a Playlists window is shown. It skips a source whose plugin
  health isn't `Ok`. A clean fetch with playlists is final for the session; a failed or empty one is
  retried, at most once per `PLAYLISTS_RETRY_FLOOR`, and a re-wire clears that floor.
- Writes: `run(cmd)` → `Session::dispatch` → a `Dispatch` saying what happened (`Queued(n)`,
  `ShuffleSet(on)`, `MembershipSet`, `Done(result)`, `Refused(why)`, …) or an `Err`; `run` never
  re-reads the session to find out.
  `with_session_mut` is for settings calls (`bind_hotkey`, `set_source_enabled`). Slow work runs on a
  spawned thread and reports as a `CoreEvent` carrying its result (`MembershipResult`,
  `PluginReport`); core keeps no message for the UI to poll.
- Failures have three channels by cause. Something the user asked for failed (`dispatch`'s `Err`,
  `MembershipOutcome::Failed`, a `PluginReport`, a command that doesn't parse, `Notice::failed`): a
  `Popup`. Blocked or not applicable right now (`Dispatch::Refused`, `MembershipOutcome::Blocked`): a
  `Flash`. Background work failed (`CoreEvent::BackgroundFailure`, `Session::warn`: playback, stream,
  search, playlist pages, scan, token refresh, config): a row in the warnings list until restart,
  deduplicated and bounded, counted by `Session::warning_count` — never a popup or a flash.
- Messages: `Notice` (`notice.rs`) is the one place that decides where a `Dispatch` or a `CoreEvent`
  is shown — `Flash` on the focused window's status row (what a key or command did, `Dispatch::Done` included) or a
  `Popup` dialog (a failure, a plugin's report) — and `MedleyView::notify` the one way to show it. The flash has one slot, `MedleyView::feedback`, gone after `FLASH_LIFETIME` (3 s, the baseline redraw hides it) or cleared by the next input event (not a
  mouse hold/release) that arrives where the slot shows: the base view, a `Screen` window included.
  A modal's keys, its closing one included, leave it, so a result that lands behind the picker is
  readable after Esc. A window or modal never writes it: it returns an outcome and the shell notifies.
- Inbound: `app/src/main.rs` loops `siv.step()` → `bus.drain()` → `Session::on_event` →
  `ui::deliver(events)` → `siv.refresh()` when dirty; a bus send wakes `step()` through cursive's
  `cb_sink`. `deliver` is the only push into the view (the root is a `NamedView`, reached by
  `on_root`, which dialog buttons also use to `run` a command); everything else is re-read from the
  session by the next `draw`. `set_fps(BASELINE_FPS)` is the idle redraw floor
  (clock, marquee, title flush); `sync_vis_fps`, run after every event, raises it to the Vis fps limit (`Vis::fps`, mirroring `cfg.vis.fps`) while
  a Vis window is shown and must never go below the floor. `Event::Refresh` only runs that sync.

## Routing in `MedleyView::route`

1. Clear the `feedback` slot (not on mouse hold/release, nor under a modal).
2. `on_edit_event`: an active text field (`Editing`) captures everything. A `/`-filter being typed is
   written through to the active list's `set_query`, a search query to its window's `set_input`, on every keystroke.
3. The open `Modal` (`modal.rs`), if any, takes every event: `MedleyView::modal` is one
   `Option<Modal>` (`Warnings`, `Picker`), so there is
   no precedence to order — a modal swallows all input, hence nothing can open a second one.
   `draw_modal`/`on_modal_event`/`relayout_modal` are the only matches over it; a modal's `on_event`
   returns a `ModalOutcome` (`Stay`, `Close`, `Run(Command)`, `Setup(row)`).
4. A left press on the warnings button (its rect from `status_widget`) opens the warnings modal. Fixed-row mouse (not under a `Screen` window, which covers those rows): row 0 → `TabBar::click`; bottom (when the scrubber line is shown) → `StatusLine::click` over the width the button leaves.
5. Any other mouse event goes to the topmost shown window whose `Placed::frame` contains it. The
   window takes focus when it uses the event, and on any press — so a press on a floating window's
   border or title row raises it; a click outside a floating window reaches what is under it and
   closes nothing.
6. Keys: Enter on the focused warnings button opens the modal, Tab and Shift-Tab cycle on from it, any other key focuses the button's host window first. Then
   `send` offers the key to the focused window — but for Tab and Shift-Tab, which only a floating or
   `Screen` window is offered. Enter and Esc a tabbed Log, Settings or Vis window ignores go on to
   the main window (`main_id()`) when that is a list (play from, or back out of, the main list while a Log has focus);
   a focused list or Help window keeps its own Enter and Esc even with nothing to act on; no other key
   ever acts on a window out of focus, so nothing toggles a tabbed Settings row or edits a list the
   user isn't in. Esc with a window that is not tabbed focused stays with that window and closes
   it when ignored (focus returns to where it came from, `close_window`), and a key a focused floating or fullscreen window that is no list ignores goes no further unless
   it is a window action (a tab digit included) or Tab/Shift-Tab; Enter never goes on to the main list past a window shown over the view. What is left goes to `on_shell_key` (`Tab` and Shift-Tab cycle `focus_order()`, seek, and
   `keybindings::map` / `hotkey_toggle` → `handle_action` with the active list's selection).
7. After `route` returns, `on_event` runs `layout()` and, for anything but a mouse event (a wheel
   scroll must stay put), `clamp_scroll()` re-follows the cursor in the active tab's and the focused
   window.

## Modals

A modal is constructed with its data at the open site (`PlaylistPicker::new(id, s)`): cursive drains every buffered input event through `on_event` before any layout pass, so
type-ahead must never meet an empty modal. One that snapshots session data also carries a `built`
stamp and re-reads in place from `relayout_modal` — never `draw` — only when the stamp moves while it
is open: the picker keeps its cursor on the same playlist. `draw_modal_frame(
printer, rect, title, footer)` draws the title bar and footer hint and returns the body printer;
`modal_body`/`modal_list` are the layout both draw and hit-test use, from the modal's `Rect`; the
picker is a floating box (`draw_float_frame`, `float_body`) sized to its playlists and centred over the still-drawn main view, and a left press outside it closes it; the warnings modal covers the whole screen (`Modal::covers_screen`) and splits its list rows once more (`WarningsModal::areas`) into the list and the message
area that wraps the selected row's full text.

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
   before writing scroll or width math. `../screen.rs` names the kinds, the placements and the
   startup windows and owns every mapping over them.
