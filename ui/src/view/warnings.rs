use std::thread;

use cursive::{Printer, Rect, Vec2};
use cursive::event::{Event, EventResult, Key};
use cursive::theme::ColorStyle;

use core::{CoreEvent, PluginHealth, SetupKind, SourceId};

use super::MedleyView;
use super::input::Editing;
use super::scroll::{ListEvent, ListState, modal_list_rect};
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

/// The plugin-warnings modal: a navigable plugin list above their messages; exists only while open.
#[derive(Default)]
pub(super) struct WarningsModal {
    list: ListState,
}

impl WarningsModal {
    /// "{id}: {msg}" for every plugin reporting a non-`Ok` health.
    fn messages(statuses: &[(SourceId, PluginHealth)]) -> Vec<String> {
        statuses.iter().filter_map(|(id, health)| health.message().map(|m| format!("{id}: {m}"))).collect()
    }

    /// The navigable list's area — what the bottom messages section leaves of the modal's list rows.
    fn list_rect(size: Vec2, statuses: &[(SourceId, PluginHealth)]) -> Rect {
        let n = Self::messages(statuses).len();
        let messages_h = if n == 0 { 0 } else { 1 + n.min(WARNINGS_MESSAGES_MAX) };
        let rows = modal_list_rect(size, WARNINGS_LIST_TOP);
        Rect::from_size(rows.top_left(), (size.x, rows.height().saturating_sub(messages_h)))
    }

    pub(super) fn selected(&self) -> usize {
        self.list.cursor
    }

    pub(super) fn relayout(&mut self, resized: bool, size: Vec2, statuses: &[(SourceId, PluginHealth)]) {
        self.list.relayout(resized, statuses.len(), Self::list_rect(size, statuses).height());
    }

    pub(super) fn on_event(&mut self, event: &Event, size: Vec2, statuses: &[(SourceId, PluginHealth)]) -> ListEvent {
        self.list.on_event(event, statuses.len(), Self::list_rect(size, statuses))
    }

    /// `setup` is the `(prompt, typed text)` of a plugin setup value being collected, if any.
    pub(super) fn draw(&self, printer: &Printer, statuses: &[(SourceId, PluginHealth)], setup: Option<(&str, &str)>) {
        printer.with_color(ColorStyle::title_primary(), |p| {
            p.print((0, 0), &pad("Plugin warnings", p.size.x));
        });

        if statuses.is_empty() {
            printer.print((0, WARNINGS_LIST_TOP), "(no plugins registered)");
        }
        let rect = Self::list_rect(printer.size, statuses);
        let lines: Vec<String> = statuses
            .iter()
            .map(|(id, health)| {
                let icon = match health {
                    PluginHealth::Ok => "✓",
                    PluginHealth::Warn(_) => "⚠",
                    PluginHealth::Fail(_) => "✗",
                };
                format!("{icon} {id}")
            })
            .collect();
        self.list.draw(&printer.windowed(rect), &lines);

        let messages_top = rect.bottom() + 2;
        for (j, msg) in Self::messages(statuses).iter().take(WARNINGS_MESSAGES_MAX).enumerate() {
            printer.print((0, messages_top + j), &pad(msg, printer.size.x));
        }

        let bottom = printer.size.y.saturating_sub(1);
        match setup {
            Some((prompt, typed)) => {
                printer.with_color(ColorStyle::highlight_inactive(), |p| {
                    p.print((0, bottom.saturating_sub(1)), &pad(prompt, p.size.x));
                });
                printer.print((0, bottom), &pad(&format!("> {typed}  [Esc] cancel"), printer.size.x));
            }
            None => printer.with_color(ColorStyle::highlight_inactive(), |p| {
                p.print((0, bottom), &pad("  [Enter] run setup   [Esc] close", p.size.x));
            }),
        }
    }
}

impl MedleyView {
    /// Opens the warnings modal, first re-probing every plugin — `probe()`
    /// can do real I/O (a cached-token read, a slskd ping), so this forces a
    /// fresh read instead of waiting out `app`'s periodic health timer.
    pub(super) fn open_warnings(&mut self) {
        self.with_session_mut(|s| s.refresh_plugin_health());
        self.warnings = Some(WarningsModal::default());
    }

    pub(super) fn draw_warnings(&self, modal: &WarningsModal, printer: &Printer) {
        let statuses = self.with_session(|s| s.plugin_statuses().to_vec());
        let prompt = match &self.editing {
            Editing::PluginSetup(id) => Some(match self.with_session(|s| s.plugin(id)).map(|p| p.setup_kind()) {
                Some(SetupKind::TextInput { prompt }) => prompt,
                Some(SetupKind::Action) | None => String::new(),
            }),
            _ => None,
        };
        modal.draw(printer, &statuses, prompt.as_deref().map(|p| (p, self.buffer.as_str())));
    }

    pub(super) fn on_warnings_event(&mut self, event: &Event) -> EventResult {
        let statuses = self.with_session(|s| s.plugin_statuses().to_vec());
        let size = self.last_screen_size;
        let Some(modal) = &mut self.warnings else { return EventResult::Ignored };
        match modal.on_event(event, size, &statuses) {
            ListEvent::Close => {
                self.warnings = None;
                self.focus = self.fallback_focus();
            }
            ListEvent::Activate | ListEvent::Clicked => self.activate_selected_warning(),
            ListEvent::Moved | ListEvent::Unhandled => {}
        }
        EventResult::consumed()
    }

    /// Number of plugins currently reporting a non-`Ok` health.
    pub(super) fn warn_count(&self) -> usize {
        self.with_session(|s| s.plugin_warning_count())
    }

    /// `Enter` (or a click) on the selected warnings-modal row.
    fn activate_selected_warning(&mut self) {
        let Some(selected) = self.warnings.as_ref().map(WarningsModal::selected) else { return };
        let Some((id, _)) = self.with_session(|s| s.plugin_statuses().get(selected).cloned()) else {
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
