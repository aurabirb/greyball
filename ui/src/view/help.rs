use cursive::Printer;
use cursive::theme::ColorStyle;

use core::HotkeyTarget;

use crate::{command, keybindings};

use super::MedleyView;
use super::playlists::top_row_name;
use super::scroll::bound_offset;
use super::text::pad;

/// Row the help screen's content starts on.
pub(super) const HELP_LIST_TOP: usize = 1;

/// Content for the help/shortcuts screen.
pub(super) fn build_help_lines(
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
    /// The help screen's content lines.
    pub(super) fn help_lines(&self) -> Vec<String> {
        let plugin_commands = self.with_session(|s| s.plugin_command_help());
        let (playlist_hotkeys, builtin_remaps) = self.with_session(|s| {
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
            (playlist_hotkeys, builtin_remaps)
        });
        build_help_lines(&playlist_hotkeys, &builtin_remaps, &plugin_commands)
    }

    /// Fullscreen help/shortcuts modal (`?` or `:help`).
    pub(super) fn draw_help(&self, printer: &Printer) {
        let lines = self.help_lines();

        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Help / Shortcuts", p.size.x));
        });

        let bottom = printer.size.y.saturating_sub(1);
        let h = bottom.saturating_sub(HELP_LIST_TOP);
        let max_scroll = lines.len().saturating_sub(h);
        let scroll = self.help_scroll.min(max_scroll);
        for (i, line) in lines.iter().skip(scroll).take(h).enumerate() {
            printer.print((0, HELP_LIST_TOP + i), line);
        }

        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, bottom), &pad("  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll", p.size.x));
        });
    }

    /// Move `help_scroll` by `step` rows, clamped to the scrollable range.
    pub(super) fn jump_help(&mut self, up: bool, step: usize) {
        self.help_scroll =
            if up { self.help_scroll.saturating_sub(step) } else { self.help_scroll.saturating_add(step) };
        let len = self.help_lines().len();
        let h = self.last_screen_size.y.saturating_sub(1).saturating_sub(HELP_LIST_TOP);
        self.help_scroll = bound_offset(self.help_scroll, len, h);
    }

    pub(super) fn open_help(&mut self) {
        self.help_open = true;
        self.help_scroll = 0;
    }
}
