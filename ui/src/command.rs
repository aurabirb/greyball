//! `:`-line parser. The target enum is [`core::Command`], so this only turns
//! a typed line into one. Dispatch is handled by
//! [`core::Session::dispatch`].

use std::path::PathBuf;

use core::{Axis, Command, PaneMode, Session, Side, TrackId};

use crate::screen::{Kind, Screen};

/// A `:panes` argument; `None` fields stay as they are, `pane: None` targets every pane window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanePatch {
    pub pane: Option<Kind>,
    pub mode: Option<PaneMode>,
    pub side: Option<Side>,
    pub stack: Option<Axis>,
}

/// A parsed `:`-line. Some forms still need a name → id lookup against the live
/// [`Session`] (done in [`resolve`]); the rest are ready.
#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Ready(Command),
    /// `add-to-playlist <name>` — track is the current row.
    AddToPlaylist(String),
    /// `export <playlist-name> [path]` — needs a name → id lookup.
    ExportM3u { name: String, path: Option<PathBuf> },
    /// `open <url-or-path>` — unifies the old `addfile`/`playlist`/`import`
    /// commands; classified against the live `Session` in
    /// `view::MedleyView::open_arg` (a recognized source URI opens a
    /// playlist link, a `.m3u`/`.m3u8` path imports, anything else local is
    /// added to the currently open playlist).
    Open(String),
    /// `open` with no arguments — open the modal filesystem browser.
    OpenBrowse,
    /// `help` — show the command list in a modal.
    Help,
    /// `log` / `settings` — open/close that pane.
    TogglePane(Kind),
    /// `vis` — toggle the visualizer pane open/closed.
    Vis,
    /// `panes ...` — change where open panes render.
    SetPaneLayout(PanePatch),
    /// `hist` — switch to the play-history screen.
    History,
    /// `keys` — open the hotkey menu (same modal as backtick outside the
    /// Playlists screen), to remap a built-in command's key. Playlist
    /// hotkeys are bound from the Playlists screen instead.
    Keys,
    /// `link` — pick the current row as one end of a link; run it again on a
    /// second row to merge them. Needs the selected track, resolved in
    /// `resolve`.
    Link,
    /// `unlink` — unlink the selected track from its links.
    Unlink,
    /// Any word not recognized as a built-in above — checked against the
    /// live `Session`'s plugin-registered commands in `resolve` (`parse`
    /// itself has no `Session` to consult), since a plugin's own command
    /// word (e.g. Spotify's `spotify`) isn't known here. `arg` is the rest
    /// of the line, verbatim.
    PluginCommand { word: String, arg: Option<String> },
}

/// One line of `:help` output: `"name/alias… <args>" — description`.
pub const HELP: &[(&str, &str)] = &[
    ("search / s <query>", "search all sources for tracks"),
    ("newplaylist / np <name>", "create a new playlist"),
    (
        "add-to-playlist / add / atp <playlist>",
        "add the selected track to a playlist by name",
    ),
    (
        "open [<url-or-path>]",
        "open a playlist link, import an M3U file, or add local audio files \
         to the open playlist — no args opens a file browser",
    ),
    ("export <playlist> [path]", "export a playlist to M3U"),
    ("log", "toggle the log pane"),
    ("settings", "toggle the settings pane"),
    ("hist", "switch the main screen to play history (same as pressing 4)"),
    ("vis", "toggle the real-audio bar-eq visualizer pane"),
    ("queue", "toggle the queue pane (dock it, or switch to it — see :panes)"),
    ("history", "toggle the history pane (dock it, or switch to it — see :panes)"),
    ("togglescan", "toggle the background scan (bpm, ...) between active and cache-only"),
    ("toggleshuffle", "toggle queue shuffle"),
    (
        "panes [<pane>] [screen|embedded|float] [left|right|top|bottom] [horizontal|vertical]",
        "change where a pane renders (float: a box over the current view) — \
         omit <pane> to place every pane",
    ),
    (
        "keys",
        "open the hotkeys menu (remap a built-in command's key — playlist hotkeys are set from the Playlists screen)",
    ),
    (
        "link",
        "pick the selected row, then run :link again on a second row to merge them as one track",
    ),
    ("unlink", "unlink the selected track from its links"),
    ("help / h / ?", "show this list"),
    ("quit / q", "exit medley"),
];

/// Short-name alias map: short name → canonical command word.
pub fn dealias(word: &str) -> &str {
    match word {
        "np" | "newpl" => "newplaylist",
        "add" | "atp" => "add-to-playlist",
        "s" | "find" => "search",
        "q" | "exit" => "quit",
        "export" => "exportm3u",
        "h" | "?" => "help",
        other => other,
    }
}

/// Parse a `:`-line (leading `:` already stripped). Pure — no `Session`.
pub fn parse(line: &str) -> Result<Parsed, String> {
    let line = line.trim();
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (line, ""),
    };
    match dealias(word) {
        "quit" => Ok(Parsed::Ready(Command::Quit)),
        "help" => Ok(Parsed::Help),
        "search" => Ok(Parsed::Ready(Command::Search(rest.to_string()))),
        "newplaylist" if !rest.is_empty() => {
            Ok(Parsed::Ready(Command::NewPlaylist(rest.to_string())))
        }
        "newplaylist" => Err("usage: newplaylist <name>".into()),
        "add-to-playlist" if !rest.is_empty() => Ok(Parsed::AddToPlaylist(rest.to_string())),
        "add-to-playlist" => Err("usage: add-to-playlist <name>".into()),
        "open" => {
            if rest.is_empty() {
                Ok(Parsed::OpenBrowse)
            } else {
                Ok(Parsed::Open(rest.to_string()))
            }
        }
        "exportm3u" if !rest.is_empty() => {
            // "<name>" or "<name> <path>": a trailing token that contains '/' or
            // ends in .m3u/.m3u8 is taken as the path, the rest is the name.
            if let Some((name, tail)) = rest.rsplit_once(char::is_whitespace) {
                let tail = tail.trim();
                if tail.contains('/') || tail.ends_with(".m3u") || tail.ends_with(".m3u8") {
                    return Ok(Parsed::ExportM3u {
                        name: name.trim().to_string(),
                        path: Some(PathBuf::from(tail)),
                    });
                }
            }
            Ok(Parsed::ExportM3u {
                name: rest.to_string(),
                path: None,
            })
        }
        "exportm3u" => Err("usage: export <playlist> [path]".into()),
        "log" => Ok(Parsed::TogglePane(Kind::Log)),
        "settings" => Ok(Parsed::TogglePane(Kind::Settings)),
        "vis" if rest.is_empty() => Ok(Parsed::Vis),
        "vis" => Err("usage: vis".into()),
        "queue" => Ok(Parsed::TogglePane(Kind::List(Screen::Queue))),
        // Unlike "hist", this toggles the History pane window per its own placement.
        "history" => Ok(Parsed::TogglePane(Kind::List(Screen::History))),
        "togglescan" => Ok(Parsed::Ready(Command::ToggleScan)),
        "toggleshuffle" => Ok(Parsed::Ready(Command::ToggleShuffle)),
        "hist" => Ok(Parsed::History),
        "keys" => Ok(Parsed::Keys),
        "link" if rest.is_empty() => Ok(Parsed::Link),
        "link" => Err("usage: link".into()),
        "unlink" if rest.is_empty() => Ok(Parsed::Unlink),
        "unlink" => Err("usage: unlink".into()),
        "panes" => parse_pane_patch(rest).map(Parsed::SetPaneLayout),
        "" => Err("empty command".into()),
        other => Ok(Parsed::PluginCommand {
            word: other.to_string(),
            arg: (!rest.is_empty()).then(|| rest.to_string()),
        }),
    }
}

/// Split a `:`-line tail into whitespace-separated paths, honouring `"…"` and
/// `'…'` quoting so filenames may contain spaces.
pub(crate) fn split_paths(rest: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for c in rest.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    out.push(PathBuf::from(std::mem::take(&mut cur)));
                    has_token = false;
                }
            }
            None => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(PathBuf::from(cur));
    }
    out
}

fn parse_pane_patch(rest: &str) -> Result<PanePatch, String> {
    let mut patch = PanePatch::default();
    let mut toks = rest.split_whitespace().peekable();
    if let Some(&first) = toks.peek() {
        let pane = match first.to_ascii_lowercase().as_str() {
            "log" => Some(Kind::Log),
            "settings" => Some(Kind::Settings),
            "vis" => Some(Kind::Vis),
            "queue" => Some(Kind::List(Screen::Queue)),
            "history" => Some(Kind::List(Screen::History)),
            _ => None,
        };
        if pane.is_some() {
            patch.pane = pane;
            toks.next();
        }
    }
    for tok in toks {
        match tok.to_ascii_lowercase().as_str() {
            "screen" => patch.mode = Some(PaneMode::Screen),
            "embedded" => patch.mode = Some(PaneMode::Embedded),
            "float" => patch.mode = Some(PaneMode::Float),
            "left" => patch.side = Some(Side::Left),
            "right" => patch.side = Some(Side::Right),
            "top" => patch.side = Some(Side::Top),
            "bottom" => patch.side = Some(Side::Bottom),
            "horizontal" => patch.stack = Some(Axis::Horizontal),
            "vertical" => patch.stack = Some(Axis::Vertical),
            other => {
                return Err(format!(
                    "usage: panes [<pane>] [screen|embedded|float] [left|right|top|bottom] [horizontal|vertical] (unknown {other:?})"
                ));
            }
        }
    }
    Ok(patch)
}

/// Resolve a [`Parsed`] against the session and current row into a
/// [`core::Command`].
pub fn resolve(parsed: Parsed, session: &Session, selected: Option<TrackId>) -> Result<Command, String> {
    match parsed {
        Parsed::Ready(c) => Ok(c),
        // Both forms are classified against the live `Session`/filesystem in
        // `view::MedleyView::open_arg`, which intercepts them before `resolve`.
        Parsed::OpenBrowse => Err("open a playlist first".into()),
        Parsed::Open(_) => Err("open is handled by the UI".into()),
        // `view::commit_edit` intercepts this before `resolve`.
        Parsed::Help => Err("help is handled by the UI".into()),
        // Both handled in `view::commit_edit` — pane visibility/layout is
        // UI-local, not a `core::Command`.
        Parsed::TogglePane(_) => Err("pane toggling is handled by the UI".into()),
        Parsed::History => Err("screen switching is handled by the UI".into()),
        Parsed::Keys => Err("the hotkey menu is handled by the UI".into()),
        Parsed::Vis => Err("pane toggling is handled by the UI".into()),
        Parsed::SetPaneLayout(_) => Err("pane layout is handled by the UI".into()),
        Parsed::ExportM3u { name, path } => {
            let playlist = session
                .playlists()
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| format!("no playlist named {name:?}"))?;
            Ok(match path {
                Some(path) => Command::ExportM3uTo {
                    playlist: playlist.id,
                    path,
                },
                None => Command::ExportM3u(playlist.id),
            })
        }
        Parsed::Link => {
            let track = selected.ok_or("no track selected")?;
            Ok(Command::LinkPick(track))
        }
        Parsed::Unlink => {
            let track = selected.ok_or("no track selected")?;
            Ok(Command::Unlink(track))
        }
        // Validated against the live `Session` here (an unregistered word is
        // a real "unknown command"), but actually run by the UI
        // (`view::run_plugin_command`) off this thread — like `setup`, a
        // plugin command can block (e.g. an OAuth browser flow).
        Parsed::PluginCommand { word, .. } if session.plugin_for_command(&word).is_none() => {
            Err(format!("unknown command: {word}"))
        }
        Parsed::PluginCommand { .. } => Err("plugin commands are handled by the UI".into()),
        Parsed::AddToPlaylist(name) => {
            let playlist = session
                .playlists()
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| format!("no playlist named {name:?}"))?;
            let track = selected.ok_or("no track selected")?;
            Ok(Command::AddToPlaylist {
                track,
                playlist: playlist.id,
            })
        }
    }
}

