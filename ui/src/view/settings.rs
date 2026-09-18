use cursive::Printer;
use cursive::theme::ColorStyle;

use core::{PaneLayoutConfig, ScanMode, Session, TOGGLABLE_SOURCES};

use crate::command::Pane;

use super::MedleyView;
use super::panes::pane_title;
use super::text::pad;

/// One row of the Settings pane: plain info text, or a togglable bool.
#[derive(Clone)]
pub(super) enum SettingsEntry {
    Info(String),
    /// One of `TOGGLABLE_SOURCES` — config-only, takes effect next restart.
    Source { name: &'static str, enabled: bool },
    /// Background scan on/off — live via `ScanDriver::set_mode`, unlike `Source`.
    Scan { enabled: bool, available: bool },
}

fn settings_entry_line(e: &SettingsEntry) -> String {
    match e {
        SettingsEntry::Info(s) => s.clone(),
        SettingsEntry::Source { name, enabled } => format!("[{}] {name}", if *enabled { "x" } else { " " }),
        SettingsEntry::Scan { enabled, available: true } => {
            format!("[{}] bpm scan", if *enabled { "x" } else { " " })
        }
        SettingsEntry::Scan { available: false, .. } => "[ ] bpm scan (unavailable)".to_string(),
    }
}

/// Effective config as togglable/info rows, for both the embedded pane and the screen-mode modal.
pub(super) fn settings_entries(s: &Session, pane_cfg: PaneLayoutConfig) -> Vec<SettingsEntry> {
    let cfg = &s.cfg;
    let mut v = vec![
        SettingsEntry::Info(format!("theme:            {}", cfg.theme)),
        SettingsEntry::Info(format!("initial_screen:   {}", cfg.initial_screen)),
        SettingsEntry::Info(format!("volume:           {:.0}%", s.player_status().volume * 100.0)),
        SettingsEntry::Info(format!("http.roots:       {}", cfg.http.roots.len())),
        SettingsEntry::Info(format!("http.recurse:     {}", cfg.http.recurse_depth)),
    ];
    for name in TOGGLABLE_SOURCES {
        v.push(SettingsEntry::Source { name, enabled: cfg.source_enabled(name).unwrap_or(false) });
    }
    v.push(SettingsEntry::Scan {
        enabled: s.scan.as_ref().is_some_and(|d| d.mode() != ScanMode::Disabled),
        available: s.scan.is_some(),
    });
    v.push(SettingsEntry::Info(String::new()));
    v.push(SettingsEntry::Info(format!("panes.mode:       {:?}", pane_cfg.mode)));
    v.push(SettingsEntry::Info(format!("panes.side:       {:?}", pane_cfg.side)));
    v.push(SettingsEntry::Info(format!("panes.stack:      {:?}", pane_cfg.stack)));
    v
}

/// Settings pane's title + rows, with a highlight on `cursor`.
pub(super) fn draw_settings_pane(printer: &Printer, entries: &[SettingsEntry], offset: usize, cursor: usize, focused: bool) {
    let mut title = pane_title(Pane::Settings).to_string();
    if focused {
        title = format!("[{title}]");
    }
    printer.with_color(ColorStyle::title_secondary(), |p| {
        p.print((0, 0), &pad(&title, p.size.x));
    });
    let width = printer.size.x;
    let h = printer.size.y.saturating_sub(1);
    for (i, entry) in entries.iter().enumerate().skip(offset).take(h) {
        let y = 1 + (i - offset);
        let line = pad(&settings_entry_line(entry), width);
        if i == cursor {
            printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
        } else {
            printer.print((0, y), &line);
        }
    }
}

impl MedleyView {
    /// Settings pane's row cursor.
    pub(super) fn jump_settings(&mut self, up: bool, step: usize) {
        let pane_cfg = self.pane_cfg;
        let n = self.with_session(|s| settings_entries(s, pane_cfg).len());
        let h = self.pane_content_dims(Pane::Settings).map_or(0, |(_, h)| h);
        self.settings.jump(up, step, n, h);
    }

    /// Enter/Space on the Settings pane's selected row.
    pub(super) fn toggle_selected_setting(&mut self) {
        let cursor = self.settings.cursor;
        let pane_cfg = self.pane_cfg;
        let Some(entry) = self.with_session(|s| settings_entries(s, pane_cfg).into_iter().nth(cursor)) else {
            return;
        };
        match entry {
            SettingsEntry::Source { name, enabled } => {
                self.with_session_mut(|s| s.set_source_enabled(name, !enabled));
                self.queue_feedback = Some(format!(
                    "  {name}: {} (restart to apply)",
                    if enabled { "disabled" } else { "enabled" }
                ));
            }
            SettingsEntry::Scan { enabled, available: true } => {
                self.with_session_mut(|s| s.set_scan_enabled(!enabled));
            }
            SettingsEntry::Scan { available: false, .. } | SettingsEntry::Info(_) => {}
        }
    }
}
