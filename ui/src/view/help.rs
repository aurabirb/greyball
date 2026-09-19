use cursive::{Printer, Rect};
use cursive::event::{Event, Key};

use core::{HotkeyTarget, Session};

use crate::{command, keybindings};

use super::MedleyView;
use super::memo::Memo;
use super::modal::{Modal, ModalOutcome, draw_modal_frame, modal_body};
use super::playlists::top_row_name;
use super::scroll::{Nav, PAGE_SCROLL_STEP, bound_offset};

/// The help/shortcuts screen (`?`/`:help`).
pub(super) struct HelpModal {
    scroll: usize,
    lines: Vec<String>,
    /// The `list_revision` that `lines` were built under.
    built: Memo<u64>,
}

impl HelpModal {
    fn new(lines: Vec<String>, list_revision: u64) -> Self {
        let built = Memo::default();
        built.changed(list_revision);
        Self { scroll: 0, lines, built }
    }

    /// Whether `list_revision` moved since the lines were built, remembering it.
    pub(super) fn stale(&self, list_revision: u64) -> bool {
        self.built.changed(list_revision)
    }

    /// Replaces the lines in place and re-clamps `scroll`, so a rebuild never jumps to the top.
    pub(super) fn refresh(&mut self, lines: Vec<String>, rect: Rect) {
        self.lines = lines;
        self.scroll = bound_offset(self.scroll, self.lines.len(), modal_body(rect, true).height());
    }

    pub(super) fn on_event(&mut self, event: &Event, rect: Rect) -> ModalOutcome {
        if let Some(nav) = Nav::of(event) {
            let (up, step) = nav.step(PAGE_SCROLL_STEP);
            let scroll = if up { self.scroll.saturating_sub(step) } else { self.scroll.saturating_add(step) };
            self.scroll = bound_offset(scroll, self.lines.len(), modal_body(rect, true).height());
        }
        if *event == Event::Key(Key::Esc) { ModalOutcome::Close } else { ModalOutcome::Stay }
    }

    pub(super) fn draw(&self, printer: &Printer, rect: Rect) {
        let footer = "  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll";
        let body = draw_modal_frame(printer, rect, Some("Help / Shortcuts"), footer);
        let scroll = bound_offset(self.scroll, self.lines.len(), body.size.y);
        for (i, line) in self.lines.iter().skip(scroll).take(body.size.y).enumerate() {
            body.print((0, i), line);
        }
    }
}

/// Content for the help/shortcuts screen.
fn build_help_lines(
    playlist_hotkeys: &[(char, String)],
    builtin_remaps: &[(char, String)],
    plugin_commands: &[(String, String)],
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("Commands".to_string());
    lines.push(String::new());
    for (name, desc) in command::HELP {
        lines.push(format!("  :{name}"));
        lines.push(format!("      {desc}"));
    }
    for (word, desc) in plugin_commands {
        lines.push(format!("  :{word}"));
        lines.push(format!("      {desc}"));
    }
    lines.push(String::new());
    lines.push("Keyboard shortcuts (defaults — see :keys for the live, remappable list)".to_string());
    lines.push(String::new());
    for (key, desc) in keybindings::RAW_KEYS {
        lines.push(format!("  {key:<10} {desc}"));
    }
    if !builtin_remaps.is_empty() {
        lines.push(String::new());
        lines.push("Remapped built-in keys (:keys)".to_string());
        lines.push(String::new());
        for (ch, name) in builtin_remaps {
            lines.push(format!("  {ch:<10} {name}"));
        }
    }
    if !playlist_hotkeys.is_empty() {
        lines.push(String::new());
        lines.push("Playlist hotkeys".to_string());
        lines.push(String::new());
        for (ch, name) in playlist_hotkeys {
            lines.push(format!("  {ch:<10} {name}"));
        }
    }
    lines
}

impl MedleyView {
    pub(super) fn help_lines(&self, s: &Session) -> Vec<String> {
        let plugin_commands = s.plugin_command_help();
        let playlists = s.playlists();
        let rows = self.top_rows(s);
        let hotkeys = s.hotkeys();
        let playlist_hotkeys = hotkeys
            .iter()
            .filter_map(|(ch, target)| {
                rows.iter()
                    .find(|r| r.target() == *target)
                    .map(|r| (*ch, top_row_name(r, &playlists)))
            })
            .collect::<Vec<_>>();
        let builtin_remaps = hotkeys
            .into_iter()
            .filter_map(|(ch, target)| match target {
                HotkeyTarget::Builtin(action) => Some((ch, action.label().to_string())),
                _ => None,
            })
            .collect::<Vec<_>>();
        build_help_lines(&playlist_hotkeys, &builtin_remaps, &plugin_commands)
    }

    pub(super) fn open_help(&mut self) {
        let (lines, list_revision) = self.with_session(|s| (self.help_lines(s), s.list_revision()));
        self.modal = Some(Modal::Help(HelpModal::new(lines, list_revision)));
    }
}
