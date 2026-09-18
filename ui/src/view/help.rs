use cursive::{Printer, Vec2};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::HotkeyTarget;

use crate::{command, keybindings};

use super::MedleyView;
use super::playlists::top_row_name;
use super::scroll::{Nav, PAGE_SCROLL_STEP, bound_offset};
use super::text::pad;

/// Row the help screen's content starts on (row 0 = title).
const LIST_TOP: usize = 1;

/// The help/shortcuts screen (`?` or `:help`), a scrollable text page; exists only while open.
#[derive(Default)]
pub(super) struct HelpModal {
    scroll: usize,
}

impl HelpModal {
    /// Content rows between the title and the footer.
    fn view_h(size: Vec2) -> usize {
        size.y.saturating_sub(1).saturating_sub(LIST_TOP)
    }

    /// Scrolls on nav keys/wheel; `true` when `event` closes the screen.
    fn on_event(&mut self, event: &Event, len: usize, size: Vec2) -> bool {
        if let Some(nav) = Nav::of(event) {
            let (up, step) = nav.step(PAGE_SCROLL_STEP);
            let scroll = if up { self.scroll.saturating_sub(step) } else { self.scroll.saturating_add(step) };
            self.scroll = bound_offset(scroll, len, Self::view_h(size));
        }
        *event == Event::Key(Key::Esc)
    }

    fn draw(&self, printer: &Printer, lines: &[String]) {
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Help / Shortcuts", p.size.x));
        });

        let h = Self::view_h(printer.size);
        let scroll = bound_offset(self.scroll, lines.len(), h);
        for (i, line) in lines.iter().skip(scroll).take(h).enumerate() {
            printer.print((0, LIST_TOP + i), line);
        }

        printer.with_color(ColorStyle::highlight_inactive(), |p| {
            p.print((0, p.size.y.saturating_sub(1)), &pad("  [Esc] close   [↑/↓ j/k PgUp/PgDn J/K] scroll", p.size.x));
        });
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
    /// The help screen's content lines.
    fn help_lines(&self) -> Vec<String> {
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

    pub(super) fn draw_help(&self, help: &HelpModal, printer: &Printer) {
        help.draw(printer, &self.help_lines());
    }

    pub(super) fn on_help_event(&mut self, event: &Event) -> EventResult {
        let len = self.help_lines().len();
        let size = self.last_screen_size;
        if self.help.as_mut().is_some_and(|help| help.on_event(event, len, size)) {
            self.help = None;
            self.focus = self.fallback_focus();
        }
        EventResult::consumed()
    }
}
