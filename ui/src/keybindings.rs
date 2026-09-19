//! Key → action map, targeting [`core::Command`] (no `a`).
//!
//! `/` and `1`-`9` are **UI-local** — they move focus / switch tabs and never reach `Session`.

use std::collections::HashMap;

use cursive::event::Event;

use core::{BuiltinAction, Command, HotkeyTarget, TrackId};

use crate::items;
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
    /// UI-local: switch to the other Playlists window, opening the playlist keys window when none is shown.
    SwitchPlaylists,
    /// UI-local: rotate the shared docked-pane dock through
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
    /// UI-local: open the command line with this command's long name and a space typed in.
    Prompt(&'static str),
    /// UI-local: export the selected or open local playlist as M3U.
    ExportPlaylist,
    /// UI-local: seek by this many ms, then reveal the playing track.
    Seek(i64),
    /// UI-local: open or close that window, or switch to its tab.
    ToggleWindow(&'static str),
    /// UI-local: bring that window into view.
    ShowWindow(&'static str),
    /// UI-local: put the cursor on the playing track in the active list.
    RevealPlaying,
    /// Nothing bound.
    None,
}

impl Action {
    /// A key about the windows themselves, which a window shown over the view still lets through.
    pub fn is_window_action(&self) -> bool {
        matches!(
            self,
            Action::Tab(_) | Action::CommandLine | Action::SwitchPlaylists | Action::CyclePlacement | Action::OpenHelp | Action::ToggleWindow(_) | Action::ShowWindow(_) | Action::Prompt(_)
        )
    }
}

/// The keys with one meaning everywhere, which no hotkey can take.
fn fixed(key: char) -> Option<Action> {
    if let Some(n @ 1..=9) = key.to_digit(10) {
        return Some(Action::Tab(n as usize - 1));
    }
    Some(match key {
        '/' => Action::FocusSearch,
        ':' => Action::CommandLine,
        '>' => Action::Command(Command::Next),
        '<' => Action::Command(Command::Previous),
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
    builtin_at(hotkeys, ch).map_or(Action::None, |action| builtin_action(action, selected))
}

/// What running `action` means with `selected` under the cursor.
pub fn builtin_action(action: BuiltinAction, selected: Option<TrackId>) -> Action {
    match action {
        BuiltinAction::Next => Action::Command(Command::Next),
        BuiltinAction::Previous => Action::Command(Command::Previous),
        BuiltinAction::SeekForward => Action::Seek(5000),
        BuiltinAction::SeekBack => Action::Seek(-5000),
        BuiltinAction::AddToPlaylistOrNew => match selected {
            Some(id) => Action::AddToPlaylistPrompt(id),
            None => prompt(action),
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
        BuiltinAction::SwitchPlaylists => Action::SwitchPlaylists,
        BuiltinAction::OpenHelp => Action::OpenHelp,
        BuiltinAction::RevealPlaying => Action::RevealPlaying,
        BuiltinAction::ToggleLog => Action::ToggleWindow("log"),
        BuiltinAction::ToggleSettings => Action::ToggleWindow("settings"),
        BuiltinAction::ToggleVis => Action::ToggleWindow("vis"),
        BuiltinAction::ToggleQueue => Action::ToggleWindow("queue"),
        BuiltinAction::ToggleHistory => Action::ToggleWindow("history"),
        BuiltinAction::ShowHistory => Action::ShowWindow("history-tab"),
        BuiltinAction::Link => selected.map_or(Action::None, |id| Action::Command(Command::LinkPick(id))),
        BuiltinAction::Unlink => selected.map_or(Action::None, |id| Action::Command(Command::Unlink(id))),
        BuiltinAction::PlayPause => Action::Command(Command::PlayPause),
        BuiltinAction::ExportPlaylist => Action::ExportPlaylist,
        BuiltinAction::PromptSearch
        | BuiltinAction::PromptAddToPlaylist
        | BuiltinAction::PromptOpen
        | BuiltinAction::PromptExport
        | BuiltinAction::PromptWindow
        | BuiltinAction::PromptPanes => prompt(action),
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
        (default == Some(ch) && !remapped_elsewhere).then_some(action)
    })
}

/// The membership toggle `ch` means when it is bound to a playlist and a track is selected;
/// `open_row` is the playlist open in the focused list and the selected row of it.
pub fn hotkey_toggle(
    ch: char,
    selected: Option<TrackId>,
    hotkeys: &HashMap<char, HotkeyTarget>,
    open_row: Option<(HotkeyTarget, usize)>,
) -> Option<Command> {
    let track = selected?;
    let playlist = match hotkeys.get(&ch)?.clone() {
        target @ (HotkeyTarget::Local(_) | HotkeyTarget::Remote(_, _)) => target,
        HotkeyTarget::Builtin(_) => return None,
    };
    let position = open_row.and_then(|(open, row)| (open == playlist).then_some(row));
    Some(Command::TogglePlaylistMembership { track, playlist, position })
}

/// The command-line prompt for the command `action` is the key of.
fn prompt(action: BuiltinAction) -> Action {
    let item = items::ITEMS.iter().find(|item| item.key == items::Key::Builtin(action));
    item.and_then(|item| item.names.first()).map_or(Action::None, |&name| Action::Prompt(name))
}
