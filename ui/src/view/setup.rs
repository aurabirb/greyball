use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;

use cursive::{Printer, Rect, Vec2};
use cursive::event::{Event, Key, MouseButton, MouseEvent};
use cursive::theme::ColorStyle;
use unicode_width::UnicodeWidthStr;

use core::{Bus, CoreEvent, LogBuf, Plugin, PluginHealth, SetupLog, SetupPrompt, SourceId};

use crate::SessionHandle;

use super::MedleyView;
use super::log::LogPane;
use super::modal::{Modal, ModalOutcome, draw_modal_frame, modal_body};
use super::panes::{draw_float_frame, float_body};
use super::scroll::{Nav, PAGE_SCROLL_STEP};
use super::text::{pad, tail_fit};

const CLOSE: &str = "[ Close ]";
const MAX_WIDTH: usize = 100;
const TRANSCRIPT_LINES: usize = 2000;

enum Phase {
    Asking(SetupPrompt),
    Running(SetupLog),
    Stopped,
}

/// A plugin's setup as a chat: its questions, the answers given and its progress, in a scrollable log.
pub(super) struct SetupModal {
    plugin: Arc<dyn Plugin>,
    id: SourceId,
    lines: Arc<LogBuf>,
    transcript: LogPane,
    answers: Vec<String>,
    typed: String,
    phase: Phase,
}

impl Drop for SetupModal {
    fn drop(&mut self) {
        if let Phase::Running(log) = &self.phase {
            log.cancel();
        }
    }
}

impl SetupModal {
    fn new(plugin: Arc<dyn Plugin>) -> Self {
        let lines = Arc::new(LogBuf::new(TRANSCRIPT_LINES));
        Self {
            id: plugin.id(),
            plugin,
            transcript: LogPane::new(lines.clone()),
            lines,
            answers: Vec::new(),
            typed: String::new(),
            phase: Phase::Stopped,
        }
    }

    fn push(&self, text: &str) {
        for line in text.lines() {
            self.lines.push(line.to_string());
        }
    }

    /// The border box, centred on `screen`.
    fn frame(screen: Vec2) -> Rect {
        let size = Vec2::new((screen.x * 9 / 10).clamp(30, MAX_WIDTH), (screen.y * 4 / 5).max(12)).or_min(screen);
        Rect::from_size((screen - size) / 2, size)
    }

    /// The log and, under a spacer row, the input row.
    fn areas(rect: Rect) -> (Rect, Rect) {
        let body = modal_body(rect, true);
        let input = Rect::from_size((body.left(), body.top() + body.height().saturating_sub(1)), (body.width(), 1));
        (Rect::from_size(body.top_left(), (body.width(), body.height().saturating_sub(2))), input)
    }

    fn close_rect(rect: Rect) -> Rect {
        let width = CLOSE.width().min(rect.width());
        Rect::from_size((rect.left() + rect.width() - width, rect.top()), (width, 1))
    }

    /// Shows the next question; `false` when the answers are complete.
    fn ask(&mut self) -> bool {
        self.typed.clear();
        let Some(prompt) = self.plugin.setup_prompt(&self.answers) else { return false };
        let hint = match (&prompt.default, prompt.secret) {
            (Some(_), true) => "  [Enter keeps the current one]".to_string(),
            (Some(default), false) => format!("  [Enter for {default}]"),
            (None, _) => String::new(),
        };
        self.push(&format!("{}{hint}", prompt.text));
        self.phase = Phase::Asking(prompt);
        true
    }

    fn begin(&mut self, session: &SessionHandle, bus: &Bus) {
        if !self.ask() {
            self.start(session, bus);
        }
    }

    /// Runs `setup` off the UI thread; the outcome comes back as `SetupDone`, dropped when the user abandoned it.
    fn start(&mut self, session: &SessionHandle, bus: &Bus) {
        let log = SetupLog::new(self.id.clone(), bus.clone());
        self.phase = Phase::Running(log.clone());
        let (plugin, answers, session, bus) = (self.plugin.clone(), self.answers.clone(), session.clone(), bus.clone());
        session.lock().unwrap().mark_setup(log.id(), true);
        thread::spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                let health = plugin.setup(answers, &log);
                if health.is_ok() { plugin.probe() } else { health }
            }));
            let health = outcome.unwrap_or_else(|_| PluginHealth::Fail("setup crashed".to_string()));
            let abandoned = log.cancelled();
            let wiring = plugin.wiring();
            let mut guard = session.lock().unwrap();
            guard.apply_wiring(log.id(), wiring);
            if !abandoned {
                guard.record_setup_result(log.id().clone(), health.clone());
            }
            guard.mark_setup(log.id(), false);
            drop(guard);
            bus.send(CoreEvent::PluginStatusChanged);
            if abandoned {
                return;
            }
            if health.is_ok() {
                bus.send(CoreEvent::PluginLoginSucceeded);
            }
            bus.send(CoreEvent::SetupDone { id: log.id().clone(), run: log.run(), health });
        });
    }

    /// Enter: takes the typed answer to the open question, or starts over after a stop.
    fn advance(&mut self, session: &SessionHandle, bus: &Bus) {
        match &self.phase {
            Phase::Asking(prompt) => {
                let typed = if prompt.secret { self.typed.as_str() } else { self.typed.trim() };
                let answer = if typed.is_empty() { prompt.default.clone().unwrap_or_default() } else { typed.to_string() };
                let shown = match (answer.is_empty(), prompt.secret) {
                    (true, _) => "(Enter)".to_string(),
                    (false, true) => "*".repeat(8),
                    (false, false) => answer.clone(),
                };
                self.push(&format!("> {shown}"));
                match self.plugin.setup_answer(&self.answers, answer) {
                    Ok(answer) => self.answers.push(answer),
                    Err(message) => {
                        self.push(&format!("! {message}"));
                        self.typed.clear();
                        return;
                    }
                }
            }
            Phase::Stopped => {
                self.answers.clear();
                self.push("Starting over.");
            }
            Phase::Running(_) => return,
        }
        self.begin(session, bus);
    }

    fn stop(&mut self, reason: &str) {
        self.push(&format!("Setup did not finish: {reason}"));
        self.phase = Phase::Stopped;
    }

    fn line(&self, run: u64, line: &str) {
        if matches!(&self.phase, Phase::Running(log) if log.run() == run) {
            self.push(line);
        }
    }

    /// Setup run `run` ended with `health`, the plugin's own probe after it; returns whether the source is ready.
    fn done(&mut self, run: u64, health: &PluginHealth) -> bool {
        if !matches!(&self.phase, Phase::Running(log) if log.run() == run) {
            return false;
        }
        match health.message() {
            Some(reason) => {
                self.stop(reason);
                false
            }
            None => {
                self.push("Setup finished.");
                true
            }
        }
    }

    fn warning(&self, context: &str, message: &str) {
        if context == self.id.as_str() {
            self.push(&format!("! warning: {message}"));
        }
    }

    pub(super) fn on_event(&mut self, event: &Event, screen: Vec2) -> ModalOutcome {
        let rect = float_body(Self::frame(screen));
        if let Event::Mouse { offset, position, event: MouseEvent::Press(MouseButton::Left) } = event {
            let on_close = position.checked_sub(*offset).is_some_and(|pos| Self::close_rect(rect).contains(pos));
            return if on_close { ModalOutcome::Close } else { ModalOutcome::Stay };
        }
        // A typed j/k/J/K is text, not scrolling.
        if let Some(nav) = Nav::of(event).filter(|_| !matches!(event, Event::Char(_))) {
            let (up, step) = nav.step(PAGE_SCROLL_STEP);
            let log = Self::areas(rect).0;
            self.transcript.scroll_by(up, step, (log.width() > 0).then_some((log.width(), log.height())));
            return ModalOutcome::Stay;
        }
        match (event, &self.phase) {
            (Event::Key(Key::Esc), _) => ModalOutcome::Close,
            (Event::Key(Key::Enter), Phase::Asking(_) | Phase::Stopped) => ModalOutcome::Submit,
            (Event::Char(c), Phase::Asking(_)) => {
                self.typed.push(*c);
                ModalOutcome::Stay
            }
            (Event::CtrlChar('u'), Phase::Asking(_)) => {
                self.typed.clear();
                ModalOutcome::Stay
            }
            (Event::Key(Key::Backspace), Phase::Asking(_)) => {
                self.typed.pop();
                ModalOutcome::Stay
            }
            _ => ModalOutcome::Stay,
        }
    }

    pub(super) fn draw(&self, printer: &Printer, screen: Vec2) {
        let frame = Self::frame(screen);
        draw_float_frame(printer, frame);
        let rect = float_body(frame);
        let footer = match &self.phase {
            Phase::Asking(_) => "  [Enter] send   [PgUp/PgDn] scroll   [Esc] close",
            Phase::Stopped => "  [Enter] start over   [PgUp/PgDn] scroll   [Esc] close",
            Phase::Running(_) => "  [PgUp/PgDn] scroll   [Esc] abandon",
        };
        draw_modal_frame(printer, rect, Some(&format!("Set up {}", self.id)), footer);
        let close = Self::close_rect(rect);
        printer.windowed(close).with_color(ColorStyle::title_primary(), |p| p.print((0, 0), CLOSE));

        let (log, input) = Self::areas(rect);
        self.transcript.draw_lines(&printer.windowed(log));
        let input = printer.windowed(input);
        let line = match &self.phase {
            Phase::Asking(prompt) => {
                let shown = if prompt.secret { "*".repeat(self.typed.chars().count()) } else { self.typed.clone() };
                format!("{}█", tail_fit(&format!("> {shown}"), input.size.x.saturating_sub(1)))
            }
            Phase::Running(_) => "Working on it…".to_string(),
                    Phase::Stopped => String::new(),
        };
        input.print((0, 0), &pad(&line, input.size.x));
    }
}

impl MedleyView {
    /// Opens `id`'s setup dialog over the main view, asking its first question (or already running, for a plugin needing no input).
    pub(super) fn open_setup(&mut self, id: &SourceId) {
        self.with_session_mut(|s| s.refresh_plugin_health());
        if self.with_session(|s| s.setup_running(id)) {
            self.set_flash(format!("An earlier setup of {id} is still running; try again in a moment"));
            return;
        }
        let Some(plugin) = self.with_session(|s| s.plugin(id)) else { return };
        let bus = self.with_session(|s| s.bus.clone());
        let mut modal = SetupModal::new(plugin);
        modal.begin(&self.session, &bus);
        self.modal = Some(Modal::Setup(modal));
    }

    pub(super) fn submit_setup(&mut self) {
        let bus = self.with_session(|s| s.bus.clone());
        let session = self.session.clone();
        if let Some(Modal::Setup(m)) = &mut self.modal {
            m.advance(&session, &bus);
        }
    }

    /// Feeds the open setup dialog what the drained `events` say about its plugin; closes it once the source is ready.
    pub(crate) fn on_setup_events(&mut self, events: &[CoreEvent]) {
        for event in events {
            let ready = match event {
                CoreEvent::SetupDone { run, health, .. } => {
                    let Some(Modal::Setup(m)) = &mut self.modal else { return };
                    m.done(*run, health)
                }
                CoreEvent::SetupLine { run, line, .. } => {
                    if let Some(Modal::Setup(m)) = &self.modal {
                        m.line(*run, line);
                    }
                    false
                }
                CoreEvent::BackgroundFailure { context, message } => {
                    if let Some(Modal::Setup(m)) = &self.modal {
                        m.warning(context, message);
                    }
                    false
                }
                _ => false,
            };
            if ready {
                let Some(Modal::Setup(m)) = &self.modal else { return };
                let id = m.id.clone();
                self.close_modal();
                self.set_flash(format!("{id} is ready"));
            }
        }
    }
}
