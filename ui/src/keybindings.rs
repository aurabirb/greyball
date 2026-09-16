//! Key → action map, targeting [`core::Command`] (no `a`).
//!
//! `/`, `1`-`5` are **UI-local** — they move focus / swap the visible
//! screen (now playing/playlists/search/history/queue) and never reach
//! `Session`.

use std::collections::HashMap;

use core::{BuiltinAction, Command, HotkeyTarget, TrackId};

/// What a keypress means outside an active text field.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Forward this to `Session::dispatch`.
    Command(Command),
    /// UI-local: focus the search edit box on the Search screen, or start a
    /// screen-local fuzzy filter over the current track list everywhere
    /// else — the view decides which, since it's the one that knows the
    /// current screen (`map` has no screen context).
    FocusSearch,
    /// UI-local: switch to screen 0-4 (now playing/queue/playlists/
    /// history/search — the raw `view::NOW_PLAYING`/`QUEUE`/`PLAYLISTS`/
    /// `HIST`/`SEARCH` constant, not a hotkey/tab-bar position).
    Screen(usize),
    /// UI-local: open the `:` command line.
    CommandLine,
    /// UI-local: activate the selected row (open a playlist, etc).
    Activate,
    /// Play the selected track as part of whatever list it's currently
    /// visible in (search results, a playlist, Liked Songs, ...) — the view
    /// resolves that list into `Command::PlayContext`, since `map` here has
    /// no access to it.
    PlayFromContext(TrackId),
    /// UI-local: open the "Playlist Hotkeys" management modal (backtick,
    /// nothing selected required) — lists every local playlist with its
    /// bound key, if any, and lets the user (re)bind or clear one. The view
    /// owns the modal's state, since `map` here has no access to the
    /// playlist list or the live hotkey bindings. The view intercepts
    /// backtick before it ever reaches `map` when a playlist is selected on
    /// the Playlists screen, going straight to the "press a key to bind"
    /// step for it instead — this `Action` is only ever produced otherwise.
    OpenHotkeyMenu,
    /// UI-local: rotate the shared embedded-pane dock through
    /// right+vertical -> bottom+horizontal -> left+vertical ->
    /// top+horizontal -> back to right+vertical (`view::PANE_LAYOUT_CYCLE`)
    /// — one key instead of four `:panes <side> <stack>` combos.
    CyclePaneLayout,
    /// UI-local: open the fullscreen help/shortcuts screen directly (`?`) —
    /// same destination as `:help`, just without going through the command
    /// line. The view owns the modal's state.
    OpenHelp,
    /// UI-local: open the "Add to Playlist" picker for the given track
    /// (`+`, a track selected) — a small scrollable list of local
    /// playlists; picking one calls `Command::AddToPlaylist`. The view owns
    /// the modal's state, since `map` here has no access to the playlist
    /// list.
    AddToPlaylistPrompt(TrackId),
    /// UI-local: start the "new playlist" name entry (`+`, no track
    /// selected — e.g. cursor on a playlist row, or nothing selected).
    /// Routed through the existing `:newplaylist <name>` command-line path.
    NewPlaylistPrompt,
    /// UI-local: open the Yes/No "remove from Liked Songs?" confirm dialog
    /// for the given track (`F`, a track selected) — the view owns the
    /// dialog; a "yes" answer is what actually dispatches `Command::Unlike`.
    ConfirmUnlike(TrackId),
    /// Nothing bound.
    None,
}

/// Map a key name (as produced by [`crate::view`]) plus the currently selected
/// track to an [`Action`]. `selected` is `None` when the cursor is on a
/// non-track row (e.g. the playlist list). `hotkeys` is the live remap table
/// (`Session::hotkeys`) — every [`BuiltinAction`] below is looked up against
/// it (falling back to [`BuiltinAction::default_key`] when unremapped)
/// instead of being a fixed key, so `:keys`/the hotkey menu can move it.
/// `/`, `:`, `1`-`5`, `Enter`, `Space` stay hardcoded: they're structural
/// (screen/focus navigation, not a "command"), not remappable commands.
pub fn map(key: &str, selected: Option<TrackId>, hotkeys: &HashMap<char, HotkeyTarget>) -> Action {
    match key {
        "/" => return Action::FocusSearch,
        ":" => return Action::CommandLine,
        // Hotkey order: 1 Now Playing, 2 Playlists, 3 Search, 4 History,
        // 5 Queue — matches `view::TABS`, not the raw constants' numeric
        // values (`view::QUEUE`/`PLAYLISTS`/`HIST`/`SEARCH`).
        "1" => return Action::Screen(0), // NOW_PLAYING
        "2" => return Action::Screen(2), // PLAYLISTS
        "3" => return Action::Screen(4), // SEARCH
        "4" => return Action::Screen(3), // HIST
        "5" => return Action::Screen(1), // QUEUE
        "Space" => return Action::Command(Command::PlayPause),
        // Fixed aliases regardless of whether `n`/`p` themselves were
        // remapped — `>`/`<` are punctuation, not commands of their own.
        ">" => return Action::Command(Command::Next),
        "<" => return Action::Command(Command::Previous),
        "Enter" => {
            return match selected {
                Some(id) => Action::PlayFromContext(id),
                None => Action::Activate,
            };
        }
        _ => {}
    }
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return Action::None;
    };
    let Some(action) = builtin_at(hotkeys, ch) else {
        return Action::None;
    };
    match action {
        BuiltinAction::Next => Action::Command(Command::Next),
        BuiltinAction::Previous => Action::Command(Command::Previous),
        BuiltinAction::SeekForward => Action::Command(Command::Seek(5000)),
        BuiltinAction::SeekBack => Action::Command(Command::Seek(-5000)),
        BuiltinAction::AddToPlaylistOrNew => match selected {
            Some(id) => Action::AddToPlaylistPrompt(id),
            None => Action::NewPlaylistPrompt,
        },
        BuiltinAction::Quit => Action::Command(Command::Quit),
        BuiltinAction::ClearQueue => Action::Command(Command::ClearQueue),
        BuiltinAction::ToggleScan => Action::Command(Command::ToggleScan),
        BuiltinAction::ToggleShuffle => Action::Command(Command::ToggleShuffle),
        BuiltinAction::CyclePaneLayout => Action::CyclePaneLayout,
        BuiltinAction::Enqueue => match selected {
            Some(id) => Action::Command(Command::Enqueue(id)),
            None => Action::None,
        },
        BuiltinAction::Wedge => match selected {
            Some(id) => Action::Command(Command::Wedge(id)),
            None => Action::None,
        },
        BuiltinAction::Like => selected
            .map(|id| Action::Command(Command::Like(id)))
            .unwrap_or(Action::None),
        BuiltinAction::Unlike => selected.map(Action::ConfirmUnlike).unwrap_or(Action::None),
        BuiltinAction::OpenHotkeyMenu => Action::OpenHotkeyMenu,
        BuiltinAction::OpenHelp => Action::OpenHelp,
    }
}

/// Which [`BuiltinAction`] (if any) `ch` currently activates: an explicit
/// remap in `hotkeys` wins; otherwise `ch` must be some action's still-
/// unremapped default (checked by scanning `BuiltinAction::ALL` — cheap,
/// bounded by the small fixed action count, and run once per keypress).
fn builtin_at(hotkeys: &HashMap<char, HotkeyTarget>, ch: char) -> Option<BuiltinAction> {
    if let Some(HotkeyTarget::Builtin(action)) = hotkeys.get(&ch) {
        return Some(*action);
    }
    BuiltinAction::ALL.iter().find_map(|&(action, default)| {
        let remapped_elsewhere = hotkeys.values().any(|t| *t == HotkeyTarget::Builtin(action));
        (default == ch && !remapped_elsewhere).then_some(action)
    })
}

/// The raw (non-`:`) keybindings, for the `?`/`:help` shortcuts screen.
/// `(key, description)`, in the same shape as `command::HELP`. Kept as a
/// separate table (not derived from `map()`) since `map()` carries no
/// description text — update this alongside `map()` when a binding changes.
pub const RAW_KEYS: &[(&str, &str)] = &[
    ("/", "focus search (or fuzzy-filter the current list, outside Search)"),
    (":", "open the command line (:help)"),
    ("1-5", "switch screen (now playing/playlists/search/history/queue)"),
    ("Tab", "switch focused pane"),
    ("Space", "play/pause"),
    ("n / p, > / <", "next/previous track"),
    (". / ,", "seek forward/back 5s"),
    ("+", "add selected track to a playlist / new playlist"),
    ("Enter", "play/activate the selected row"),
    ("q", "enqueue the selected track"),
    ("w", "wedge the selected track to the front of the queue"),
    ("l", "like the selected track (add to Liked Songs)"),
    ("L", "unlike the selected track (confirms first)"),
    ("B", "pause/resume the background scan"),
    ("P", "cycle the embedded-pane layout"),
    ("E", "clear the queue"),
    ("Q", "quit"),
    ("`", "open the playlist hotkeys menu"),
    ("?", "open this help/shortcuts screen"),
];

/// `key`'s dynamic playlist-hotkey binding, if any: `hotkeys` is the live
/// set of currently-bound letters, unknowable to `map` above (that's a
/// static, per-key-always-the-same table; this is runtime per-user state).
/// Checked in `MedleyView::on_event` before falling through to `map` — a
/// hit dispatches `TogglePlaylistMembership` directly instead of an
/// `Action`. `None` whenever `key` isn't a single char, nothing is
/// selected, that char has no binding, or it's bound to a `Builtin` (those
/// are handled by `map` falling through to `builtin_at`, not this toggle).
pub fn hotkey_toggle(
    key: &str,
    selected: Option<TrackId>,
    hotkeys: &HashMap<char, HotkeyTarget>,
) -> Option<Command> {
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let track = selected?;
    let playlist = match hotkeys.get(&ch)?.clone() {
        target @ (HotkeyTarget::Local(_) | HotkeyTarget::Remote(_, _)) => target,
        HotkeyTarget::Builtin(_) => return None,
    };
    Some(Command::TogglePlaylistMembership { track, playlist })
}

