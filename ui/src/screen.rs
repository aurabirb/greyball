/// What a window shows; any number of windows may share one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    List(ListKind),
    Log,
    Settings,
    Vis,
    Help,
    Files,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::List(list) => list.label(),
            Kind::Log => "Log",
            Kind::Settings => "Settings",
            Kind::Vis => "Vis",
            Kind::Help => "Help",
            Kind::Files => "Files",
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
            Placement::Docked => "docked",
            Placement::Screen => "screen",
            Placement::Floating => "float",
        }
    }

    /// Shown over the view rather than in it.
    pub fn over_view(self) -> bool {
        matches!(self, Placement::Floating | Placement::Screen)
    }

    /// Esc closes it once it has nothing left to undo.
    pub fn closes_on_esc(self) -> bool {
        self != Placement::Tabbed
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

/// A bottom corner of the screen, where a surface may host a widget or an input line.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    Left,
    Right,
}

/// Which bottom corners a surface offers.
#[derive(Clone, Copy)]
pub struct Corners {
    left: bool,
    right: bool,
}

impl Corners {
    pub const NONE: Corners = Corners { left: false, right: false };
    pub const RIGHT: Corners = Corners { left: false, right: true };
    pub const BOTH: Corners = Corners { left: true, right: true };

    pub fn offers(self, corner: Corner) -> bool {
        match corner {
            Corner::Left => self.left,
            Corner::Right => self.right,
        }
    }
}

/// A startup window: the name `:panes`, `:window` and `state.toml` know it by, and its instance settings.
pub struct Startup {
    pub name: &'static str,
    pub kind: Kind,
    pub home: Home,
    /// A Playlists list puts the playlists that have a key first.
    pub keyed_first: bool,
    /// The startup tab this window is the companion of: it opens and cycles in the tab's place, and never joins the tab bar.
    pub companion_of: Option<&'static str>,
}

const fn startup(name: &'static str, kind: Kind, home: Home) -> Startup {
    Startup { name, kind, home, keyed_first: false, companion_of: None }
}

const fn companion(name: &'static str, kind: Kind, home: Home, of: &'static str) -> Startup {
    Startup { companion_of: Some(of), ..startup(name, kind, home) }
}

/// The Now Playing tab.
pub const NOW_PLAYING: &str = "now-playing";

/// The window the `OpenHelp` key and `:help` open as a tab; closed, it sits floating.
pub const HELP: &str = "help";

/// The file browser window `:open` opens; closed, it sits floating.
pub const FILES: &str = "files";

/// Every window startup builds, tabs first in tab order; each tab has a companion instance.
pub const WINDOWS: [Startup; 15] = [
    startup(NOW_PLAYING, Kind::List(ListKind::NowPlaying), Home::Tab),
    startup("playlists", Kind::List(ListKind::Playlists), Home::Tab),
    startup("search", Kind::List(ListKind::Search), Home::Tab),
    startup("history-tab", Kind::List(ListKind::History), Home::Tab),
    startup("queue-tab", Kind::List(ListKind::Queue), Home::Tab),
    startup("log", Kind::Log, Home::Pane),
    startup("settings", Kind::Settings, Home::Pane),
    startup("vis", Kind::Vis, Home::Pane),
    companion("playing", Kind::List(ListKind::NowPlaying), Home::Pane, NOW_PLAYING),
    companion("results", Kind::List(ListKind::Search), Home::Pane, "search"),
    companion("queue", Kind::List(ListKind::Queue), Home::Pane, "queue-tab"),
    companion("history", Kind::List(ListKind::History), Home::Pane, "history-tab"),
    Startup { keyed_first: true, ..companion("playlist-keys", Kind::List(ListKind::Playlists), Home::Float, "playlists") },
    startup(HELP, Kind::Help, Home::Float),
    startup(FILES, Kind::Files, Home::Float),
];
