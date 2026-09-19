use std::ops::{Index, IndexMut};
use std::sync::Arc;

use cursive::{Printer, Rect};
use cursive::event::{Event, Key, MouseEvent};

use core::{Command, LogBuf, PaneLayoutConfig, PaneMode, Session};

use crate::screen::{Kind, Screen};
use crate::vis::Vis;

use super::log::LogPane;
use super::scroll::{Nav, PAGE_SCROLL_STEP};
use super::settings::{SettingsEntry, SettingsPane};
use super::track_list::{ListFrame, TrackList};

/// One window instance; says nothing about its kind or where it is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct WindowId(usize);

/// Where the shell shows a window: as the active tab, docked beside it, fullscreen, or in a box over the view.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Placement {
    Tabbed,
    Docked,
    Screen,
    Floating,
}

impl From<PaneMode> for Placement {
    fn from(mode: PaneMode) -> Self {
        match mode {
            PaneMode::Screen => Placement::Screen,
            PaneMode::Embedded => Placement::Docked,
            PaneMode::Float => Placement::Floating,
        }
    }
}

/// What the shell hands a window under its one session lock.
pub(super) struct Ctx<'a> {
    pub(super) s: &'a Session,
    pub(super) pane_cfg: PaneLayoutConfig,
    /// A search query is being typed.
    pub(super) searching: bool,
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
    pub(super) placement: Placement,
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
    log: Arc<LogBuf>,
    vis: Arc<Vis>,
    tabs: Vec<(Screen, WindowId)>,
    /// The one pane window each `:log`/`:settings`/`:vis`/`:queue`/`:history` command names.
    command_panes: Vec<(Kind, WindowId)>,
}

impl Windows {
    /// A tab per screen and one window per `:panes` name, placed by `pane_mode`.
    pub(super) fn new(log: Arc<LogBuf>, vis: Arc<Vis>, pane_mode: PaneMode) -> Self {
        let mut windows = Self { items: Vec::new(), log, vis, tabs: Vec::new(), command_panes: Vec::new() };
        for screen in Screen::ALL {
            let id = windows.add(Kind::List(screen), Placement::Tabbed);
            windows.tabs.push((screen, id));
        }
        for kind in [Kind::Log, Kind::Settings, Kind::Vis, Kind::List(Screen::Queue), Kind::List(Screen::History)] {
            let id = windows.add(kind, pane_mode.into());
            windows.command_panes.push((kind, id));
        }
        windows
    }

    fn add(&mut self, kind: Kind, placement: Placement) -> WindowId {
        let body = match kind {
            Kind::Log => Body::Log(LogPane::new(self.log.clone())),
            Kind::Settings => Body::Settings(SettingsPane::default()),
            Kind::Vis => Body::Vis(self.vis.clone()),
            Kind::List(screen) => Body::List(TrackList::new(screen)),
        };
        self.items.push(Window { kind, placement, body, rect: Rect::from_size((0, 0), (0, 0)) });
        WindowId(self.items.len() - 1)
    }

    /// Every window a pane command names.
    pub(super) fn panes(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.command_panes.iter().map(|&(_, id)| id)
    }

    pub(super) fn tab(&self, screen: Screen) -> WindowId {
        self.tabs.iter().find(|&&(tab, _)| tab == screen).expect("`Windows::new` builds a tab per screen").1
    }

    /// The window the pane command for `kind` opens.
    pub(super) fn command_pane(&self, kind: Kind) -> Option<WindowId> {
        self.command_panes.iter().find(|&&(pane, _)| pane == kind).map(|&(_, id)| id)
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
