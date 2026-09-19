//! The one table of what the user can do: `:`-command spellings, descriptions and keys.

use core::BuiltinAction;

/// Help sections, in display order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Commands,
    Movement,
    Player,
    Tracks,
    Windows,
}

impl Section {
    pub const ALL: [Section; 5] =
        [Section::Commands, Section::Movement, Section::Player, Section::Tracks, Section::Windows];

    pub fn title(self) -> &'static str {
        match self {
            Section::Commands => "Commands",
            Section::Movement => "Movement",
            Section::Player => "Player",
            Section::Tracks => "Tracks and playlists",
            Section::Windows => "Windows",
        }
    }
}

/// The key an item answers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    None,
    /// Structural, matched as an event in code; shown as written.
    Fixed(&'static str),
    /// Rebindable; the shortcut shown is its live effective key.
    Builtin(BuiltinAction),
}

pub struct Item {
    pub section: Section,
    /// `:`-spellings, canonical first; empty for a key-only item.
    pub names: &'static [&'static str],
    /// `<required>` and `[optional]` arguments as the user types them.
    pub args: &'static str,
    pub summary: &'static str,
    pub detail: &'static str,
    pub key: Key,
}

impl Item {
    /// The command cell: `:name/:alias <args>`.
    pub fn command(&self) -> String {
        let names: Vec<String> = self.names.iter().map(|name| format!(":{name}")).collect();
        [names.join("/"), self.args.to_string()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")
    }

    pub fn usage(&self) -> String {
        format!("usage: {}", self.command().trim_start_matches(':'))
    }
}

const fn command(names: &'static [&'static str], args: &'static str, summary: &'static str, detail: &'static str) -> Item {
    Item { section: Section::Commands, names, args, summary, detail, key: Key::None }
}

const fn keyed(item: Item, action: BuiltinAction) -> Item {
    Item { key: Key::Builtin(action), ..item }
}

const fn key(section: Section, key: Key, summary: &'static str, detail: &'static str) -> Item {
    Item { section, names: &[], args: "", summary, detail, key }
}

const fn builtin(section: Section, action: BuiltinAction, summary: &'static str, detail: &'static str) -> Item {
    key(section, Key::Builtin(action), summary, detail)
}

const fn fixed(section: Section, keys: &'static str, summary: &'static str, detail: &'static str) -> Item {
    key(section, Key::Fixed(keys), summary, detail)
}

pub const ITEMS: &[Item] = &[
    command(&["search", "s", "find"], "<query>", "search all sources for tracks", ""),
    keyed(
        command(&["newplaylist", "np", "newpl"], "<name>", "create a new playlist", "With a track selected the key picks a playlist to add it to instead."),
        BuiltinAction::AddToPlaylistOrNew,
    ),
    command(&["add-to-playlist", "add", "atp"], "<playlist>", "add the selected track to a playlist by name", ""),
    command(
        &["open"],
        "[<url-or-path>]",
        "open a playlist link, an M3U file or local audio files",
        "A link opens as a playlist, an M3U file is imported, audio files are added to the open playlist. Without an argument: a file browser.",
    ),
    command(&["export"], "<playlist> [path]", "export a playlist to M3U", ""),
    command(&["log"], "", "toggle the log pane", ""),
    command(&["settings"], "", "toggle the settings pane", ""),
    command(&["vis"], "", "toggle the real-audio bar-eq visualizer pane", ""),
    command(&["queue"], "", "toggle the queue pane", ""),
    command(&["history"], "", "toggle the history pane", ""),
    command(&["hist"], "", "show the History tab window (history-tab)", ""),
    command(
        &["window"],
        "<window>",
        "open or close any window, or switch to its tab",
        "The windows: now-playing, playlists, search, history-tab, queue-tab (the startup tabs), log, settings, vis, queue, history (the panes), playlist-keys, help.",
    ),
    command(
        &["panes"],
        "[<window>] [tabbed|embedded|screen|float] [left|right|top|bottom] [horizontal|vertical]",
        "move a window to the tab bar, the dock, fullscreen or a box over the view",
        "Window names as for :window; without one, every window that is not a tab moves. The last tab stays tabbed.",
    ),
    keyed(command(&["togglescan"], "", "pause or resume the background scan (bpm, ...)", "Paused, it reads only what is cached."), BuiltinAction::ToggleScan),
    keyed(command(&["toggleshuffle"], "", "toggle queue shuffle", ""), BuiltinAction::ToggleShuffle),
    command(&["link"], "", "merge two rows as one track", "Pick the selected row, then run :link again on a second row."),
    command(&["unlink"], "", "unlink the selected track from its links", ""),
    keyed(
        command(&["help", "h", "?", "keys"], "", "open this window", "Enter on a row gives it a new key, Backspace its default back."),
        BuiltinAction::OpenHelp,
    ),
    keyed(command(&["quit", "q", "exit"], "", "exit medley", ""), BuiltinAction::Quit),
    fixed(Section::Movement, "↑/↓ j/k", "move the cursor", ""),
    fixed(Section::Movement, "PgUp/PgDn K/J", "move a page", ""),
    fixed(Section::Movement, "Enter", "play or open the selected row", ""),
    fixed(Section::Movement, "Esc", "clear the filter, leave the playlist, close the window", ""),
    fixed(Section::Movement, "/", "search, or fuzzy-filter the current list outside Search", ""),
    fixed(Section::Movement, ":", "open the command line", ""),
    fixed(Section::Movement, "1-9", "switch to that tab", ""),
    fixed(Section::Movement, "Tab", "focus the next window", ""),
    fixed(Section::Player, "Space", "play/pause", ""),
    builtin(Section::Player, BuiltinAction::Next, "next track", "> always works too."),
    builtin(Section::Player, BuiltinAction::Previous, "previous track", "< always works too."),
    builtin(Section::Player, BuiltinAction::SeekForward, "seek forward 5s", "→ always works too."),
    builtin(Section::Player, BuiltinAction::SeekBack, "seek back 5s", "← always works too."),
    builtin(Section::Tracks, BuiltinAction::Enqueue, "enqueue the selected track", ""),
    builtin(Section::Tracks, BuiltinAction::Wedge, "wedge the selected track to the front of the queue", ""),
    builtin(Section::Tracks, BuiltinAction::ClearQueue, "clear the queue", ""),
    builtin(Section::Tracks, BuiltinAction::Like, "like the selected track (add to Liked Songs)", ""),
    builtin(Section::Tracks, BuiltinAction::Unlike, "unlike the selected track", "Confirms first."),
    fixed(Section::Tracks, "x", "export the selected or open local playlist as M3U", ""),
    fixed(
        Section::Tracks,
        "other keys",
        "on a row of a Playlists list: bind that key to the playlist",
        "Backspace clears it. With a track selected, a playlist's key adds it to or removes it from that playlist.",
    ),
    builtin(Section::Windows, BuiltinAction::TogglePlaylistKeys, "open the playlist keys window over the view", "Again, or Esc, closes it."),
    builtin(Section::Windows, BuiltinAction::CyclePlacement, "move the focused window: tabbed, embedded, screen, float", ""),
    builtin(Section::Windows, BuiltinAction::CyclePaneLayout, "cycle the embedded-pane layout", ""),
];

/// The item `word` names, by any of its spellings.
pub fn named(word: &str) -> Option<&'static Item> {
    ITEMS.iter().find(|item| item.names.contains(&word))
}

/// What a built-in does, as its row says it.
pub fn describe(action: BuiltinAction) -> &'static str {
    ITEMS.iter().find(|item| item.key == Key::Builtin(action)).map_or(action.id(), |item| item.summary)
}
