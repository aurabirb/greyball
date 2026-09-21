//! `ui` — a thin cursive front-end for medley.
//!
//! This crate holds **zero application state**. Every screen renders from a
//! [`core::Session`] snapshot and every keypress becomes a [`core::Command`]
//! passed to `Session::dispatch`. UI-local state is limited to the cursor
//! position, which of the three screens is visible, and the in-progress text of
//! the search / `:` line (`/`, `1`, `2`, `3` never reach `Session`).
//!
//! Rather than separate `listview.rs` / `tabbedview.rs` / `layout.rs` modules
//! (which would drag in `ViewExt` / `BoxedViewExt` / `CommandResult` /
//! `Pagination` / `ContextMenu` — infrastructure disproportionate to a
//! 3-screen read-only client), the screens are drawn by one compact
//! [`MedleyView`]. The [`RowItem`] trait (+ `impl RowItem for core::Track`),
//! [`theme`]-loading, and the `:`-line [`command`] parser with its alias map
//! are kept as separate pieces.

use std::sync::{Arc, Mutex};

use cursive::event::EventResult;
use cursive::view::Nameable;
use cursive::views::NamedView;
use cursive::{Cursive, CursiveRunner};
use core::{CoreEvent, LastPlayed, Layout, LogBuf, Session};

pub mod command;
pub mod items;
pub mod keybindings;
pub mod theme;
mod row;
mod screen;
mod view;
mod vis;

pub use row::RowItem;
pub use view::{
    MedleyView, SCROLL_GAP, marquee_offset, player_state_glyph, scroll_title, window_title_track_text,
};

/// Cursive only flushes queued backend-side calls (e.g. `set_window_title`)
/// from inside its own `step()` — either right after a real input event, or,
/// when idle, once every `1000 / INPUT_POLL_DELAY_MS / fps` iterations if an
/// fps is set (see `cursive_core::CursiveRunner::post_events`). With no fps
/// set (the default whenever the Vis pane is closed), that idle flush path
/// never runs, so a title change queued from `app`'s own event-loop code
/// (which runs *after* `step()` returns) sits unflushed until the next real
/// keypress. A low always-on floor fps gives that idle flush a periodic
/// chance to run regardless of the Vis pane; `view.rs`'s own fps toggle for
/// the Vis pane must never drop below this floor.
pub const BASELINE_FPS: u32 = 4;

/// Shortest interval between frames while scrolling.
pub const SCROLL_FRAME: std::time::Duration = std::time::Duration::from_micros(1_000_000 / 45);

static SCROLLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a scroll gesture was handled since the last call.
pub fn take_scrolled() -> bool {
    SCROLLED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Shared handle to the single [`Session`]. Not application state — the same
/// `Rc` the `app` event loop pumps `on_event` on.
pub type SessionHandle = std::sync::Arc<Mutex<Session>>;

/// Create a `CursiveRunner` with the default terminal backend.
pub fn create_cursive() -> Result<CursiveRunner<Cursive>, Box<dyn std::error::Error>> {
    let backend = cursive::backends::try_default()?;
    Ok(CursiveRunner::new(Cursive::new(), backend))
}

/// The root view; `layout` is last run's `saved_layout`, `last_played` the track to select once it is found.
pub fn root_view(
    session: SessionHandle,
    log: Arc<LogBuf>,
    layout: Option<Layout>,
    last_played: Option<LastPlayed>,
) -> NamedView<MedleyView> {
    MedleyView::new(session, log, layout, last_played).with_name(ROOT)
}

/// The window layout to persist, read at shutdown.
pub fn saved_layout(siv: &mut Cursive) -> Option<Layout> {
    siv.call_on_name(ROOT, |view: &mut MedleyView| view.saved_layout())
}

const ROOT: &str = "medley";

/// Runs `f` on the shell from outside its own `on_event`, then the callback it returns.
pub(crate) fn on_root(siv: &mut Cursive, f: impl FnOnce(&mut MedleyView) -> EventResult) {
    if let Some(EventResult::Consumed(Some(cb))) = siv.call_on_name(ROOT, f) {
        cb(siv);
    }
}

/// Shows whatever the drained `events` carry for the user; `app` calls it once per batch.
pub fn deliver(siv: &mut Cursive, events: &[CoreEvent]) {
    for notice in events.iter().filter_map(view::Notice::of_event) {
        on_root(siv, |view| view.notify(notice));
    }
    on_root(siv, |view| {
        view.on_setup_events(events);
        EventResult::consumed()
    });
}
