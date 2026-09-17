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

use cursive::{Cursive, CursiveRunner};
use core::{LogBuf, Session};

pub mod command;
mod filebrowser;
pub mod keybindings;
pub mod theme;
mod row;
mod view;
mod vis;

pub use row::RowItem;
pub use view::{
    MedleyView, SCROLL_GAP, marquee_offset, player_state_icon, scroll_title, window_title_track_text,
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

/// Shared handle to the single [`Session`]. Not application state — the same
/// `Rc` the `app` event loop pumps `on_event` on.
pub type SessionHandle = std::sync::Arc<Mutex<Session>>;

/// Create a `CursiveRunner` with the default terminal backend.
pub fn create_cursive() -> Result<CursiveRunner<Cursive>, Box<dyn std::error::Error>> {
    let backend = cursive::backends::try_default()?;
    Ok(CursiveRunner::new(Cursive::new(), backend))
}

/// Build the root view for the three screens, seeded with the configured
/// initial screen. `log` backs the optional Log pane (`:log`) — shared with
/// whatever sink `app` installs for the `log` crate.
pub fn root_view(session: SessionHandle, initial_screen: &str, log: Arc<LogBuf>) -> MedleyView {
    MedleyView::new(session, initial_screen, log)
}
