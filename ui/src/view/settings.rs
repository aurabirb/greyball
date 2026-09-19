use cursive::{Printer, Rect};
use cursive::theme::ColorStyle;

use core::{PaneLayoutConfig, ScanMode, Session, TOGGLABLE_SOURCES};

use crate::command::Pane;

use super::MedleyView;
use super::panes::pane_title;
use super::scroll::ListState;
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

/// The Settings pane: `settings_entries` as a cursor list under a title row.
#[derive(Default)]
pub(super) struct SettingsPane {
    list: ListState,
}

impl SettingsPane {
    pub(super) fn draw(&self, printer: &Printer, entries: &[SettingsEntry], focused: bool) {
        let mut title = pane_title(Pane::Settings).to_string();
        if focused {
            title = format!("[{title}]");
        }
        printer.with_color(ColorStyle::title_secondary(), |p| {
            p.print((0, 0), &pad(&title, p.size.x));
        });
        let lines: Vec<String> = entries.iter().map(settings_entry_line).collect();
        let body = Rect::from_size((0, 1), (printer.size.x, printer.size.y.saturating_sub(1)));
        self.list.draw(&printer.windowed(body), &lines);
    }
}

impl MedleyView {
    /// Settings pane's row cursor.
    pub(super) fn jump_settings(&mut self, up: bool, step: usize) {
        let pane_cfg = self.panes.cfg;
        let n = self.with_session(|s| settings_entries(s, pane_cfg).len());
        let h = self.panes.content_dims(Pane::Settings, self.is_fullscreen(Pane::Settings), self.last_screen_size).map_or(0, |(_, h)| h);
        self.settings.list.jump(up, step, n, h);
    }

    /// Enter/Space on the Settings pane's selected row.
    pub(super) fn toggle_selected_setting(&mut self) {
        let cursor = self.settings.list.cursor;
        let pane_cfg = self.panes.cfg;
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
