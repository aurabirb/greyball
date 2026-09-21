//! Non-blocking startup health + deferred interactive setup for optional
//! source plugins (Spotify, SoundCloud, ...).
//!
//! Before this, `app`'s `wire_spotify`/`wire_soundcloud` ran a plugin's full
//! login (an OAuth browser flow, or a blocking stdin prompt) synchronously
//! during startup — so a user with no cached Spotify/SoundCloud credentials
//! had the whole app hang behind a login before the TUI even appeared. The
//! `Plugin` trait splits that into two halves: [`Plugin::probe`], which only
//! reads local disk state (no network, no interactive prompt) to report
//! whether the plugin already has what it needs, and [`Plugin::setup`],
//! which does the actual (possibly slow/interactive) fix-up but is never
//! called except by an explicit user action from the UI — see the
//! "warnings" panel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::event::{Bus, CoreEvent};
use crate::traits::{MediaProvider, Player, Source};
use crate::types::SourceId;

/// One question of a plugin's setup: `text` may span lines, and the answer is an input line under it.
#[derive(Clone, PartialEq, Eq)]
pub struct SetupPrompt {
    pub text: String,
    /// Masked wherever the answer is shown.
    pub secret: bool,
    /// What an empty answer means, shown next to a plain question.
    pub default: Option<String>,
}

impl SetupPrompt {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), secret: false, default: None }
    }

    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    pub fn default(mut self, value: impl Into<String>) -> Self {
        self.default = Some(value.into());
        self
    }
}

/// A running setup's link to the UI: progress lines out, abandonment in.
#[derive(Clone)]
pub struct SetupLog {
    id: SourceId,
    run: u64,
    bus: Bus,
    cancelled: Arc<AtomicBool>,
}

impl SetupLog {
    pub fn new(id: SourceId, bus: Bus) -> Self {
        static RUNS: AtomicU64 = AtomicU64::new(0);
        Self { id, run: RUNS.fetch_add(1, Ordering::Relaxed), bus, cancelled: Arc::default() }
    }

    pub fn id(&self) -> &SourceId {
        &self.id
    }

    pub fn run(&self) -> u64 {
        self.run
    }

    /// Adds a line to the setup dialog's log.
    pub fn say(&self, line: impl Into<String>) {
        self.bus.send(CoreEvent::SetupLine { id: self.id.clone(), run: self.run, line: line.into() });
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// The user abandoned the setup: stop waiting, its result is dropped.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// A plugin's health as of the last [`Plugin::probe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginHealth {
    /// Fully usable.
    Ok,
    /// Usable, but degraded — e.g. not logged in, so some functionality
    /// (browsing playlists, say) is unavailable until [`Plugin::setup`]
    /// runs. `0` is shown to the user and explains why.
    Warn(String),
    /// Not usable at all — excluded from the running session entirely.
    Fail(String),
}

impl PluginHealth {
    pub fn is_ok(&self) -> bool {
        matches!(self, PluginHealth::Ok)
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            PluginHealth::Ok => None,
            PluginHealth::Warn(m) | PluginHealth::Fail(m) => Some(m),
        }
    }
}

/// `Source`/`Player`/`MediaProvider` a plugin can currently offer, given its
/// present (possibly `Warn`) health — e.g. SoundCloud without a login still
/// offers a working (if browse-less) `Source`, while Spotify without one
/// offers nothing at all yet. Returned by [`Plugin::wiring`] at startup and
/// again after a successful [`Plugin::setup`], for the caller to register.
#[derive(Default, Clone)]
pub struct Wiring {
    pub source: Option<Arc<dyn Source>>,
    pub player: Option<Arc<dyn Player>>,
    pub media: Option<Arc<dyn MediaProvider>>,
}

/// A `:`-command a plugin registers at startup — the generic seam so a
/// plugin-specific command (e.g. Spotify's `spotify`) doesn't need
/// hand-adding to `ui::command`. Namespaced under the plugin's own leading
/// word, with everything after it handed to [`Plugin::run_command`] as
/// `arg` verbatim — so a plugin can grow its own sub-vocabulary (`spotify
/// addlogin ncspot`, say) without a second registration mechanism.
pub struct PluginCommand {
    /// The bare `:`-word, no leading `:`.
    pub word: String,
    /// Shown in `:help` alongside the built-ins.
    pub help: String,
}

/// An optional source plugin, wrapping whatever login/config state it needs
/// behind a uniform probe/setup interface the UI can drive generically
/// without knowing Spotify from SoundCloud.
pub trait Plugin: Send + Sync {
    fn id(&self) -> SourceId;

    /// Non-blocking: local disk reads only, no network, no interactive
    /// prompt. Safe to call from a UI render path on every redraw.
    fn probe(&self) -> PluginHealth;

    /// The next question given the `answers` collected so far (an empty answer already replaced by its
    /// default), or `None` once [`Plugin::setup`] can run (immediately, for a plugin needing no input). Non-blocking.
    fn setup_prompt(&self, _answers: &[String]) -> Option<SetupPrompt> {
        None
    }

    /// Checks the answer just given to the question `setup_prompt(answers)` asked (an empty answer already replaced by its
    /// default); `Err` is shown and the question repeats, `Ok` is the answer kept.
    fn setup_answer(&self, _answers: &[String], answer: String) -> Result<String, String> {
        Ok(answer)
    }

    /// One line on the plugin's detected state for Settings. Non-blocking; default: none.
    fn detail(&self) -> Option<String> {
        None
    }

    /// Best-effort `Source`/`Player`/`MediaProvider` for the current health.
    /// Also non-blocking — built from whatever's already on disk/in memory,
    /// same as `probe`.
    fn wiring(&self) -> Wiring;

    /// Run the actual fix-up. Blocking and possibly interactive (an OAuth
    /// browser flow, a network call) is fine here — the only caller is an
    /// explicit user action, always off the UI thread. `answers` holds one entry per
    /// `setup_prompt`; `log` reports progress to the dialog and says when the user gave up. Returns the
    /// resulting health; call `wiring()` again afterward to pick up anything newly available.
    fn setup(&self, answers: Vec<String>, log: &SetupLog) -> PluginHealth;

    /// `:`-commands this plugin wants registered at startup, beyond the
    /// generic probe/setup/wiring lifecycle every plugin already gets.
    /// Default: none.
    fn commands(&self) -> Vec<PluginCommand> {
        Vec::new()
    }

    /// Run one of `commands()`'s words. `arg` is everything on the `:`-line
    /// after the command word, verbatim (`None` if there was none). Blocking
    /// is fine here for the same reason it's fine in `setup` — the only
    /// caller runs it off the UI thread (see `ui::run_plugin_command`).
    /// Returns the message to show the user in a modal. Never called for a
    /// word `commands()` didn't return, so the default body is unreachable
    /// for a plugin with no commands.
    fn run_command(&self, _word: &str, _arg: Option<String>) -> String {
        String::new()
    }
}

/// Live media-provider registry shared by the session, scan driver and player, so a provider wired after setup reaches all of them.
#[derive(Clone, Default)]
pub struct SharedMedia(Arc<std::sync::RwLock<std::collections::HashMap<SourceId, Arc<dyn MediaProvider>>>>);

impl SharedMedia {
    pub fn insert(&self, id: SourceId, m: Arc<dyn MediaProvider>) {
        self.0.write().unwrap().insert(id, m);
    }

    pub fn contains(&self, id: &SourceId) -> bool {
        self.0.read().unwrap().contains_key(id)
    }

    /// Cheap Arc-clone copy, so callers never hold the lock across a blocking fetch.
    pub fn snapshot(&self) -> std::collections::HashMap<SourceId, Arc<dyn MediaProvider>> {
        self.0.read().unwrap().clone()
    }
}
