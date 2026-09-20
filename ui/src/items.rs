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
    command(Cmd::NewPlaylist, BuiltinAction::AddToPlaylistOrNew, &["newplaylist", "np"], "<name>", "create a new playlist", "With a track selected the key picks a playlist to add it to instead."),
    command(Cmd::AddToPlaylist, BuiltinAction::PromptAddToPlaylist, &["add-to-playlist", "add"], "<playlist>", "add the selected track to a playlist by name", ""),
    command(
        Cmd::Open,
        BuiltinAction::PromptOpen,
        &["open", "o"],
        "[<url-or-path>]",
        "open a playlist link, an M3U file or local audio files",
        "A link opens as a playlist, an M3U file is imported, audio files are added to the open playlist. Without an argument: the files window (a file browser).",
    ),
    command(Cmd::Export, BuiltinAction::PromptExport, &["export", "ex"], "<playlist> [path]", "export a playlist to M3U", ""),
    action(BuiltinAction::ToggleLog, &["log", "l"], "toggle the log pane", ""),
    action(BuiltinAction::ToggleSettings, &["settings", "set"], "toggle the settings pane", ""),
    action(BuiltinAction::ToggleVis, &["vis", "v"], "toggle the real-audio bar-eq visualizer pane", ""),
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
        "Window names as for :window; without one, every window that is not a tab moves. A startup tab stays tabbed: naming it moves its companion window (tabbed closes it).",
    ),
    action(BuiltinAction::ToggleScan, &["togglescan", "ts"], "pause or resume the background scan (bpm, ...)", "Paused, it reads only what is cached."),
    action(BuiltinAction::Update, &["update"], "install the latest release into ~/.local/bin/medley", "Checks GitHub; restart to run the new version."),
    action(BuiltinAction::ToggleShuffle, &["toggleshuffle", "sh"], "toggle queue shuffle", ""),
    action(BuiltinAction::Link, &["link", "ln"], "merge two rows as one track", "Pick the selected row, then run :link again on a second row."),
    action(BuiltinAction::Unlink, &["unlink", "ul"], "unlink the selected track from its links", ""),
    action(BuiltinAction::OpenHelp, &["help", "h"], "open this window", "Enter on a row gives it a new key, Bksp its default back."),
    action(BuiltinAction::Quit, &["quit", "q"], "exit medley", ""),
    fixed(Section::Movement, "j/k", "move the cursor", ""),
    fixed(Section::Movement, "J/K", "move a page", ""),
    fixed(Section::Movement, "Enter", "play or open the selected row", ""),
    fixed(Section::Movement, "Esc", "clear the filter, leave the playlist, close a window that is not a tab", ""),
    fixed(Section::Movement, "/", "search, or fuzzy-filter the current list outside Search", ""),
    fixed(Section::Movement, ":", "open the command line", ""),
    fixed(Section::Movement, "1-9", "switch to that tab", ""),
    fixed(Section::Movement, "Tab", "focus the next window", ""),
    builtin(Section::Player, BuiltinAction::PlayPause, "play/pause", ""),
    builtin(Section::Player, BuiltinAction::Next, "next track", "> always works too."),
    builtin(Section::Player, BuiltinAction::Previous, "previous track", "< always works too."),
    builtin(Section::Player, BuiltinAction::SeekForward, "seek forward 5s", "→ always works too."),
    builtin(Section::Player, BuiltinAction::SeekBack, "seek back 5s", "← always works too."),
    builtin(Section::Player, BuiltinAction::RevealPlaying, "select the playing track in the active window", "Seeking does this too."),
    builtin(Section::Tracks, BuiltinAction::Enqueue, "enqueue the selected track", ""),
    builtin(Section::Tracks, BuiltinAction::Wedge, "wedge the selected track to the front of the queue", ""),
    builtin(Section::Tracks, BuiltinAction::ClearQueue, "clear the queue", ""),
    builtin(Section::Tracks, BuiltinAction::Like, "like the selected track (add to Liked Songs), or unlike it when it is already liked", "Confirms before removing."),
    builtin(Section::Tracks, BuiltinAction::ExportPlaylist, "export the selected or open local playlist as M3U", ""),
    builtin(Section::Tracks, BuiltinAction::CycleKindFilter, "cycle what the Search results show: all, songs, albums, playlists", ""),
    fixed(
        Section::Tracks,
        "any key",
        "bind key to playlist",
        "On a row of a Playlists list; Bksp clears it. With a track selected, a playlist's key adds it to or removes it from that playlist.",
    ),
    builtin(Section::Windows, BuiltinAction::SwitchPlaylists, "switch to the other Playlists window", "Opens the playlist keys window over the view when it is closed; on that window floating, closes it."),
    builtin(Section::Windows, BuiltinAction::CyclePlacement, "move the focused window: tabbed, docked, screen, float", ""),
    builtin(Section::Windows, BuiltinAction::CyclePaneLayout, "cycle the docked-pane layout", ""),
];

/// The item `word` names, by any of its spellings.
pub fn named(word: &str) -> Option<(Cmd, &'static Item)> {
    ITEMS.iter().find_map(|item| item.cmd.filter(|_| item.names.contains(&word)).map(|cmd| (cmd, item)))
}

/// What a built-in does, as its row says it.
pub fn describe(action: BuiltinAction) -> &'static str {
    ITEMS.iter().find(|item| item.key == Key::Builtin(action)).map_or(action.id(), |item| item.summary)
}
