//! Playback order.
//!
//! Holds `Vec<TrackId>` (every id is playable, so no `is_playable()` checks
//! or `LoadDebouncer` are needed); no `Library`/`Spotify`/mpris/notifications,
//! no `Player` and no callback (`Queue::new(bus)`). `play`/`toggleplayback`
//! emit `CoreEvent::PlayRequested` or `CoreEvent::QueueChanged` instead of
//! calling a player. Repeat/shuffle state lives on the `Queue` itself.
//!
//! Redesigned as a strict-priority drain FIFO (a `VecDeque<TrackId>`) rather
//! than a `Vec` plus a separate `current_track` index: the old shape let
//! context-driven playback (`Session::play_now`'s `replace_current`) and the
//! manual queue's own advancing share one index into one Vec, so an item
//! `q`-enqueued while context playback was live could land *behind* the
//! index and never get picked up by `next_index()` — silently skipped. Now
//! `current` (what's playing) and `queue` (what's upcoming, strictly FIFO)
//! are fully independent: a track is popped off the front and handed to
//! `Session` the moment it's due to play, so it can never desync from
//! what's actually on-screen. See `Session::advance` and `Session::play_now`
//! (`core/src/app.rs`) for how this priority is enforced against
//! `PlaybackContext`.

use std::collections::VecDeque;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use log::info;
use serde::{Deserialize, Serialize};

use crate::event::{Bus, CoreEvent};
use crate::types::TrackId;

/// Cap on [`Queue::history`] — old enough plays just fall off the front. Also used by
/// `Session::rewrite_history_file` (`core/src/app.rs`) to cap the on-disk M3U history log itself,
/// so it doesn't grow unbounded.
pub(crate) const MAX_HISTORY: usize = 5000;

/// Repeat behavior for the [Queue]. `RepeatPlaylist` no longer has a
/// queue-specific meaning: it used to mean "wrap the queue's own Vec back to
/// the start," but a draining FIFO has no fixed "start" to wrap to, so it's
/// now a no-op as far as the queue itself is concerned — kept only as a
/// setting callers can still read/write/persist (e.g. for a future repeat
/// toggle) without retyping the API. `RepeatTrack` (replay the
/// currently-playing track) is unaffected by any of this — it doesn't touch
/// queue contents at all, see `Session::advance`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RepeatSetting {
    #[serde(rename = "off")]
    None,
    #[serde(rename = "playlist")]
    RepeatPlaylist,
    #[serde(rename = "track")]
    RepeatTrack,
}

pub struct Queue {
    /// Upcoming tracks, strict FIFO: the front is what plays next. Drains as
    /// tracks play — nothing lingers behind a moving pointer, so the Queue
    /// screen's list always reads as "what's coming up."
    queue: RwLock<VecDeque<TrackId>>,
    /// The track currently playing, if any. Independent of `queue` — it may
    /// have come from the front of it, or from `PlaybackContext` (see
    /// `Session::play_now`), and either way it's not itself a member of
    /// `queue` any more once it's playing.
    current: RwLock<Option<TrackId>>,
    repeat: RwLock<RepeatSetting>,
    shuffle: RwLock<bool>,
    /// Chronological log of every track played, most recent at the back —
    /// a fallback for `previous` once the queue itself has nothing earlier
    /// (e.g. already at the start). Capped at `MAX_HISTORY`. Paired with the
    /// timestamp playback started, for the on-disk M3U history log
    /// (`Session::append_history_entry`).
    history: RwLock<VecDeque<(TrackId, DateTime<Utc>)>>,
    /// Bumped by every change to `queue`'s contents, for caches of it.
    queue_gen: AtomicU64,
    /// Bumped by every change to `history`'s contents.
    history_gen: AtomicU64,
    bus: Bus,
}

impl Queue {
    pub fn new(bus: Bus) -> Self {
        Self {
            queue: RwLock::new(VecDeque::new()),
            current: RwLock::new(None),
            repeat: RwLock::new(RepeatSetting::None),
            shuffle: RwLock::new(false),
            history: RwLock::new(VecDeque::new()),
            queue_gen: AtomicU64::new(0),
            history_gen: AtomicU64::new(0),
            bus,
        }
    }

    pub fn queue_gen(&self) -> u64 {
        self.queue_gen.load(Ordering::Relaxed)
    }

    pub fn history_gen(&self) -> u64 {
        self.history_gen.load(Ordering::Relaxed)
    }

    /// The one announcement of a change to `queue`'s contents.
    fn queue_changed(&self) {
        self.queue_gen.fetch_add(1, Ordering::Relaxed);
        self.bus.send(CoreEvent::QueueChanged);
    }

    /// History only changes under `Session::dispatch`/`on_event`, which already redraw.
    fn history_changed(&self) {
        self.history_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that `id` just started playing. Consecutive duplicates (e.g.
    /// `RepeatTrack`) are collapsed so repeatedly playing the same track
    /// doesn't burn through the cap or block `previous_from_history`.
    /// Returns the timestamp it was recorded under, or `None` if collapsed
    /// as a duplicate — the caller (`Session::play_track`) only appends a
    /// new line to the on-disk history log for an actual new entry.
    pub fn record_played(&self, id: TrackId) -> Option<DateTime<Utc>> {
        let mut h = self.history.write().unwrap();
        if h.back().map(|(last, _)| *last) == Some(id) {
            return None;
        }
        let played_at = Utc::now();
        h.push_back((id, played_at));
        if h.len() > MAX_HISTORY {
            h.pop_front();
        }
        self.history_changed();
        Some(played_at)
    }

    /// Pop the currently-playing entry off the history and return the track
    /// before it, for `previous` once the queue has nowhere earlier to go.
    /// Consumes history as it walks back — there's no "forward" into it.
    pub fn previous_from_history(&self) -> Option<TrackId> {
        let mut h = self.history.write().unwrap();
        if h.pop_back().is_some() {
            self.history_changed();
        }
        h.back().map(|(id, _)| *id)
    }

    /// The play history, oldest first — for `Session::history_ids`.
    pub fn history_snapshot(&self) -> Vec<TrackId> {
        self.history.read().unwrap().iter().map(|(id, _)| *id).collect()
    }

    /// Number of entries in the history, with no clone — for sizing the
    /// `:hist` screen without paying for `history_snapshot`'s full copy.
    pub fn history_len(&self) -> usize {
        self.history.read().unwrap().len()
    }

    /// A most-recent-first window of the history (`offset..offset+limit`) —
    /// for rendering just the visible slice of a possibly-huge history
    /// instead of resolving the whole thing every redraw.
    pub fn history_window(&self, offset: usize, limit: usize) -> Vec<TrackId> {
        self.history
            .read()
            .unwrap()
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Load a previously-saved history (oldest first), replacing whatever's
    /// there — used once at startup, from the tail of the on-disk M3U
    /// history log, before anything's been played this run.
    pub fn restore_history(&self, entries: Vec<(TrackId, DateTime<Utc>)>) {
        let mut h = self.history.write().unwrap();
        *h = entries.into_iter().collect();
        while h.len() > MAX_HISTORY {
            h.pop_front();
        }
        self.history_changed();
    }

    /// Fold newly-discovered remote plays into the in-memory history in place, keeping each
    /// existing entry (including any already consumed by `previous_from_history` this session —
    /// those aren't in `new_entries` since they came from disk) untouched. Unlike
    /// `restore_history`'s full replace, this can't resurrect entries `previous_from_history`
    /// already popped, so it's safe to call mid-session — see `Session::merge_remote_history`.
    /// `new_entries` needn't be sorted; each is inserted at its chronological position.
    pub fn merge_new_history(&self, new_entries: Vec<(TrackId, DateTime<Utc>)>) {
        if new_entries.is_empty() {
            return;
        }
        let mut h = self.history.write().unwrap();
        for entry in new_entries {
            let pos = h.iter().rposition(|(_, at)| *at <= entry.1).map(|i| i + 1).unwrap_or(0);
            h.insert(pos, entry);
        }
        while h.len() > MAX_HISTORY {
            h.pop_front();
        }
        self.history_changed();
    }

    /// The track currently playing, if any.
    pub fn get_current(&self) -> Option<TrackId> {
        *self.current.read().unwrap()
    }

    /// Set `current` directly, without touching `queue` or emitting
    /// `PlayRequested` — used by `Session` when it drives playback itself
    /// (`play_now`, `advance`), which loads the track into a `Player` on its
    /// own rather than reacting to the bus.
    pub fn set_current(&self, track: Option<TrackId>) {
        *self.current.write().unwrap() = track;
        self.bus.send(CoreEvent::QueueChanged);
    }

    /// A read-only snapshot of the upcoming queue, front (plays next) first.
    pub fn snapshot(&self) -> Vec<TrackId> {
        self.queue.read().unwrap().iter().copied().collect()
    }

    /// A window of the upcoming queue (`offset..offset+limit`), front first —
    /// for rendering just the visible slice on the Queue screen.
    pub fn window(&self, offset: usize, limit: usize) -> Vec<TrackId> {
        self.queue.read().unwrap().iter().skip(offset).take(limit).copied().collect()
    }

    /// The number of items in the upcoming queue. Does *not* count whatever
    /// is currently playing — once played, a track is out of this list.
    pub fn len(&self) -> usize {
        self.queue.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add `track` to the back of the queue — plays last among what's
    /// currently queued. Unaffected by shuffle: the manual queue always
    /// keeps its insertion order, on screen and in what plays next — only
    /// advancing through a `PlaybackContext` (a playlist) is randomized, see
    /// `Session::play_next_in_context`.
    pub fn append(&self, track: TrackId) {
        self.queue.write().unwrap().push_back(track);
        self.queue_changed();
    }

    pub fn append_many(&self, tracks: &[TrackId]) {
        self.queue.write().unwrap().extend(tracks.iter().copied());
        self.queue_changed();
    }

    /// Insert `track` at the very front of the queue — it plays immediately
    /// after whatever's currently playing, ahead of everything already
    /// queued. Replaces the old `insert_after_current`: with no index into
    /// this queue standing for "current" any more, "after current" now just
    /// means "front of the FIFO."
    pub fn play_next(&self, track: TrackId) {
        self.queue.write().unwrap().push_front(track);
        self.queue_changed();
    }

    /// Insert `tracks` at the front, in order (`tracks[0]` plays first,
    /// ahead of whatever was already queued). Replaces `append_next`.
    pub fn play_next_many(&self, tracks: &[TrackId]) {
        let mut q = self.queue.write().unwrap();
        for t in tracks.iter().rev() {
            q.push_front(*t);
        }
        drop(q);
        self.queue_changed();
    }

    /// Remove the first occurrence of `track` from the queue, if present.
    /// Returns whether it was found. Used by `Session::play_now` to pull a
    /// track that's already queued out of line when the user jumps straight
    /// to it (e.g. clicking it on the Queue screen, or picking it elsewhere
    /// while it's still waiting its turn).
    pub fn remove_track(&self, track: TrackId) -> bool {
        let found = {
            let mut q = self.queue.write().unwrap();
            match q.iter().position(|t| *t == track) {
                Some(pos) => {
                    q.remove(pos);
                    true
                }
                None => false,
            }
        };
        if found {
            self.queue_changed();
        }
        found
    }

    /// Remove the item at literal `index` into the upcoming queue (e.g. a
    /// future "delete this row" keybinding on the Queue screen).
    pub fn remove_at(&self, index: usize) {
        let removed = {
            let mut q = self.queue.write().unwrap();
            if index >= q.len() {
                info!("queue: remove_at({index}) out of bounds ({})", q.len());
                false
            } else {
                q.remove(index);
                true
            }
        };
        if removed {
            self.queue_changed();
        }
    }

    /// Pop the front item off the queue — the track that should play next
    /// once nothing else is in the way. Leaves `current` untouched; callers
    /// (`Session::advance`, `toggleplayback`) decide when to point at it.
    pub fn pop_front(&self) -> Option<TrackId> {
        let item = self.queue.write().unwrap().pop_front();
        if item.is_some() {
            self.queue_changed();
        }
        item
    }

    /// Clear all upcoming items and stop playback.
    pub fn clear(&self) {
        self.stop();
        self.queue.write().unwrap().clear();
        self.queue_changed();
    }

    /// Stop playback (clears `current`; leaves the upcoming queue alone).
    pub fn stop(&self) {
        *self.current.write().unwrap() = None;
        self.bus.send(CoreEvent::QueueChanged);
    }

    /// Point `current` at `track` and ask the front-end to play it
    /// (`CoreEvent::PlayRequested`). Only used by `toggleplayback`, which —
    /// living on `Queue` rather than `Session` — has no direct way to call
    /// `Session::play_track` itself and has to go through the bus.
    fn request_play(&self, track: TrackId) {
        *self.current.write().unwrap() = Some(track);
        self.bus.send(CoreEvent::PlayRequested(track));
    }

    /// Toggle playback. If something's already loaded, this is a no-op —
    /// the front-end toggles the active player directly for that case. If
    /// nothing's loaded, start playing the front of the queue, if there is one.
    pub fn toggleplayback(&self) {
        if self.get_current().is_some() {
            return; // the front-end toggles the active player directly
        }
        if let Some(id) = self.pop_front() {
            self.request_play(id);
        }
    }

    pub fn get_repeat(&self) -> RepeatSetting {
        *self.repeat.read().unwrap()
    }

    pub fn set_repeat(&self, new: RepeatSetting) {
        *self.repeat.write().unwrap() = new;
    }

    pub fn get_shuffle(&self) -> bool {
        *self.shuffle.read().unwrap()
    }

    /// Toggle shuffle. Doesn't touch the manual queue's contents or order at
    /// all — it only changes how `Session::play_next_in_context` picks the
    /// next track once a `PlaybackContext` (playlist) takes over from an
    /// empty manual queue.
    pub fn set_shuffle(&self, new: bool) {
        *self.shuffle.write().unwrap() = new;
    }
}

