
use crate::command::Pane;

/// A main-content screen, declared in tab-bar and number-key order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Screen {
    /// The track list `Command::PlayContext` last started playing from; independent of the Playlists screen's state.
    NowPlaying,
    Playlists,
    Search,
    History,
    Queue,
}

impl Screen {
    pub const ALL: [Screen; 5] =
        [Screen::NowPlaying, Screen::Playlists, Screen::Search, Screen::History, Screen::Queue];

    /// The bare tab name, shared by the tab strip and the docked Queue/History pane title.
    pub fn label(self) -> &'static str {
        match self {
            Screen::NowPlaying => "Now Playing",
            Screen::Playlists => "Playlists",
            Screen::Search => "Search",
            Screen::History => "History",
            Screen::Queue => "Queue",
        }
    }

    /// The number key that switches to this screen, also its tab number.
    pub fn digit(self) -> usize {
        self as usize + 1
    }

    pub fn from_digit(key: &str) -> Option<Screen> {
        let n: usize = key.parse().ok()?;
        Screen::ALL.into_iter().find(|s| s.digit() == n)
    }

    /// `Config::initial_screen`'s value; anything unrecognized starts on Search.
    pub fn from_config(name: &str) -> Screen {
        match name {
            "queue" => Screen::Queue,
            "playlists" => Screen::Playlists,
            "hist" => Screen::History,
            "now_playing" => Screen::NowPlaying,
            _ => Screen::Search,
        }
    }

    /// The screen whose list a docked pane shows, `None` for a non-list pane.
    pub fn from_pane(pane: Pane) -> Option<Screen> {
        match pane {
            Pane::Queue => Some(Screen::Queue),
            Pane::History => Some(Screen::History),
            Pane::Log | Pane::Settings | Pane::Vis => None,
        }
    }
}

