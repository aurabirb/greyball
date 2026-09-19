use cursive::{Printer, Vec2};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::HotkeyTarget;

use crate::{command, keybindings};

use super::MedleyView;
use super::playlists::top_row_name;
use super::scroll::{Nav, PAGE_SCROLL_STEP, bound_offset, stale};
use super::text::pad;

/// Row the help screen's content starts on (row 0 = title).
const LIST_TOP: usize = 1;

/// The help/shortcuts screen (`?`/`:help`); rebuilt on a `list_revision` drift — see `refresh_help`.
pub(super) struct HelpModal {
    scroll: usize,
    lines: Vec<String>,
    list_revision: u64,
}

impl HelpModal {
    pub(super) fn new(lines: Vec<String>, list_revision: u64) -> Self {
        Self { scroll: 0, lines, list_revision }
    }

    pub(super) fn list_revision(&self) -> u64 {
        self.list_revision
    }

    /// Replaces the lines in place and re-clamps `scroll`, so a rebuild never jumps to the top.
    fn refresh(&mut self, lines: Vec<String>, list_revision: u64, size: Vec2) {
        self.lines = lines;
        self.list_revision = list_revision;
        self.scroll = bound_offset(self.scroll, self.lines.len(), Self::view_h(size));
    }

    /// Content rows between the title and the footer.
    fn view_h(size: Vec2) -> usize {
        size.y.saturating_sub(1).saturating_sub(LIST_TOP)
    }

    /// Scrolls on nav keys/wheel; `true` when `event` closes the screen.
    fn on_event(&mut self, event: &Event, size: Vec2) -> bool {
        if let Some(nav) = Nav::of(event) {
            let (up, step) = nav.step(PAGE_SCROLL_STEP);
            let scroll = if up { self.scroll.saturating_sub(step) } else { self.scroll.saturating_add(step) };
            self.scroll = bound_offset(scroll, self.lines.len(), Self::view_h(size));
        }
        *event == Event::Key(Key::Esc)
    }

    fn draw(&self, printer: &Printer) {
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Help / Shortcuts", p.size.x));
        });

        let h = Self::view_h(printer.size);
        let scroll = bound_offset(self.scroll, self.lines.len(), h);
        for (i, line) in self.lines.iter().skip(scroll).take(h).enumerate() {
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
    /// The help screen's content lines plus the `list_revision` they were built under.
    fn help_lines(&self) -> (Vec<String>, u64) {
        self.with_session(|s| {
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
            let lines = build_help_lines(&playlist_hotkeys, &builtin_remaps, &plugin_commands);
            (lines, s.list_revision())
        })
    }

    /// Opens the Help modal, built for the current `list_revision`.
    pub(super) fn open_help(&mut self) {
        let (lines, list_revision) = self.help_lines();
        self.help = Some(HelpModal::new(lines, list_revision));
    }

    /// Rebuilds Help's content once `list_revision` drifts — called from `required_size`, never per-draw.
    pub(super) fn refresh_help(&mut self, list_revision: u64, size: Vec2) {
        if !self.help.as_ref().is_some_and(|h| stale(h.list_revision(), list_revision)) {
            return;
        }
        let (lines, list_revision) = self.help_lines();
        if let Some(help) = &mut self.help {
            help.refresh(lines, list_revision, size);
        }
    }

    pub(super) fn draw_help(&self, help: &HelpModal, printer: &Printer) {
        help.draw(printer);
    }

    pub(super) fn on_help_event(&mut self, event: &Event) -> EventResult {
        let size = self.last_screen_size;
        if self.help.as_mut().is_some_and(|help| help.on_event(event, size)) {
            self.help = None;
            self.focus = self.fallback_focus();
        }
        EventResult::consumed()
    }
}
