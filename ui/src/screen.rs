/// What a window shows; any number of windows may share one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    List(ListKind),
    Log,
    Settings,
    Vis,
    Help,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::List(list) => list.label(),
            Kind::Log => "Log",
            Kind::Settings => "Settings",
            Kind::Vis => "Vis",
            Kind::Help => "Help",
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

/// Where a startup window is placed before any layout is restored.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Home {
    Tab,
    /// Placed by `Config::panes`' mode.
    Pane,
    Float,
}

/// A startup window: the name `:panes`, `:window` and `state.toml` know it by, and its instance settings.
pub struct Startup {
    pub name: &'static str,
    pub kind: Kind,
    pub home: Home,
    /// A Playlists list puts the playlists that have a key first.
    pub keyed_first: bool,
    /// The window reserves its last row for its own hints and the last bind result.
    pub status_row: bool,
    /// The startup tab this window is the companion of: it opens and cycles in the tab's place, and never joins the tab bar.
    pub companion_of: Option<&'static str>,
}

const fn startup(name: &'static str, kind: Kind, home: Home) -> Startup {
    Startup { name, kind, home, keyed_first: false, status_row: false, companion_of: None }
}

const fn companion(name: &'static str, kind: Kind, home: Home, of: &'static str) -> Startup {
    Startup { companion_of: Some(of), ..startup(name, kind, home) }
}

/// The window the `OpenHelp` key and `:help` open over whatever is shown.
pub const HELP: &str = "help";

/// Every window startup builds, tabs first in tab order; each tab has a companion instance.
pub const WINDOWS: [Startup; 14] = [
    startup("now-playing", Kind::List(ListKind::NowPlaying), Home::Tab),
    startup("playlists", Kind::List(ListKind::Playlists), Home::Tab),
    startup("search", Kind::List(ListKind::Search), Home::Tab),
    startup("history-tab", Kind::List(ListKind::History), Home::Tab),
    startup("queue-tab", Kind::List(ListKind::Queue), Home::Tab),
    startup("log", Kind::Log, Home::Pane),
    startup("settings", Kind::Settings, Home::Pane),
    startup("vis", Kind::Vis, Home::Pane),
    companion("playing", Kind::List(ListKind::NowPlaying), Home::Pane, "now-playing"),
    companion("results", Kind::List(ListKind::Search), Home::Pane, "search"),
    companion("queue", Kind::List(ListKind::Queue), Home::Pane, "queue-tab"),
    companion("history", Kind::List(ListKind::History), Home::Pane, "history-tab"),
    Startup { keyed_first: true, status_row: true, ..companion("playlist-keys", Kind::List(ListKind::Playlists), Home::Float, "playlists") },
    Startup { status_row: true, ..startup(HELP, Kind::Help, Home::Float) },
];
