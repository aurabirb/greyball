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
5. Queueing a remote album that is still paging in (a long list): playback reaches its tail without
   waiting for the whole list.
6. With an empty queue, the last Queue row reads "Continues: <playlist> — <next track>"; queueing a
   collection changes that row to the context that resumes after it.
7. Shuffle and repeat-track on with a queued album: the album still plays in order and the queue
   never shows or plays a collection as a track.
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
fn search_collections(&self, q: &SearchQuery, kind: ItemKind) -> Result<Vec<(String, BrowseNode)>>;
```

The kind is a property of the query, not of the result. Spotify calls its search endpoint with
`type=album` / `type=playlist`; the name is `"Artist – Title"`. `search::Search::run` runs the track
search plus one `search_collections` per requested kind and reports collections through a new
`CoreEvent::SearchCollection { kind, source, name, node }`.

The Search window shows a kind bar (All, Songs, Albums, Playlists) in its title row; a click or the
kind key cycles which kinds are listed (no refetch). Songs come first, then `[album] …` and
`[playlist] …` rows. Collection rows are still `TopRow::Remote`, drawn through `plain_row` with a kind
prefix, so Enter/open/hotkey-bind/queue reuse the Playlists window's paths. Tag `SearchHit`/`SearchCollection`/`SearchDone` events with the search
generation in the same change (fixes the mixed-results bug in TODO.md).

### 3. Albums view

A Playlists window gets an `ItemKind` filter (All / Albums / Playlists) applied in `top_rows`;
"Albums" is the same window with the filter set, so it docks/tabs like any other. Saved albums come
from a source listing them as folders (Spotify: `/me/albums`, node `album:<id>`). Local playlists
are always `Playlist`.

### 4. Queue

`Queue.queue: VecDeque<TrackId>` becomes `VecDeque<Entry>`:

```rust
enum Entry { Track(TrackId), Remote(SourceId, BrowseNode) }
```

- Enqueuing a local playlist expands to `Entry::Track`s immediately (fully loaded).
- A remote collection stays one `Entry::Remote` with a cursor: `Session::advance` takes its next
  track from `remote_playlist_track_ids(source, node)` (which may still be paging in) and pops the
  entry when exhausted. Expanding at enqueue time would break for a not-yet-loaded list, and pushing
  into `PlaybackContext` would let later queued tracks jump ahead of it.
- Queue persistence (a `Playlist` of `TrackId`s under a reserved id) gets a new on-disk shape; the
  old saved queue is dropped, no shim.
- Shuffle and `RepeatTrack` are unaffected: the queue only ever yields tracks.

MVP pseudo-track: `queue_window` appends a derived, never-stored final row
`Continues: <context name> — <next track>` while the playback context has tracks left; when the
queue's last entry is a collection, the row shows the context that resumes after it.

## Rendering metadata with minimal UI change

- Rows: collection names are source-formatted strings (`"Artist – Title"`), rendered by the existing
  `plain_row`; no new row kind.
- Opened collection: add `subtitle: Option<String>` to `BrowsePage` next to `title`, filled by the
  source (`"Album · 2019 · 12 tracks"`); the list window's existing title line shows it. The
  release label comes from `release_label(tracks.len())` once the list is loaded. Nothing else in
  the UI reads it.
- Queue row of an `Entry::Remote`: name from the cached `BrowsePage.title`; the pseudo-track row is
  a plain row.

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
- **Few new types.** The only new type is `Entry`. Before adding another, look for an existing
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
  and its freshness logic serve search-opened albums and queue cursors alike. Don't add a second
  cache or copy tracks out of it.
- **The queue yields tracks only.** `Entry::Remote` is drained through a cursor at the front, so
  shuffle, `RepeatTrack`, history and the player never see a collection. Don't expand a remote
  collection at enqueue time (the list may not have loaded) and don't push it into
  `PlaybackContext` (later queued tracks would play before it finishes).
- **The pseudo-track row is derived, never stored.** It is built in `queue_window` from
  `PlaybackContext`; it is not an `Entry`, is not persisted, and can't be removed or reordered.
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
4. Playlists-window kind filter + saved-albums listing.
5. `Entry` queue + enqueue action + pseudo-track row.
