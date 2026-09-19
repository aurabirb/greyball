use std::sync::Arc;

use cursive::{Printer, Rect};
use cursive::theme::ColorStyle;

use core::{PaneLayoutConfig, ScanMode, Session, TOGGLABLE_SOURCES};

use crate::screen::Kind;

use super::MedleyView;
use super::memo::Memo;
use super::scroll::ListState;
use super::text::pad;
use super::window::{Ctx, Placements};

/// One row of the Settings pane: plain info text, or a togglable bool.
#[derive(Clone)]
pub(super) enum SettingsEntry {
    Info(String),
    /// One of `TOGGLABLE_SOURCES` — config-only, takes effect next restart.
    Source { name: &'static str, enabled: bool },
    /// Background scan on/off — live via `ScanDriver::set_mode`, unlike `Source`.
    Scan { enabled: bool, available: bool },
    /// Vis frame-rate limit — live; Enter steps through `VIS_FPS_STEPS`.
    VisFps(u32),
    /// The bottom scrubber row shown or hidden — live.
    StatusLine(bool),
}

const VIS_FPS_STEPS: [u32; 6] = [10, 15, 20, 30, 45, 60];

fn settings_entry_line(e: &SettingsEntry) -> String {
    match e {
        SettingsEntry::Info(s) => s.clone(),
        SettingsEntry::Source { name, enabled } => format!("[{}] {name}", if *enabled { "x" } else { " " }),
        SettingsEntry::Scan { enabled, available: true } => {
            format!("[{}] bpm scan", if *enabled { "x" } else { " " })
        }
        SettingsEntry::StatusLine(shown) => format!("[{}] status line", if *shown { "x" } else { " " }),
        SettingsEntry::VisFps(fps) => format!("vis.fps:          {fps}"),
        SettingsEntry::Scan { available: false, .. } => "[ ] bpm scan (unavailable)".to_string(),
    }
}

/// Effective config as togglable/info rows.
fn settings_entries(s: &Session, pane_cfg: PaneLayoutConfig, placements: &Placements) -> Vec<SettingsEntry> {
    let cfg = &s.cfg;
    let mut v = vec![
        SettingsEntry::Info(format!("theme:            {}", cfg.theme)),
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
    v.push(SettingsEntry::VisFps(cfg.vis.limit()));
    v.push(SettingsEntry::StatusLine(cfg.status_line));
    v.push(SettingsEntry::Info(String::new()));
    v.push(SettingsEntry::Info(format!("panes.side:       {:?}", pane_cfg.side)));
    v.push(SettingsEntry::Info(format!("panes.stack:      {:?}", pane_cfg.stack)));
    v.push(SettingsEntry::Info(String::new()));
    for (name, placement) in placements.named() {
        v.push(SettingsEntry::Info(format!("{:<18}{}", format!("{name}:"), placement.word())));
    }
    v
}

/// The Settings pane: `settings_entries` as a cursor list under a title row.
#[derive(Default)]
pub(super) struct SettingsPane {
    list: ListState,
    entries: Memo<(u64, PaneLayoutConfig, u64), Arc<Vec<SettingsEntry>>>,
}

impl SettingsPane {
    /// The rows, rebuilt only when the session, the dock layout or a window's placement changed.
    pub(super) fn entries(&self, ctx: &Ctx) -> Arc<Vec<SettingsEntry>> {
        let key = (ctx.s.revision(), ctx.pane_cfg, ctx.placements.generation());
        self.entries.get_or_build(key, || Arc::new(settings_entries(ctx.s, ctx.pane_cfg, ctx.placements)))
    }

    pub(super) fn cursor(&self) -> usize {
        self.list.cursor
    }

    pub(super) fn jump(&mut self, up: bool, step: usize, len: usize, view_h: usize) {
        self.list.jump(up, step, len, view_h);
    }

    pub(super) fn draw(&self, printer: &Printer, entries: &[SettingsEntry], focused: bool) {
        let mut title = Kind::Settings.label().to_string();
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
    /// Enter/Space on row `cursor` of the Settings window.
    pub(super) fn toggle_setting(&mut self, cursor: usize) {
        let entries = self.with_session(|s| settings_entries(s, self.pane_cfg, self.windows.placements()));
        let Some(entry) = entries.into_iter().nth(cursor) else {
            return;
        };
        match entry {
            SettingsEntry::Source { name, enabled } => {
                self.with_session_mut(|s| s.set_source_enabled(name, !enabled));
                self.set_flash(format!(
                    "{name}: {} (restart to apply)",
                    if enabled { "disabled" } else { "enabled" }
                ));
            }
            SettingsEntry::Scan { enabled, available: true } => {
                self.with_session_mut(|s| s.set_scan_enabled(!enabled));
            }
            SettingsEntry::VisFps(fps) => {
                let next = VIS_FPS_STEPS.into_iter().find(|&step| step > fps).unwrap_or(VIS_FPS_STEPS[0]);
                self.with_session_mut(|s| s.set_vis_fps(next));
                self.vis.set_fps(next);
            }
            SettingsEntry::StatusLine(shown) => {
                self.with_session_mut(|s| s.set_status_line(!shown));
                self.status_line = !shown;
            }
            SettingsEntry::Scan { available: false, .. } | SettingsEntry::Info(_) => {}
        }
    }
}
