use std::thread;

use cursive::Printer;
use cursive::event::{Event, Key};
use cursive::theme::ColorStyle;

use core::{CoreEvent, PluginHealth, SetupKind, SourceId};

use super::MedleyView;
use super::input::Editing;
use super::scroll::{CursorWindow, modal_list_h};
use super::text::pad;

/// Row the warnings modal's plugin list starts on (row 0 = title, row 1 = blank spacer).
pub(super) const WARNINGS_LIST_TOP: usize = 2;

/// Cap on how many plugin messages the warnings modal's bottom section shows.
const WARNINGS_MESSAGES_MAX: usize = 5;

/// Only ever called for `count > 0` — the button isn't drawn at all when there are no warnings.
pub(super) fn warnings_label(count: usize) -> String {
    format!(" ⚠ warnings ({count}) ")
}

/// Whether a keyboard event arriving while `Focus::Warnings` is focused should knock focus off the button.
pub(super) fn defocuses_warnings(event: &Event) -> bool {
    !matches!(event, Event::Key(Key::Enter))
}

impl MedleyView {
    /// "{id}: {msg}" for every plugin currently reporting a non-`Ok` health.
    fn warnings_messages(&self) -> Vec<String> {
        self.with_session(|s| {
            s.plugin_statuses()
                .into_iter()
                .filter_map(|(id, health)| health.message().map(|m| format!("{id}: {m}")))
                .collect()
        })
    }

    /// Rows the bottom messages section reserves.
    fn warnings_messages_h(&self) -> usize {
        let n = self.warnings_messages().len();
        if n == 0 { 0 } else { 1 + n.min(WARNINGS_MESSAGES_MAX) }
    }

    /// Visible plugin rows in the warnings modal's navigable list, given the whole-screen height.
    pub(super) fn warnings_list_h(&self, screen_h: usize) -> usize {
        modal_list_h(screen_h, WARNINGS_LIST_TOP).saturating_sub(self.warnings_messages_h())
    }

    /// Fullscreen plugin-warnings modal.
    pub(super) fn draw_warnings(&self, printer: &Printer) {
        let statuses = self.with_session(|s| s.plugin_statuses());
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Plugin warnings", p.size.x));
        });

        if statuses.is_empty() {
            printer.print((0, WARNINGS_LIST_TOP), "(no plugins registered)");
        }
        let list_h = self.warnings_list_h(printer.size.y);
        for (i, (id, health)) in statuses.iter().enumerate().skip(self.warnings_offset).take(list_h) {
            let y = WARNINGS_LIST_TOP + (i - self.warnings_offset);
            let icon = match health {
                PluginHealth::Ok => "✓",
                PluginHealth::Warn(_) => "⚠",
                PluginHealth::Fail(_) => "✗",
            };
            let line = pad(&format!("{icon} {id}"), printer.size.x);
            if i == self.warnings_cursor {
                printer.with_color(ColorStyle::highlight(), |p| p.print((0, y), &line));
            } else {
                printer.print((0, y), &line);
            }
        }

        let messages = self.warnings_messages();
        if !messages.is_empty() {
            let messages_top = WARNINGS_LIST_TOP + list_h + 1;
            for (j, msg) in messages.iter().take(WARNINGS_MESSAGES_MAX).enumerate() {
                printer.print((0, messages_top + j), &pad(msg, printer.size.x));
            }
        }

        if let Editing::PluginSetup(id) = &self.editing {
            let prompt = self
                .with_session(|s| s.plugin(id))
                .map(|p| match p.setup_kind() {
                    SetupKind::TextInput { prompt } => prompt,
                    SetupKind::Action => String::new(),
                })
                .unwrap_or_default();
            let y1 = printer.size.y.saturating_sub(2);
            let y2 = printer.size.y.saturating_sub(1);
            printer.with_color(ColorStyle::highlight_inactive(), |p| {
                p.print((0, y1), &pad(&prompt, p.size.x));
            });
            printer.print((0, y2), &pad(&format!("> {}  [Esc] cancel", self.buffer), printer.size.x));
        } else {
            let bottom = printer.size.y.saturating_sub(1);
            printer.with_color(ColorStyle::highlight_inactive(), |p| {
                p.print((0, bottom), &pad("  [Enter] run setup   [Esc] close", p.size.x));
            });
        }
    }

    /// Move `warnings_cursor` by `step` rows, keeping `warnings_offset` following it via `CursorWindow`.
    pub(super) fn jump_warnings(&mut self, up: bool, step: usize) {
        let n = self.with_session(|s| s.plugin_statuses().len());
        let h = self.warnings_list_h(self.last_screen_size.y);
        CursorWindow { cursor: &mut self.warnings_cursor, offset: &mut self.warnings_offset }
            .jump(up, step, n, h);
    }

    /// Resync `warnings_offset` to `warnings_cursor` without moving the cursor.
    pub(super) fn follow_warnings_offset(&mut self) {
        let h = self.warnings_list_h(self.last_screen_size.y);
        CursorWindow { cursor: &mut self.warnings_cursor, offset: &mut self.warnings_offset }.follow(h);
    }

    pub(super) fn open_warnings(&mut self) {
        self.warnings_open = true;
        self.warnings_cursor = 0;
        self.warnings_offset = 0;
    }

    /// Number of plugins currently reporting a non-`Ok` health.
    pub(super) fn warn_count(&self) -> usize {
        self.with_session(|s| s.plugin_statuses().iter().filter(|(_, h)| !h.is_ok()).count())
    }

    /// `Enter` (or a click) on the selected warnings-modal row.
    pub(super) fn activate_selected_warning(&mut self) {
        let Some((id, _)) =
            self.with_session(|s| s.plugin_statuses().into_iter().nth(self.warnings_cursor))
        else {
            return;
        };
        let Some(plugin) = self.with_session(|s| s.plugin(&id)) else {
            return;
        };
        match plugin.setup_kind() {
            SetupKind::Action => self.run_plugin_setup(id, None),
            SetupKind::TextInput { .. } => {
                self.buffer.clear();
                self.editing = Editing::PluginSetup(id);
            }
        }
    }

    /// Run a plugin's `setup()` on a background thread.
    pub(super) fn run_plugin_setup(&self, id: SourceId, input: Option<String>) {
        let session = self.session.clone();
        let bus = self.with_session(|s| s.bus.clone());
        thread::spawn(move || {
            let Some(plugin) = session.lock().unwrap().plugin(&id) else {
                return;
            };
            let health = plugin.setup(input);
            let succeeded = health.is_ok();
            let wiring = plugin.wiring();
            let mut guard = session.lock().unwrap();
            guard.apply_wiring(&id, wiring);
            // See `Session::plugin_statuses`.
            guard.record_setup_result(id, health);
            drop(guard);
            // The UI's cue to redraw the warnings panel / pick up whatever just got registered.
            bus.send(CoreEvent::PluginStatusChanged);
            if succeeded {
                bus.send(CoreEvent::PluginLoginSucceeded);
            }
        });
    }
}
