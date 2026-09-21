use unicode_width::UnicodeWidthStr;

use cursive::{Printer, Rect};
use cursive::event::Event;

use core::{PluginHealth, Session, SourceId};

use super::MedleyView;
use super::modal::{Modal, ModalOutcome, draw_modal_frame, modal_list};
use super::scroll::{ListEvent, ListState};
use super::text::wrap;

/// Rows of the message area under the list, which shows the selected row's text in full.
const MESSAGE_ROWS: usize = 5;

/// Only ever called for `count > 0` — the button isn't drawn at all when there are no warnings.
pub(super) fn warnings_label(count: usize) -> String {
    format!(" ⚠ warnings ({count}) ")
}

/// The button's cells at the right end of `row`, shared by draw and the click hit-test.
pub(super) fn warnings_rect(count: usize, row: Rect) -> Rect {
    let width = warnings_label(count).width().min(row.width());
    Rect::from_size((row.left() + row.width() - width, row.top()), (width, 1))
}

/// What the modal lists, read under one lock: every plugin's health, then this session's background failures.
pub(super) struct Warnings {
    statuses: Vec<(SourceId, PluginHealth)>,
    failures: Vec<(String, String)>,
}

impl Warnings {
    pub(super) fn read(s: &Session) -> Self {
        Self { statuses: s.plugin_statuses().to_vec(), failures: s.background_failures().to_vec() }
    }

    fn rows(&self) -> usize {
        self.statuses.len() + self.failures.len()
    }

    /// Row `i`'s text in full: a plugin's health message, or a failure as listed.
    fn message(&self, i: usize) -> Option<String> {
        match self.statuses.get(i) {
            Some((id, health)) => health.message().map(|m| format!("{id}: {m}")),
            None => self.failures.get(i - self.statuses.len()).map(|(context, message)| format!("{context}: {message}")),
        }
    }
}

/// The warnings modal: plugin rows that run setup, then background-failure rows, above the selected row's full text.
#[derive(Default)]
pub(super) struct WarningsModal {
    list: ListState,
}

impl WarningsModal {
    /// The navigable list and, a spacer row below it, the message area: the one layout draw, keys and clicks share.
    fn areas(rect: Rect) -> (Rect, Rect) {
        let rows = modal_list(rect);
        let message_h = MESSAGE_ROWS.min(rows.height() / 2);
        let list = Rect::from_size(rows.top_left(), (rows.width(), rows.height().saturating_sub(message_h + 1)));
        (list, Rect::from_size((rows.left(), rows.top() + list.height() + 1), (rows.width(), message_h)))
    }

    pub(super) fn relayout(&mut self, resized: bool, rect: Rect, warnings: &Warnings) {
        self.list.relayout(resized, warnings.rows(), Self::areas(rect).0.height());
    }

    pub(super) fn on_event(&mut self, event: &Event, rect: Rect, warnings: &Warnings) -> ModalOutcome {
        match self.list.on_event(event, warnings.rows(), Self::areas(rect).0) {
            ListEvent::Close => ModalOutcome::Close,
            // A background failure has nothing to set up.
            ListEvent::Activate | ListEvent::Clicked if self.list.cursor < warnings.statuses.len() => {
                ModalOutcome::Setup(self.list.cursor)
            }
            _ => ModalOutcome::Stay,
        }
    }

    pub(super) fn draw(&self, printer: &Printer, rect: Rect, warnings: &Warnings) {
        let body = draw_modal_frame(printer, rect, Some("Warnings"), "  [Enter] run setup   [Esc] close");
        if warnings.rows() == 0 {
            body.print((0, 1), "(no plugins registered)");
        }
        let (list, message) = Self::areas(rect);
        let plugins = warnings.statuses.iter().map(|(id, health)| {
            let icon = match health {
                PluginHealth::Ok => "✓",
                PluginHealth::Warn(_) => "⚠",
                PluginHealth::Fail(_) => "✗",
            };
            format!("{icon} {id}")
        });
        let failures = warnings.failures.iter().map(|(context, message)| format!("! {context}: {message}"));
        let lines: Vec<String> = plugins.chain(failures).collect();
        self.list.draw(&printer.windowed(list), &lines);

        let text = warnings.message(self.list.cursor).unwrap_or_default();
        for (y, line) in wrap(&text, message.width()).iter().take(message.height()).enumerate() {
            printer.windowed(message).print((0, y), line);
        }
    }
}

impl MedleyView {
    /// Re-probes every plugin first: `probe()` is real I/O, so the modal would otherwise show the periodic timer's stale read.
    pub(super) fn open_warnings(&mut self) {
        self.with_session_mut(|s| s.refresh_plugin_health());
        self.modal = Some(Modal::Warnings(WarningsModal::default()));
    }

    /// What the warnings button counts.
    pub(super) fn warn_count(&self) -> usize {
        self.with_session(|s| s.warning_count())
    }

    /// `Enter` (or a click) on row `selected` of the warnings modal.
    pub(super) fn activate_warning(&mut self, selected: usize) {
        if let Some((id, _)) = self.with_session(|s| s.plugin_statuses().get(selected).cloned()) {
            self.open_setup(&id);
        }
    }
}
