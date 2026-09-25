use std::path::PathBuf;
use std::sync::Arc;

use cursive::{Printer, Rect};
use cursive::theme::ColorStyle;

use core::{PaneLayoutConfig, ScanMode, Session, SourceId, TOGGLABLE_SOURCES};

use crate::screen::Kind;

use super::MedleyView;
use super::input::Editing;
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
    /// Global background-analysis on/off (pauses every scan plugin at once) — live via
    /// `ScanDriver::set_mode`, unlike `Source`.
    Scan { enabled: bool, available: bool },
    /// Decode-based waveform generation on/off — live, settings-only (no hotkey).
    /// SoundCloud's own API-provided waveforms are unaffected.
    WaveformGen(bool),
    /// One plugin from `Session::scan_plugin_toggles` on/off — live, on top of `Scan`'s global mode:
    /// turning `Scan` off still stops everything regardless of these.
    ScanPlugin { id: &'static str, name: &'static str, enabled: bool, available: bool },
    /// Debug toggle: makes the three BPM plugins re-analyze tracks they already have an attr for.
    ForceBpmReanalysis(bool),
    /// Vis frame-rate limit — live; Enter steps through `VIS_FPS_STEPS`.
    VisFps(u32),
    /// The bottom scrubber row shown or hidden — live.
    StatusLine(bool),
    ShowHints(bool),
    AutoUpdate(bool),
    /// Media cache directory — Enter edits it; applies at the next launch.
    CacheDir(PathBuf),
    /// Playlist unsourced likes go to — Enter edits it; applies at once.
    LikedPlaylist(String),
    /// Enter opens this plugin's setup dialog.
    Setup(SourceId),
}

const VIS_FPS_STEPS: [u32; 6] = [10, 15, 20, 30, 45, 60];

fn settings_entry_line(e: &SettingsEntry) -> String {
    match e {
        SettingsEntry::Info(s) => s.clone(),
        SettingsEntry::Source { name, enabled } => format!("[{}] {name}", if *enabled { "x" } else { " " }),
        SettingsEntry::Scan { enabled, available: true } => {
            format!("[{}] background analysis", if *enabled { "x" } else { " " })
        }
        SettingsEntry::WaveformGen(on) => format!("[{}] waveform scan (decode)", if *on { "x" } else { " " }),
        SettingsEntry::ScanPlugin { name, enabled, available: true, .. } => {
            format!("[{}] {name} scan", if *enabled { "x" } else { " " })
        }
        SettingsEntry::ScanPlugin { name, available: false, .. } => format!("[ ] {name} scan (unavailable)"),
        SettingsEntry::ForceBpmReanalysis(on) => format!("[{}] force BPM reanalysis", if *on { "x" } else { " " }),
        SettingsEntry::StatusLine(shown) => format!("[{}] status line", if *shown { "x" } else { " " }),
        SettingsEntry::ShowHints(shown) => format!("[{}] hints", if *shown { "x" } else { " " }),
        SettingsEntry::AutoUpdate(on) => format!("[{}] auto update", if *on { "x" } else { " " }),
        SettingsEntry::CacheDir(dir) => format!("cache.dir:        {}", core::tilde(dir)),
        SettingsEntry::LikedPlaylist(name) => format!("likes.playlist:   {name}"),
        SettingsEntry::Setup(id) => format!("    [Enter] set up {id}"),
        SettingsEntry::VisFps(fps) => format!("vis.fps:          {fps}"),
        SettingsEntry::Scan { available: false, .. } => "[ ] background analysis (unavailable)".to_string(),
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
        let id = SourceId::from(name);
        if let Some(plugin) = s.plugin(&id) {
            v.push(SettingsEntry::Setup(id));
            if let Some(detail) = plugin.detail() {
                v.push(SettingsEntry::Info(format!("    {detail}")));
            }
        }
    }
    v.push(SettingsEntry::Scan {
        enabled: s.scan.as_ref().is_some_and(|d| d.mode() != ScanMode::Disabled),
        available: s.scan.is_some(),
    });
    v.push(SettingsEntry::WaveformGen(cfg.scan.waveform.enabled));
    for &(id, name, _) in &s.scan_plugin_toggles {
        v.push(SettingsEntry::ScanPlugin {
            id,
            name,
            enabled: s.scan_plugin_enabled(id),
            available: s.scan.as_ref().is_some_and(|d| d.has_plugin(id)),
        });
    }
    v.push(SettingsEntry::ForceBpmReanalysis(s.force_bpm_reanalysis_enabled()));
    v.push(SettingsEntry::VisFps(cfg.vis.limit()));
    v.push(SettingsEntry::StatusLine(cfg.status_line));
    v.push(SettingsEntry::ShowHints(cfg.show_hints));
    v.push(SettingsEntry::AutoUpdate(cfg.auto_update));
    v.push(SettingsEntry::CacheDir(cfg.media_cache_dir.clone()));
    v.push(SettingsEntry::LikedPlaylist(cfg.liked_playlist.clone()));
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
    entries: Memo<(u64, u64, PaneLayoutConfig, u64), Arc<Vec<SettingsEntry>>>,
}

impl SettingsPane {
    /// The rows, rebuilt only when the session, the dock layout or a window's placement changed.
    pub(super) fn entries(&self, ctx: &Ctx) -> Arc<Vec<SettingsEntry>> {
        let key = (ctx.s.revision(), ctx.s.warnings_revision(), ctx.pane_cfg, ctx.placements.generation());
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
            SettingsEntry::WaveformGen(on) => {
                self.with_session_mut(|s| s.set_waveform_gen_enabled(!on));
            }
            SettingsEntry::ScanPlugin { id, enabled, available: true, .. } => {
                self.with_session_mut(|s| s.set_scan_plugin_enabled(id, !enabled));
            }
            SettingsEntry::ForceBpmReanalysis(on) => {
                self.with_session_mut(|s| s.set_force_bpm_reanalysis(!on));
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
            SettingsEntry::ShowHints(shown) => {
                self.with_session_mut(|s| s.set_show_hints(!shown));
                self.show_hints = !shown;
            }
            SettingsEntry::AutoUpdate(on) => self.with_session_mut(|s| s.set_auto_update(!on)),
            SettingsEntry::CacheDir(dir) => {
                self.editing = Editing::CacheDir;
                self.buffer = core::tilde(&dir);
            }
            SettingsEntry::LikedPlaylist(name) => {
                self.editing = Editing::LikedPlaylist;
                self.buffer = name;
            }
            SettingsEntry::Setup(id) => self.open_setup(&id),
            SettingsEntry::Scan { available: false, .. }
            | SettingsEntry::ScanPlugin { available: false, .. }
            | SettingsEntry::Info(_) => {}
        }
    }
}
