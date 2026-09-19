//! Key → action map, targeting [`core::Command`] (no `a`).
//!
//! `/` and `1`-`9` are **UI-local** — they move focus / switch tabs and never reach `Session`.

use std::collections::HashMap;

use cursive::event::Event;

use core::{BuiltinAction, Command, HotkeyTarget, TrackId};

use crate::view::Nav;

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
    /// UI-local: open the playlist keys window over the view, or close it when it has focus.
    TogglePlaylistKeys,
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
fn fixed(key: char) -> Option<Action> {
    if let Some(n @ 1..=9) = key.to_digit(10) {
        return Some(Action::Tab(n as usize - 1));
    }
    Some(match key {
        '/' => Action::FocusSearch,
        ':' => Action::CommandLine,
        ' ' => Action::Command(Command::PlayPause),
        '>' => Action::Command(Command::Next),
        '<' => Action::Command(Command::Previous),
        'x' => Action::ExportPlaylist,
        _ => return None,
    })
}

/// Why a key is spoken for.
pub enum Taken {
    Fixed,
    Builtin(BuiltinAction),
}

/// The one rule for whether `key` can be bound: not fixed, not a list's navigation key, not a built-in's effective key.
pub fn taken(key: char, hotkeys: &HashMap<char, HotkeyTarget>) -> Option<Taken> {
    if fixed(key).is_some() || Nav::of(&Event::Char(key)).is_some() {
        return Some(Taken::Fixed);
    }
    builtin_at(hotkeys, key).map(Taken::Builtin)
}

/// What `ch` means with `selected` under the cursor; built-ins answer to their effective key in `hotkeys`.
pub fn map(ch: char, selected: Option<TrackId>, hotkeys: &HashMap<char, HotkeyTarget>) -> Action {
    if let Some(action) = fixed(ch) {
        return action;
    }
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
        BuiltinAction::TogglePlaylistKeys => Action::TogglePlaylistKeys,
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

/// The membership toggle `ch` means when it is bound to a playlist and a track is selected.
pub fn hotkey_toggle(ch: char, selected: Option<TrackId>, hotkeys: &HashMap<char, HotkeyTarget>) -> Option<Command> {
    let track = selected?;
    let playlist = match hotkeys.get(&ch)?.clone() {
        target @ (HotkeyTarget::Local(_) | HotkeyTarget::Remote(_, _)) => target,
        HotkeyTarget::Builtin(_) => return None,
    };
    Some(Command::TogglePlaylistMembership { track, playlist })
}
