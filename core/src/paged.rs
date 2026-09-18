//! Generic background pager for a `Source::browse` node whose full list
//! can't be fetched in one blocking call without stalling the (synchronous)
//! UI render path — e.g. a Liked Songs / saved-tracks list, or a root
//! playlist-folder listing, that's paginated server-side. Not source- or
//! item-type-specific: any `Source` with a similar paginated endpoint
//! (Spotify, SoundCloud, ...) can hold a [`PagedList<T>`] per browsable node
//! and delegate to it from `browse`.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::event::{Bus, CoreEvent};

/// Gap between page fetches — avoids tripping a provider's rate limit/daily
/// quota. `new_with_delay` overrides (tests use `Duration::ZERO`).
const DEFAULT_PAGE_DELAY: Duration = Duration::from_secs(5);

/// One page of a paginated remote list.
pub struct RemotePage<T> {
    pub hits: Vec<T>,
    /// Total items the API reports, independent of `hits.len()` (a page can
    /// be short — filtered-out malformed items, or the last page).
    pub total: usize,
    /// How many raw items this page advanced the *source's* cursor by —
    /// i.e. what to add to the offset for the next request. Independent of
    /// `hits.len()`: a page that filtered out a null/malformed/no-id entry
    /// still consumed that slot server-side. Must equal `hits.len()` only
    /// when nothing on this page was filtered — get this wrong and every
    /// later page is requested at the wrong offset, silently skipping or
    /// re-fetching items from wherever the first page dropped something,
    /// typically not surfacing until the very end of the list.
    pub consumed: usize,
}

struct State<T> {
    items: Vec<T>,
    /// Raw items consumed so far — the offset for the next `fetch_page`
    /// call. Deliberately not `items.len()`: see `RemotePage::consumed`.
    consumed: usize,
    /// Set once page 0 comes back. `None` also means "no fetch started yet".
    total: Option<usize>,
    /// A background walk is already running (or the synchronous first page
    /// is in flight) — guards against a second `snapshot` call while still
    /// loading from starting a duplicate walk.
    fetching: bool,
    /// High-water mark of `snapshot`'s `want` — while `items.len()` is below
    /// this, the walk skips `page_delay` between pages.
    demand: usize,
    /// Set once a page fetch fails — `items` then stops short of the real
    /// list even though `total`/`consumed` make `snapshot` report "done".
    /// See `PagedList::errored`.
    errored: bool,
}

impl<T> Default for State<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            consumed: 0,
            total: None,
            fetching: false,
            demand: 0,
            errored: false,
        }
    }
}

/// A `Source::browse` node's list, loaded page-by-page in the background.
/// Cheap to `Clone` (an `Arc` around the shared state) and safe to call
/// `snapshot` from `browse` on every redraw: only the very first call does
/// any blocking work.
#[derive(Clone)]
pub struct PagedList<T> {
    label: Arc<str>,
    state: Arc<Mutex<State<T>>>,
    page_delay: Duration,
}

impl<T: Clone + Send + Sync + 'static> PagedList<T> {
    /// `label` identifies this list in the warning logged when a page fetch
    /// fails, e.g. `"spotify: liked songs"`. Paced at [`DEFAULT_PAGE_DELAY`]
    /// between pages — see [`Self::new_with_delay`] to override (tests want
    /// `Duration::ZERO`; a real source generally shouldn't).
    pub fn new(label: impl Into<Arc<str>>) -> Self {
        Self::new_with_delay(label, DEFAULT_PAGE_DELAY)
    }

    pub fn new_with_delay(label: impl Into<Arc<str>>, page_delay: Duration) -> Self {
        Self {
            label: label.into(),
            state: Arc::new(Mutex::new(State::default())),
            page_delay,
        }
    }

    /// The list loaded so far, plus whether more may still land — pass the
    /// second value straight through as `BrowsePage::partial`. Never touches
    /// the network itself (nor blocks on anything but its own uncontended
    /// mutex) — safe to call from a UI render path.
    ///
    /// On the very first call this spawns a background thread that calls
    /// `fetch_page` with successive offsets starting at 0, extending the
    /// cache and sending `CoreEvent::PlaylistsChanged` on `bus` after every
    /// page, until the reported total is reached or a page comes back empty
    /// or erroring — so that very first call returns an empty, `partial`
    /// snapshot; the real page 0 shows up once that `PlaylistsChanged`
    /// prompts a redraw and this is called again. Any call while a walk is
    /// already running (or once it's finished) is just a cheap snapshot —
    /// `fetch_page` is not re-invoked.
    /// `want`: how many items the caller needs ready now — the walk skips
    /// its pacing delay below this, so active scrolling loads fast while
    /// idle background completion stays slow. High-water mark, never
    /// shrinks.
    pub fn snapshot<F>(&self, bus: &Bus, want: usize, fetch_page: F) -> (Vec<T>, bool)
    where
        F: Fn(usize) -> Result<RemotePage<T>, String> + Send + Sync + 'static,
    {
        let should_start = {
            let mut s = self.state.lock().unwrap();
            s.demand = s.demand.max(want);
            if s.total.is_some() || s.fetching {
                false
            } else {
                s.fetching = true;
                true
            }
        };
        if should_start {
            let list = self.clone();
            let bus = bus.clone();
            thread::spawn(move || {
                loop {
                    let (consumed, total) = {
                        let s = list.state.lock().unwrap();
                        (s.consumed, s.total)
                    };
                    if total.is_some_and(|t| consumed >= t) {
                        break;
                    }
                    let got = list.fetch_one(&fetch_page);
                    bus.send(CoreEvent::PlaylistsChanged);
                    if got == 0 {
                        break;
                    }
                    // After the break checks, so a finished/failed walk
                    // doesn't pay a pointless final delay. Still behind
                    // demand: keep pulling fast instead of pacing.
                    let (items_len, demand) = {
                        let s = list.state.lock().unwrap();
                        (s.items.len(), s.demand)
                    };
                    if items_len >= demand && !list.page_delay.is_zero() {
                        thread::sleep(list.page_delay);
                    }
                }
                list.state.lock().unwrap().fetching = false;
            });
        }
        let s = self.state.lock().unwrap();
        let done = s.total.is_some_and(|t| s.consumed >= t) && !s.fetching;
        (s.items.clone(), !done)
    }

    /// Fetch the next unfetched page and fold it into `state`. Returns how
    /// many raw items this page consumed — `0` ends the walk, whether
    /// that's because the source has nothing left or the request failed
    /// (logged either way). Deliberately not `hits.len()`: a page that
    /// filtered out a malformed entry still consumed that slot, and the
    /// *next* request's offset (`state.consumed`) must account for it or
    /// every later page drifts — see `RemotePage::consumed`.
    ///
    /// A failure also sets `total` (to whatever's been consumed so far,
    /// i.e. "no more") rather than leaving it `None`: `None` is exactly the
    /// signal `snapshot` uses to decide a fresh walk needs starting, so
    /// leaving it unset on error would make every subsequent `snapshot`
    /// call — on whatever thread calls `browse`, typically the UI's own —
    /// synchronously retry the failing request (with `fetch_page`'s own
    /// retry/backoff, if any) forever, once per redraw.
    fn fetch_one<F>(&self, fetch_page: &F) -> usize
    where
        F: Fn(usize) -> Result<RemotePage<T>, String>,
    {
        let offset = self.state.lock().unwrap().consumed;
        match fetch_page(offset) {
            Ok(page) => {
                let consumed = page.consumed;
                let mut s = self.state.lock().unwrap();
                s.total = Some(page.total);
                s.consumed += consumed;
                s.items.extend(page.hits);
                consumed
            }
            Err(e) => {
                log::warn!("{}: page (offset {offset}) failed: {e}", self.label);
                let mut s = self.state.lock().unwrap();
                s.total = Some(s.consumed);
                s.errored = true;
                0
            }
        }
    }

    /// Whether the walk stopped short because a page fetch failed, rather
    /// than because it reached the real end of the list — see
    /// `BrowsePage::errored`, which callers should copy this into.
    pub fn errored(&self) -> bool {
        self.state.lock().unwrap().errored
    }

    /// Undo the freeze `fetch_one` leaves behind after a failed page, so
    /// the next `snapshot` call resumes the walk from `consumed` instead of
    /// treating the list as done. No-op if the walk isn't `errored` (e.g.
    /// it genuinely finished, or hasn't started).
    pub fn retry(&self) {
        let mut s = self.state.lock().unwrap();
        if s.errored {
            s.errored = false;
            s.total = None;
        }
    }
}

