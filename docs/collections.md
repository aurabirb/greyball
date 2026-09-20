# Collections: albums, playlists in search, queueable playlists

Design for the three TODO.md sections "Album support", "Search experience" and "Queue". Goal: as few
new types as possible, no change to unrelated UI code.

## Why

Everything medley lists today is a track, but people think in releases: they look for an album, not
its songs; they want to queue "this album next", not twelve `q` presses; and they want albums kept
apart from playlists when browsing. Search returns only songs, the Playlists window mixes every
collection together, and the queue can't hold anything but single tracks, so it can't show what
plays once it runs dry.

What this enables:
- Find an album, EP, single or playlist directly from Search, and see which is which.
- Browse albums without playlists in the way, and playlists without albums.
- Queue a whole album or playlist in one action, and see in the Queue window what plays after it.
- Do all of that for any source, since a collection is just a `BrowseNode` the source understands.

Scenarios to build and check against:
1. Search "some artist": albums and playlists are listed after the songs as `[album] …` / `[playlist] …`
   rows, and a kind bar (All, Songs, Albums, Playlists, with counts) in the title row, clickable or
   cycled by `f`, filters them; Enter opens an album, whose title line reads "Album · year · N tracks", and it plays like
   a playlist.
2. A 2-track single, a 5-track EP and a 12-track album each get the right label once opened.
3. Playlists window filtered to Albums shows only saved albums; docked next to Now Playing it keeps
   working while music plays.
4. Playing a track from a playlist, then queueing an album: the album plays in full after the current
   track, then the playlist resumes.
5. Queueing a remote album that is still paging in (a long list): the UI never blocks, a `loading …`
   row shows in the Queue, tracks appear as pages land, and a failing or stalled load flashes a
   notice and clears the row.
6. The last Queue row reads "Continues: <playlist> — <next track>" while the playing context has
   tracks left.
7. Shuffle and repeat-track on with a queued album: the queued tracks still play in order, and single
   tracks of the album can be removed from the queue.
8. A second search issued while the first is streaming shows only the second one's results.

## Vocabulary

- **Collection**: anything that expands to tracks through `Source::browse`. Represented as
  `(String, BrowseNode)` — the same pair `BrowsePage.folders` already carries. `BrowseNode` is the
  immutable handle only the owning source understands; core never parses it.
- **Album**: a collection whose `ItemKind` is `Album`. Single, EP and album are all `Album`; the
  Single/EP/Album wording is `release_label(track_count) -> &'static str` (≤3 single, 4–7 EP, else
  album), a display function with nothing stored.
- Local playlists are unchanged. `Playlist` gets no new fields.
- `ItemKind` becomes `Track | Album | Playlist` (drop the unused `Artist`).

## Existing pieces reused as they are

- `ViewCache`'s per-`(source, node)` track cache (`RemotePlaylistTracks`), `Session::
  remote_playlist_track_ids`, and the Playlists window's `Open::Remote` path: an album found through
  search opens and plays exactly like a playlist folder.
- `SearchQuery.kinds` already exists; today it is always `[Track]`.
- `Source::browse_uri` already maps a pasted album/playlist link to a node.
- Spotify already browses `album:` nodes (`ALBUM_PREFIX`, `sources/spotify/src/source.rs`).

## Changes

### 1. `SearchHit` → `Track`

`Source::search`'s sink and `Source::resolve` return `Track` (random id, exactly one rendition);
`BrowsePage.tracks` becomes `Vec<Track>`. `Catalog::ingest(track) -> TrackId` still decides
identity: on an ISRC/fuzzy match it merges the rendition into the existing track and returns that
id, otherwise it keeps the fresh one. `SearchHit` and `SearchHit::to_rendition` are deleted; the
matcher takes `(&Track, &Track)`. The source fills `Rendition.link`/`added_at`.

### 2. Collections in search

New `Source` method, default returns nothing:

```rust
fn search_collections(&self, q: &SearchQuery, kind: ItemKind, sink: &mut dyn FnMut(String, BrowseNode)) -> Result<()>;
```

The kind is a property of the query, not of the result. Spotify calls its search endpoint with
`type=album` / `type=playlist`; the name is `"Artist – Title"`. `search::Search::run` runs the track
search plus one `search_collections` per requested kind and reports collections through a new
`CoreEvent::SearchCollection { kind, source, name, node }`.

The Search window shows a kind bar (All, Songs, Albums, Playlists) in its title row; a click or the
kind key cycles which kinds are listed (no refetch). Songs come first, then `[album] …` and
`[playlist] …` rows. Collection rows are still `TopRow::Remote`, drawn through `plain_row` with a kind
prefix, so Enter/open/queue reuse the Playlists window's paths. Tag `SearchHit`/`SearchCollection`/`SearchDone` events with the search
generation in the same change (fixes the mixed-results bug in TODO.md).

### 3. Albums view

A Playlists window at its top level shows the same kind bar as Search with segments All / Albums /
Playlists (`KindFilter::COLLECTIONS`; `f` cycles them). `TopRow::Remote` carries its `ItemKind`, set by
the listing that produced the row, and the window's filter is applied to `top_rows` in `TrackList::top`;
"Albums" is the same window with the filter set, so it docks/tabs like any other. Local playlists are
always `Playlist`; album rows read `[album] [source] Artist – Title`. Album rows are never bound to
hotkeys (albums are read-only): a key on one is ignored and the assign hint is not shown.

Saved albums come from a new default-empty `Source::saved_albums(&self, want: usize) -> Result<BrowsePage>`
(the albums are `BrowsePage.folders`; Spotify pages `/me/albums` into nodes `album:<id>`), offered
only by a source whose `has_saved_albums` is true. `ViewCache::ensure_remote_playlists` fetches it as
its own single-flight walk beside the playlist folders (`ensure_folders`, used once per list) into
`remote_albums`, persisted in the same store table under the key `<source>#albums`, read with
`Session::remote_albums(source)`; `remote_playlists_gen` covers both lists.

### 4. Queue

The queue stays `VecDeque<TrackId>`: tracks only, no new entry type. Enqueuing a collection resolves
it to tracks and appends them.
- One command, `Command::EnqueueCollection(HotkeyTarget, String)` (the target and its display name), serves a Search row or a Playlists row
  (album or playlist); `q` on a track row queues that one track. A local playlist appends its tracks at once; a second job for a
  collection already loading is refused.
- A remote collection may still be paging in. The session holds a pending-enqueue job for it, driven
  by the existing playlists-changed event (no polling, nothing blocks the UI): as pages land, the
  newly loaded tracks are appended in order, read from the confirmed walked list
  (`ViewCache::remote_playlist_confirmed_ids`: empty until a cache hydrated from a previous session
  is revalidated, and without pending adds), so the appended count always indexes the list it
  counts. The job finishes when the list settles, and is dropped with a flashed notice when it
  errors or hits a 60 s timeout; on timeout it only gives up (the walk continues in the cache). One
  timer thread per job set, armed for the earliest deadline, sends `QueueChanged` and is re-armed for
  the next; notices go through `CoreEvent::Flash`. Clearing the queue cancels the jobs. The tracks it appends are ordinary queue tracks (removable, reorderable, persisted like
  any other).
- A job appends when tracks are loaded, so tracks queued meanwhile land before its later tracks; this
  is accepted and not worked around with placeholder entries.
- One collection appends at most `ENQUEUE_CAP` (500) tracks, and the notice says when it was cut.
- The Queue window shows non-selectable info rows after the tracks (`Session::queue_info_rows`): `loading <name> …` per pending
  job and, last, the derived continuation row.
- Shuffle, `RepeatTrack`, `advance`, `upcoming_track` and queue persistence are unchanged.

MVP pseudo-row: `Continues: <context name> — <next track>`, derived from `PlaybackContext` while it
has tracks left, never stored, not selectable.

## Rendering metadata with minimal UI change

- Rows: collection names are source-formatted strings (`"Artist – Title"`), rendered by the existing
  `plain_row`; no new row kind.
- Opened collection: add `subtitle: Option<String>` to `BrowsePage` next to `title`, filled by the
  source (`"Album · 2019 · 12 tracks"`); the list window's existing title line shows it. The
  release label comes from `release_label(tracks.len())` once the list is loaded. Nothing else in
  the UI reads it.
- Queue info rows (pending job, continuation): plain rows, not selectable.

## Constraints

Each rule names the shape it forces and the decision behind it. If an implementation seems to need
to break one, report it back instead of working around it.

- **A `Track` is always a playable track.** Never add a `kind` to `Track` (or a placeholder track)
  to stand for an album or playlist. The queue's "every id is playable" invariant, scan, analysis,
  download, history, m3u export, hotkey membership, ISRC dedup and the matcher all assume it; a
  kind field would need a guard in every one. Collections are `(String, BrowseNode)`.
- **No type carrying meaningless fields.** Don't overload `SearchHit`/`Track` with `duration_ms`,
  `isrc`, `quality` or `album` set to placeholders for a collection. Each thing has one meaning, so
  each consumer needs no `kind` check.
- **Few new types.** No new type is planned. Before adding one, look for an existing
  shape (`BrowseNode`, `(String, BrowseNode)`, `TopRow::Remote`, `BrowsePage`) that carries it. A
  `CollectionHit`, `SearchItem`, `Folder` or `CollectionRef` was considered and rejected as
  duplicating the tuple that `folders` and `TopRow::Remote` already are.
- **`BrowseNode` stays opaque to core.** Core never parses it or infers a kind from it; the source
  owns its meaning. That is what keeps albums source-agnostic. The kind of a collection comes from
  the query (`SearchQuery.kinds`) or from the window's filter, never from the node.
- **Kind is a property of the query/window, not of the item.** This is why `search_collections`
  takes a `kind` and returns plain pairs, and why the Playlists window filters in `top_rows`
  instead of the items carrying a tag.
- **Single, EP and album are one kind.** `ItemKind::Album` only; the wording is the display
  function `release_label`. Don't store a release type or add a filter on it.
- **`Playlist` is local only.** Don't give it `kind` or `origin` fields to make remote collections
  fit: remote playlists are cached by `ViewCache` under `(source, node)` and listed by their source,
  and search-result collections stored as `Playlist`s would leak into the user's own playlists list.
- **Reuse the remote track cache untouched.** `remote_playlist_track_ids(source, node)`, its paging
  and its freshness logic serve search-opened albums and pending enqueues alike. Don't add a second
  cache or copy tracks out of it.
- **The queue holds tracks only.** No collection or placeholder entry ever sits in `Queue`, so
  shuffle, `RepeatTrack`, history, persistence and the player never see one. A remote collection is
  resolved to tracks by a pending job that appends as pages load; never block the UI on it and
  always give the job a timeout that flashes a notice.
- **Queue info rows are derived, never stored.** The continuation row and the `loading …` rows are
  built for display, are not selectable, and are never persisted or removable.
- **UI changes stay in the touched windows.** Search, the Playlists window and the Queue window
  change; nothing else does. Search shares `Open::Remote` with Playlists for an opened collection.
  Collection rows render through the existing `TopRow::Remote` / `plain_row` path and metadata
  through `BrowsePage.subtitle` on the existing title line — no new row kind. The one dedicated
  drawing code is the title row's kind bar (`ui/src/view/kind_bar.rs`).
- **`Catalog::ingest` decides identity.** Sources hand over `Track`s with throwaway ids and never
  merge or dedupe themselves.
- **No compatibility shims.** The saved queue and any changed persisted shape are dropped, not
  migrated (AGENTS.md).

## Order

1. `SearchHit` → `Track`; `ItemKind` cleanup. (done)
2. Search generation tags on events. (done)
3. `search_collections` + kind bar + prefix rows. (done)
4. Playlists-window kind filter + saved-albums listing. (done)
5. `EnqueueCollection` with the pending-enqueue job + Queue info rows (loading, continuation). (done)
