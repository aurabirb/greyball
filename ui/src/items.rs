//! The one table of what the user can do: `:`-command spellings, descriptions and keys.

use core::BuiltinAction;

/// Help sections, in display order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Player,
    Hotkeys,
    Tracks,
    Windows,
    Movement,
    Commands,
}

impl Section {
    pub const ALL: [Section; 6] =
        [Section::Player, Section::Hotkeys, Section::Tracks, Section::Windows, Section::Movement, Section::Commands];

    pub fn title(self) -> &'static str {
        match self {
            Section::Player => "Player",
            Section::Hotkeys => "Custom playlist hotkeys",
            Section::Tracks => "Tracks and playlists",
            Section::Windows => "Windows",
            Section::Movement => "Movement",
            Section::Commands => "Commands",
        }
    }
}

/// The key an item answers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// Structural, matched as an event in code; shown as written.
    Fixed(&'static str),
    /// Rebindable; the shortcut shown is its live effective key.
    Builtin(BuiltinAction),
}

/// A `:`-command, which `command::parse` matches on exhaustively; a `Builtin` takes no argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Search,
    NewPlaylist,
    AddToPlaylist,
    Open,
    Export,
    Window,
    Panes,
    Builtin(BuiltinAction),
}

pub struct Item {
    /// `None` for a key-only item.
    pub cmd: Option<Cmd>,
    pub section: Section,
    /// A command's two `:`-spellings, long then short; empty for a key-only item.
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

const fn command(cmd: Cmd, action: BuiltinAction, names: &'static [&'static str], args: &'static str, summary: &'static str, detail: &'static str) -> Item {
    Item { cmd: Some(cmd), section: Section::Commands, names, args, summary, detail, key: Key::Builtin(action) }
}

const fn action(action: BuiltinAction, names: &'static [&'static str], summary: &'static str, detail: &'static str) -> Item {
    Item { cmd: Some(Cmd::Builtin(action)), section: Section::Commands, names, args: "", summary, detail, key: Key::Builtin(action) }
}

const fn key(section: Section, key: Key, summary: &'static str, detail: &'static str) -> Item {
    Item { cmd: None, section, names: &[], args: "", summary, detail, key }
}

const fn builtin(section: Section, action: BuiltinAction, summary: &'static str, detail: &'static str) -> Item {
    key(section, Key::Builtin(action), summary, detail)
}

const fn fixed(section: Section, keys: &'static str, summary: &'static str, detail: &'static str) -> Item {
    key(section, Key::Fixed(keys), summary, detail)
}

pub const ITEMS: &[Item] = &[
    command(Cmd::Search, BuiltinAction::PromptSearch, &["search", "s"], "<query>", "search all sources for tracks", ""),
    command(Cmd::NewPlaylist, BuiltinAction::AddToPlaylistOrNew, &["newplaylist", "np"], "<name>", "create a new playlist", ""),
    command(Cmd::AddToPlaylist, BuiltinAction::PromptAddToPlaylist, &["add-to-playlist", "add"], "<playlist>", "add the selected track to a playlist by name", ""),
    command(
        Cmd::Open,
        BuiltinAction::PromptOpen,
        &["open", "o"],
        "[<url-or-path>]",
        "open a playlist link, an M3U file or local audio files",
        "Link: opens as a playlist. M3U: imported. Audio files: added to the open playlist. No argument: the file browser.",
    ),
    command(Cmd::Export, BuiltinAction::PromptExport, &["export", "ex"], "<playlist> [path]", "export a playlist to M3U", ""),
    action(BuiltinAction::ToggleLog, &["log", "l"], "toggle the log pane", ""),
    action(BuiltinAction::ToggleSettings, &["settings", "set"], "toggle the settings pane", ""),
    action(BuiltinAction::ToggleVis, &["vis", "v"], "toggle the visualizer pane", ""),
    action(BuiltinAction::ToggleQueue, &["queue", "qu"], "toggle the queue pane", ""),
    action(BuiltinAction::ToggleHistory, &["history", "hi"], "toggle the history pane", ""),
    action(BuiltinAction::ShowHistory, &["hist", "ht"], "show the History tab window (history-tab)", ""),
    command(
        Cmd::Window,
        BuiltinAction::PromptWindow,
        &["window", "w"],
        "<window>",
        "open or close any window, or switch to its tab",
        "The windows: now-playing, playlists, search, history-tab, queue-tab (the startup tabs), log, settings, vis, queue, history (the panes), playlist-keys, help, files.",
    ),
    command(
        Cmd::Panes,
        BuiltinAction::PromptPanes,
        &["panes", "p"],
        "[<window>] [tabbed|docked|screen|float] [left|right|top|bottom] [horizontal|vertical]",
        "move a window to the tab bar, the dock, fullscreen or a box over the view",
        "Names as for :window; none moves every non-tab window. Naming a startup tab moves its companion window.",
    ),
    action(BuiltinAction::ToggleScan, &["togglescan", "ts"], "pause or resume the background scan (bpm, ...)", ""),
    action(BuiltinAction::Update, &["update"], "install the latest release into ~/.local/bin/medley (restart after)", ""),
    action(BuiltinAction::ToggleShuffle, &["toggleshuffle", "sh"], "toggle queue shuffle", ""),
    action(BuiltinAction::Link, &["link", "ln"], "merge two rows as one track (run it on each row)", ""),
    action(BuiltinAction::Unlink, &["unlink", "ul"], "unlink the selected track from its links", ""),
    action(BuiltinAction::OpenHelp, &["help", "h"], "open this window", ""),
    action(BuiltinAction::Quit, &["quit", "q"], "exit medley", ""),
    builtin(Section::Player, BuiltinAction::PlayPause, "play/pause", ""),
    builtin(Section::Player, BuiltinAction::Next, "next track (>)", ""),
    builtin(Section::Player, BuiltinAction::Previous, "previous track (<)", ""),
    builtin(Section::Player, BuiltinAction::SeekForward, "seek forward 5s (→)", ""),
    builtin(Section::Player, BuiltinAction::SeekBack, "seek back 5s (←)", ""),
    builtin(Section::Player, BuiltinAction::ToggleShuffle, "toggle queue shuffle", ""),
    builtin(Section::Player, BuiltinAction::LikePlaying, "like the playing track, or unlike it (asks first)", ""),
    builtin(Section::Player, BuiltinAction::ToggleVis, "toggle the visualizer pane", ""),
    builtin(Section::Player, BuiltinAction::RevealPlaying, "select the playing track (seeking does too)", ""),
    fixed(
        Section::Hotkeys,
        "any key",
        "on a Playlists row, press a key to bind it (Bksp clears)",
        "With a track selected, that key adds it to the playlist or removes it.",
    ),
    builtin(Section::Hotkeys, BuiltinAction::SwitchPlaylists, "switch to the playlist keys window (opens it when closed)", ""),
    builtin(Section::Tracks, BuiltinAction::AddToPlaylistOrNew, "add the selected track to a playlist, or make a new one", ""),
    builtin(Section::Tracks, BuiltinAction::CopyLink, "copy the selected track's shareable web link", ""),
    builtin(Section::Tracks, BuiltinAction::Enqueue, "enqueue the selected track, playlist or album", ""),
    builtin(Section::Tracks, BuiltinAction::Wedge, "wedge the selected track to the front of the queue", ""),
    builtin(Section::Tracks, BuiltinAction::ClearQueue, "clear the queue", ""),
    builtin(Section::Tracks, BuiltinAction::Like, "like the selected track, or unlike it (asks first)", ""),
    builtin(Section::Tracks, BuiltinAction::ExportPlaylist, "export the selected or open local playlist as M3U", ""),
    builtin(Section::Tracks, BuiltinAction::CycleKindFilter, "cycle the list's kinds: all, songs, albums, playlists", ""),
    builtin(Section::Windows, BuiltinAction::OpenHelp, "open or leave this window (Enter rebinds a row, Bksp resets it)", ""),
    builtin(Section::Windows, BuiltinAction::CyclePlacement, "move the focused window: tabbed, docked, screen, float", ""),
    builtin(Section::Windows, BuiltinAction::CyclePaneLayout, "cycle the docked-pane layout", ""),
    fixed(Section::Windows, "1-9", "switch to that tab", ""),
    fixed(Section::Windows, "Tab", "focus the next window", ""),
    fixed(Section::Movement, "j/k", "move the cursor (↑/↓ too)", ""),
    fixed(Section::Movement, "J/K", "move a page (PgUp/PgDn too)", ""),
    fixed(Section::Movement, "Enter", "play or open the selected row", ""),
    fixed(Section::Movement, "Esc", "clear the filter, go back, close a window that is not a tab", ""),
    fixed(Section::Movement, "/", "search in Search, filter the current list elsewhere", ""),
    fixed(Section::Movement, ":", "open the command line", ""),
];

/// The item `word` names, by any of its spellings.
pub fn named(word: &str) -> Option<(Cmd, &'static Item)> {
    ITEMS.iter().find_map(|item| item.cmd.filter(|_| item.names.contains(&word)).map(|cmd| (cmd, item)))
}

/// What a built-in does, as its row says it.
pub fn describe(action: BuiltinAction) -> &'static str {
    ITEMS.iter().find(|item| item.key == Key::Builtin(action)).map_or(action.id(), |item| item.summary)
}
