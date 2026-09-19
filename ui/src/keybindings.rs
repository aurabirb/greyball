//! Key → action map, targeting [`core::Command`] (no `a`).
//!
//! `/` and `1`-`9` are **UI-local** — they move focus / switch tabs and never reach `Session`.

use std::collections::HashMap;

use core::{BuiltinAction, Command, HotkeyTarget, TrackId};

/// What a keypress means outside an active text field.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Forward this to `Session::dispatch`.
    Command(Command),
    /// UI-local: `/` — filters the active list, or focuses the Search input when that list is the Search one.
    FocusSearch,
    /// UI-local: switch to the tab at this tab-bar position, from 0.
    Tab(usize),
    /// UI-local: open the `:` command line.
    CommandLine,
    /// UI-local: open the built-ins remap menu.
    OpenHotkeyMenu,
    /// UI-local: rotate the shared embedded-pane dock through
    /// right+vertical -> bottom+horizontal -> left+vertical ->
    /// top+horizontal -> back to right+vertical (`view::PANE_LAYOUT_CYCLE`)
    /// — one key instead of four `:panes <side> <stack>` combos.
    CyclePaneLayout,
    /// UI-local: move the focused window to its next placement.
    CyclePlacement,
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
    /// UI-local: export the selected or open local playlist as M3U.
    ExportPlaylist,
    /// Nothing bound.
    None,
}

/// The keys with one meaning everywhere, which no hotkey can take.
fn fixed(key: &str) -> Option<Action> {
    if let Ok(n @ 1..=9) = key.parse::<usize>() {
        return Some(Action::Tab(n - 1));
    }
    Some(match key {
        "/" => Action::FocusSearch,
        ":" => Action::CommandLine,
        "Space" => Action::Command(Command::PlayPause),
        ">" => Action::Command(Command::Next),
        "<" => Action::Command(Command::Previous),
        "x" => Action::ExportPlaylist,
        _ => return None,
    })
}

pub fn is_fixed(key: char) -> bool {
    fixed(&key.to_string()).is_some()
}

/// The character a playlist can take by `key` being pressed on its row: not fixed, not a built-in's effective key.
pub fn bindable(key: &str, hotkeys: &HashMap<char, HotkeyTarget>) -> Option<char> {
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else { return None };
    (fixed(key).is_none() && builtin_at(hotkeys, ch).is_none()).then_some(ch)
}

/// What `key` means with `selected` under the cursor; built-ins answer to their effective key in `hotkeys`.
pub fn map(key: &str, selected: Option<TrackId>, hotkeys: &HashMap<char, HotkeyTarget>) -> Action {
    if let Some(action) = fixed(key) {
        return action;
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
        BuiltinAction::CyclePlacement => Action::CyclePlacement,
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

/// Each key and what it does, for the help screen.
pub const RAW_KEYS: &[(&str, &str)] = &[
    ("/", "focus search (or fuzzy-filter the current list, outside Search)"),
    (":", "open the command line (:help)"),
    ("1-9", "switch to that tab"),
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
    ("M", "move the focused window: tabbed, embedded, screen, float"),
    ("E", "clear the queue"),
    ("Q", "quit"),
    ("`", "open the hotkeys menu"),
    ("x", "export the selected or open local playlist as M3U"),
    ("other keys", "on a row of the Playlists list: bind that key to the playlist (Backspace clears it)"),
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

