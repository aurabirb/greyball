use std::ops::{Index, IndexMut};
use std::sync::Arc;

use cursive::{Printer, Rect};
use cursive::event::{Event, Key, MouseEvent};

use core::{Command, LogBuf, PaneLayoutConfig, Session};

use crate::screen::{Kind, Placement, WINDOWS};
use crate::vis::Vis;

use super::log::LogPane;
use super::scroll::{Nav, PAGE_SCROLL_STEP};
use super::settings::{SettingsEntry, SettingsPane};
use super::track_list::{ListFrame, TrackList};

/// One window instance; says nothing about its kind or where it is shown.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct WindowId(usize);

/// What the shell hands a window under its one session lock.
pub(super) struct Ctx<'a> {
    pub(super) s: &'a Session,
    pub(super) pane_cfg: PaneLayoutConfig,
    /// A search query is being typed.
    pub(super) searching: bool,
    pub(super) placements: &'a Placements,
}

/// Every window's placement, by id, apart from the windows so a `Ctx` can lend it while one window is borrowed mutably.
pub(super) struct Placements {
    of: Vec<Placement>,
    generation: u64,
}

impl Placements {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Each startup window's name and placement.
    pub(super) fn named(&self) -> impl Iterator<Item = (&'static str, Placement)> + '_ {
        WINDOWS.iter().zip(&self.of).map(|(startup, &placement)| (startup.name, placement))
    }
}

/// What an event meant to a window, for the shell to act on; `Ignored` lets the shell route it on.
pub(super) enum WindowOutcome {
    Ignored,
    Consumed,
    Run(Command),
    /// Toggle this row of `settings_entries`.
    ToggleSetting(usize),
}

/// A window's session-derived draw data, taken under the frame's one lock.
pub(super) enum WindowFrame {
    List(Arc<ListFrame>),
    Settings(Arc<Vec<SettingsEntry>>),
    /// Log and Vis read only their own live state.
    Live,
}

enum Body {
    List(TrackList),
    Log(LogPane),
    Settings(SettingsPane),
    Vis(Arc<Vis>),
}

/// A window: one component plus the rect the shell last laid it out in, which draw and hit-test share.
pub(super) struct Window {
    pub(super) kind: Kind,
    body: Body,
    rect: Rect,
}

impl Window {
    pub(super) fn list(&self) -> Option<&TrackList> {
        match &self.body {
            Body::List(list) => Some(list),
            _ => None,
        }
    }

    pub(super) fn list_mut(&mut self) -> Option<&mut TrackList> {
        match &mut self.body {
            Body::List(list) => Some(list),
            _ => None,
        }
    }

    pub(super) fn rect(&self) -> Rect {
        self.rect
    }

    /// Takes the rect this layout pass gave the window and keeps its scroll state inside it.
    pub(super) fn relayout(&mut self, rect: Rect, s: &Session) {
        let resized = rect.height() != self.rect.height();
        self.rect = rect;
        if let Body::List(list) = &mut self.body {
            list.relayout(resized, s, rect);
        }
    }

    /// Brings a list's scroll window back around its cursor.
    pub(super) fn follow(&mut self) {
        if let Body::List(list) = &mut self.body {
            list.follow(self.rect);
        }
    }

    pub(super) fn frame(&self, ctx: &Ctx) -> WindowFrame {
        match &self.body {
            Body::List(list) => WindowFrame::List(list.frame(ctx, self.rect)),
            Body::Settings(settings) => WindowFrame::Settings(settings.entries(ctx)),
            Body::Log(_) | Body::Vis(_) => WindowFrame::Live,
        }
    }

    /// Draws into `printer`'s window over `rect()`; `frame` is this window's own `frame()`.
    pub(super) fn draw(&self, printer: &Printer, focused: bool, frame: &WindowFrame) {
        let printer = &printer.windowed(self.rect);
        match (&self.body, frame) {
            (Body::List(list), WindowFrame::List(frame)) => list.draw(printer, focused, frame),
            (Body::Settings(settings), WindowFrame::Settings(entries)) => settings.draw(printer, entries, focused),
            (Body::Log(log), _) => log.draw(printer, focused),
            (Body::Vis(vis), _) => vis.draw(printer, focused),
            (Body::List(_) | Body::Settings(_), _) => {}
        }
    }

    pub(super) fn on_event(&mut self, event: &Event, ctx: &Ctx) -> WindowOutcome {
        let rect = self.rect;
        if let Body::List(list) = &mut self.body {
            return list.on_event(event, ctx, rect);
        }
        if let Event::Mouse { offset, position, event: mouse } = event {
            let inside = position.checked_sub(*offset).is_some_and(|pos| rect.contains(pos));
            // Log is free-form terminal output the user wants to select/copy with the mouse.
            let selecting =
                matches!(self.body, Body::Log(_)) && !matches!(mouse, MouseEvent::WheelUp | MouseEvent::WheelDown);
            if !inside || selecting {
                return WindowOutcome::Ignored;
            }
        }
        let body_h = rect.height().saturating_sub(1);
        match (&mut self.body, Nav::of(event).map(|nav| nav.step(PAGE_SCROLL_STEP))) {
            (Body::Settings(settings), Some((up, step))) => settings.jump(up, step, settings.entries(ctx).len(), body_h),
            (Body::Settings(settings), None) if matches!(event, Event::Key(Key::Enter) | Event::Char(' ')) => {
                return WindowOutcome::ToggleSetting(settings.cursor());
            }
            // Until a layout pass gives it a width there is nothing to wrap against.
            (Body::Log(log), Some((up, step))) => log.scroll_by(up, step, (rect.width() > 0).then_some((rect.width(), body_h))),
            // Vis is live; it swallows nav keys like any focused pane.
            (Body::Vis(_), Some(_)) => {}
            _ if matches!(event, Event::Mouse { .. }) => {}
            _ => return WindowOutcome::Ignored,
        }
        WindowOutcome::Consumed
    }
}

/// Every window instance, by id; ids stay valid because windows are never removed.
pub(super) struct Windows {
    items: Vec<Window>,
    placements: Placements,
    log: Arc<LogBuf>,
    vis: Arc<Vis>,
}

impl Windows {
    /// One window per `WINDOWS` entry, in its order; the pane windows are placed by `panes`.
    pub(super) fn new(log: Arc<LogBuf>, vis: Arc<Vis>, panes: Placement) -> Self {
        let placements = Placements { of: Vec::new(), generation: 0 };
        let mut windows = Self { items: Vec::new(), placements, log, vis };
        for startup in &WINDOWS {
            windows.add(startup.kind, if startup.tabbed { Placement::Tabbed } else { panes });
        }
        windows
    }

    fn add(&mut self, kind: Kind, placement: Placement) -> WindowId {
        let body = match kind {
            Kind::Log => Body::Log(LogPane::new(self.log.clone())),
            Kind::Settings => Body::Settings(SettingsPane::default()),
            Kind::Vis => Body::Vis(self.vis.clone()),
            Kind::List(list) => Body::List(TrackList::new(list)),
        };
        self.items.push(Window { kind, body, rect: Rect::from_size((0, 0), (0, 0)) });
        self.placements.of.push(placement);
        WindowId(self.items.len() - 1)
    }

    pub(super) fn ids(&self) -> impl Iterator<Item = WindowId> + use<> {
        (0..self.items.len()).map(WindowId)
    }

    /// The startup window `:panes`, `:window` and `state.toml` call `name`.
    pub(super) fn named(&self, name: &str) -> Option<WindowId> {
        WINDOWS.iter().position(|startup| startup.name == name).map(WindowId)
    }

    pub(super) fn name(&self, id: WindowId) -> &'static str {
        WINDOWS[id.0].name
    }

    /// The windows `:panes <mode>` without a name places: those that are not tabs now.
    pub(super) fn panes(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.ids().filter(|&id| self.placement(id) != Placement::Tabbed)
    }

    pub(super) fn placements(&self) -> &Placements {
        &self.placements
    }

    pub(super) fn placement(&self, id: WindowId) -> Placement {
        self.placements.of[id.0]
    }

    /// The one write to a placement; the shell's `set_placement` keeps the tab bar and `open` in step.
    pub(super) fn place(&mut self, id: WindowId, placement: Placement) {
        self.placements.of[id.0] = placement;
        self.placements.generation += 1;
    }

    /// Offers `event` to each of `ids`; the first window not ignoring it, and its outcome.
    pub(super) fn send(
        &mut self,
        ids: &[WindowId],
        event: &Event,
        (s, pane_cfg, searching): (&Session, PaneLayoutConfig, bool),
    ) -> Option<(WindowId, WindowOutcome)> {
        let ctx = Ctx { s, pane_cfg, searching, placements: &self.placements };
        ids.iter().find_map(|&id| match self.items[id.0].on_event(event, &ctx) {
            WindowOutcome::Ignored => None,
            outcome => Some((id, outcome)),
        })
    }
}

impl Index<WindowId> for Windows {
    type Output = Window;
    fn index(&self, id: WindowId) -> &Window {
        &self.items[id.0]
    }
}

impl IndexMut<WindowId> for Windows {
    fn index_mut(&mut self, id: WindowId) -> &mut Window {
        &mut self.items[id.0]
    }
}
