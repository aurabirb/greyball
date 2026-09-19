/// What a window shows; any number of windows may share one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    List(ListKind),
    Log,
    Settings,
    Vis,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::List(list) => list.label(),
            Kind::Log => "Log",
            Kind::Settings => "Settings",
            Kind::Vis => "Vis",
        }
    }
}

/// Which list a track-list window shows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ListKind {
    /// The track list `Command::PlayContext` last started playing from; independent of any Playlists window's state.
    NowPlaying,
    Playlists,
    Search,
    History,
    Queue,
}

impl ListKind {
    pub fn label(self) -> &'static str {
        match self {
            ListKind::NowPlaying => "Now Playing",
            ListKind::Playlists => "Playlists",
            ListKind::Search => "Search",
            ListKind::History => "History",
            ListKind::Queue => "Queue",
        }
    }
}

/// Where the shell shows a window: as a tab, docked beside the active tab, fullscreen, or in a box over the view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placement {
    Tabbed,
    Docked,
    Screen,
    Floating,
}

impl Placement {
    /// The mode key's rotation.
    pub const CYCLE: [Placement; 4] = [Placement::Tabbed, Placement::Docked, Placement::Screen, Placement::Floating];

    /// What `:panes`, `state.toml`, Settings and the mode flash call it.
    pub fn word(self) -> &'static str {
        match self {
            Placement::Tabbed => "tabbed",
            Placement::Docked => "embedded",
            Placement::Screen => "screen",
            Placement::Floating => "float",
        }
    }

    pub fn from_word(word: &str) -> Option<Placement> {
        Placement::CYCLE.into_iter().find(|placement| placement.word() == word)
    }
}

impl From<core::PaneMode> for Placement {
    fn from(mode: core::PaneMode) -> Self {
        match mode {
            core::PaneMode::Screen => Placement::Screen,
            core::PaneMode::Embedded => Placement::Docked,
            core::PaneMode::Float => Placement::Floating,
        }
    }
}

/// A startup window: the name `:panes`, `:window` and `state.toml` know it by, and whether it starts as a tab.
pub struct Startup {
    pub name: &'static str,
    pub kind: Kind,
    pub tabbed: bool,
}

/// Every window startup builds, tabs first in tab order; Queue and History each have a tab and a pane instance.
pub const WINDOWS: [Startup; 10] = [
    Startup { name: "now-playing", kind: Kind::List(ListKind::NowPlaying), tabbed: true },
    Startup { name: "playlists", kind: Kind::List(ListKind::Playlists), tabbed: true },
    Startup { name: "search", kind: Kind::List(ListKind::Search), tabbed: true },
    Startup { name: "history-tab", kind: Kind::List(ListKind::History), tabbed: true },
    Startup { name: "queue-tab", kind: Kind::List(ListKind::Queue), tabbed: true },
    Startup { name: "log", kind: Kind::Log, tabbed: false },
    Startup { name: "settings", kind: Kind::Settings, tabbed: false },
    Startup { name: "vis", kind: Kind::Vis, tabbed: false },
    Startup { name: "queue", kind: Kind::List(ListKind::Queue), tabbed: false },
    Startup { name: "history", kind: Kind::List(ListKind::History), tabbed: false },
];

/// The window `Config::initial_screen` names; anything unrecognized starts on Search.
pub fn initial_window(name: &str) -> &'static str {
    match name {
        "queue" => "queue-tab",
        "playlists" => "playlists",
        "hist" => "history-tab",
        "now_playing" => "now-playing",
        _ => "search",
    }
}
