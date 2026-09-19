//! `:`-line parser. The target enum is [`core::Command`], so this only turns
//! a typed line into one. Dispatch is handled by
//! [`core::Session::dispatch`].

use std::path::PathBuf;

use core::{Axis, Command, Session, Side, TrackId};

use crate::items;
use crate::screen::{Placement, WINDOWS};

/// A `:panes` argument; `None` fields stay as they are, `window: None` targets every window that is not a tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanePatch {
    pub window: Option<&'static str>,
    pub mode: Option<Placement>,
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
    /// `window <name>` and `log`/`settings`/`vis`/`queue`/`history` — open or close that window, or switch to its tab.
    ToggleWindow(&'static str),
    /// `panes ...` — change where open panes render.
    SetPaneLayout(PanePatch),
    /// `hist` — show the History tab window.
    History,
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

/// Parse a `:`-line (leading `:` already stripped). Pure — no `Session`.
pub fn parse(line: &str) -> Result<Parsed, String> {
    let line = line.trim();
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (line, ""),
    };
    if word.is_empty() {
        return Err("empty command".into());
    }
    // A word the table doesn't know may be a plugin's, which only the live session can tell.
    let Some(item) = items::named(word) else {
        return Ok(Parsed::PluginCommand { word: word.to_string(), arg: (!rest.is_empty()).then(|| rest.to_string()) });
    };
    // `<arg>` is required, `[arg]` optional, and a command without any takes none.
    let (required, none) = (item.args.starts_with('<'), item.args.is_empty());
    if (required && rest.is_empty()) || (none && !rest.is_empty()) {
        return Err(item.usage());
    }
    match item.names[0] {
        "quit" => Ok(Parsed::Ready(Command::Quit)),
        "help" => Ok(Parsed::Help),
        "search" => Ok(Parsed::Ready(Command::Search(rest.to_string()))),
        "newplaylist" => Ok(Parsed::Ready(Command::NewPlaylist(rest.to_string()))),
        "add-to-playlist" => Ok(Parsed::AddToPlaylist(rest.to_string())),
        "open" if rest.is_empty() => Ok(Parsed::OpenBrowse),
        "open" => Ok(Parsed::Open(rest.to_string())),
        "export" => {
            // A trailing token with a '/' or an .m3u/.m3u8 ending is the path, the rest the name.
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
        name @ ("log" | "settings" | "vis" | "queue" | "history") => window_name(name).map(Parsed::ToggleWindow),
        "window" => window_name(rest).map(Parsed::ToggleWindow),
        "togglescan" => Ok(Parsed::Ready(Command::ToggleScan)),
        "toggleshuffle" => Ok(Parsed::Ready(Command::ToggleShuffle)),
        "hist" => Ok(Parsed::History),
        "link" => Ok(Parsed::Link),
        "unlink" => Ok(Parsed::Unlink),
        "panes" => parse_pane_patch(rest).map(Parsed::SetPaneLayout).map_err(|unknown| format!("{} {unknown}", item.usage())),
        other => Err(format!("unknown command: {other}")),
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

/// The startup window `name` names.
fn window_name(name: &str) -> Result<&'static str, String> {
    let known = WINDOWS.iter().map(|window| window.name);
    known.clone().find(|&known| known == name).ok_or_else(|| {
        format!("no window named {name:?}; the windows are {}", known.collect::<Vec<_>>().join(", "))
    })
}

fn parse_pane_patch(rest: &str) -> Result<PanePatch, String> {
    let mut patch = PanePatch::default();
    for (i, tok) in rest.split_whitespace().enumerate() {
        let tok = tok.to_ascii_lowercase();
        match tok.as_str() {
            "left" => patch.side = Some(Side::Left),
            "right" => patch.side = Some(Side::Right),
            "top" => patch.side = Some(Side::Top),
            "bottom" => patch.side = Some(Side::Bottom),
            "horizontal" => patch.stack = Some(Axis::Horizontal),
            "vertical" => patch.stack = Some(Axis::Vertical),
            word => match (Placement::from_word(word), window_name(word)) {
                (Some(placement), _) => patch.mode = Some(placement),
                (None, Ok(name)) if i == 0 => patch.window = Some(name),
                _ => return Err(format!("(unknown {word:?})")),
            },
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
        Parsed::ToggleWindow(_) => Err("window toggling is handled by the UI".into()),
        Parsed::History => Err("screen switching is handled by the UI".into()),
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

