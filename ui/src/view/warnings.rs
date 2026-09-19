use std::thread;

use cursive::{Printer, Rect};
use cursive::event::{Event, Key};
use cursive::theme::ColorStyle;

use core::{CoreEvent, PluginHealth, SetupKind, SourceId};

use super::MedleyView;
use super::input::Editing;
use super::modal::{Modal, ModalOutcome, draw_modal_frame, modal_list};
use super::scroll::{ListEvent, ListState};
use super::text::pad;

/// Cap on how many plugin messages the warnings modal's bottom section shows.
const WARNINGS_MESSAGES_MAX: usize = 5;

/// Only ever called for `count > 0` — the button isn't drawn at all when there are no warnings.
pub(super) fn warnings_label(count: usize) -> String {
    format!(" ⚠ warnings ({count}) ")
}

/// The button's `(start, width)` on the hint row, shared by draw and the click hit-test; `None` without warnings.
pub(super) fn warnings_span(count: usize, row_w: usize) -> Option<(usize, usize)> {
    let width = warnings_label(count).chars().count().min(row_w);
    (count > 0).then_some((row_w - width, width))
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
    fn list_rect(rect: Rect, statuses: &[(SourceId, PluginHealth)]) -> Rect {
        let n = Self::messages(statuses).len();
        let messages_h = if n == 0 { 0 } else { 1 + n.min(WARNINGS_MESSAGES_MAX) };
        let rows = modal_list(rect);
        Rect::from_size(rows.top_left(), (rows.width(), rows.height().saturating_sub(messages_h)))
    }

    pub(super) fn relayout(&mut self, resized: bool, rect: Rect, statuses: &[(SourceId, PluginHealth)]) {
        self.list.relayout(resized, statuses.len(), Self::list_rect(rect, statuses).height());
    }

    pub(super) fn on_event(&mut self, event: &Event, rect: Rect, statuses: &[(SourceId, PluginHealth)]) -> ModalOutcome {
        match self.list.on_event(event, statuses.len(), Self::list_rect(rect, statuses)) {
            ListEvent::Close => ModalOutcome::Close,
            ListEvent::Activate | ListEvent::Clicked => ModalOutcome::Setup(self.list.cursor),
            ListEvent::Moved | ListEvent::Unhandled => ModalOutcome::Stay,
        }
    }

    /// `setup` is the `(prompt, typed text)` of a plugin setup value being collected, if any.
    pub(super) fn draw(&self, printer: &Printer, rect: Rect, statuses: &[(SourceId, PluginHealth)], setup: Option<(&str, &str)>) {
        let body = draw_modal_frame(printer, rect, Some("Plugin warnings"), "  [Enter] run setup   [Esc] close");
        if statuses.is_empty() {
            body.print((0, 1), "(no plugins registered)");
        }
        let list = Self::list_rect(rect, statuses);
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
        self.list.draw(&printer.windowed(list), &lines);

        let messages = printer.windowed(Rect::from_size((list.left(), list.bottom() + 2), (list.width(), WARNINGS_MESSAGES_MAX)));
        for (j, msg) in Self::messages(statuses).iter().take(WARNINGS_MESSAGES_MAX).enumerate() {
            messages.print((0, j), &pad(msg, messages.size.x));
        }

        // The setup prompt takes the footer's place, its typed text on the bottom row.
        if let Some((prompt, typed)) = setup {
            let frame = printer.windowed(rect);
            let bottom = frame.size.y.saturating_sub(1);
            frame.print((0, bottom), &pad(&format!("> {typed}  [Esc] cancel"), frame.size.x));
            frame.with_color(ColorStyle::highlight_inactive(), |p| {
                p.print((0, bottom.saturating_sub(1)), &pad(prompt, p.size.x));
            });
        }
    }
}

impl MedleyView {
    /// Re-probes every plugin first: `probe()` is real I/O, so the modal would otherwise show the periodic timer's stale read.
    pub(super) fn open_warnings(&mut self) {
        self.with_session_mut(|s| s.refresh_plugin_health());
        self.modal = Some(Modal::Warnings(WarningsModal::default()));
    }

    /// Number of plugins currently reporting a non-`Ok` health.
    pub(super) fn warn_count(&self) -> usize {
        self.with_session(|s| s.plugin_warning_count())
    }

    /// `Enter` (or a click) on row `selected` of the warnings modal.
    pub(super) fn activate_warning(&mut self, selected: usize) {
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
