use std::ops::{Index, IndexMut};
use std::sync::Arc;

use cursive::{Printer, Rect};
use cursive::event::{Event, Key, MouseEvent};
use cursive::theme::{BaseColor, Color, ColorStyle};

use unicode_width::UnicodeWidthStr;

use core::{Command, HotkeyTarget, LogBuf, PaneLayoutConfig, Session};

use crate::screen::{Corners, Home, Kind, Placement, Startup, WINDOWS};
use crate::vis::Vis;

use super::Chrome;
use super::files::FilesPane;
use super::help::{Built, HelpPane};
use super::log::LogPane;
use super::scroll::{Nav, PAGE_SCROLL_STEP};
use super::text::{pad, pad_right_aligned};
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
    Bind(HotkeyTarget, char),
    Unbind(HotkeyTarget),
    /// Add this file to the open playlist, else the queue.
    AddFile(std::path::PathBuf),
}

/// A window's session-derived draw data, taken under the frame's one lock.
pub(super) enum WindowFrame {
    List(Arc<ListFrame>),
    Settings(Arc<Vec<SettingsEntry>>),
    Help(Arc<Built>),
    /// Log and Vis read only their own live state.
    Live,
}

enum Body {
    List(Box<TrackList>),
    Log(LogPane),
    Settings(SettingsPane),
    Vis(Arc<Vis>),
    Help(HelpPane),
    Files(FilesPane),
}

/// A window's own last row: the last bind result and whether it was a refusal, else the flash, else the component's hints.
#[derive(Default)]
struct StatusRow {
    message: Option<(String, bool)>,
}

/// What the shell lends a window's status row for one frame; the focus-only parts are `None` in an unfocused window.
pub(super) struct StatusCtx<'a> {
    pub(super) flash: Option<&'a str>,
    /// The placement key's hint.
    pub(super) place: Option<String>,
    pub(super) chrome: &'a Chrome,
    /// Some window is docked.
    pub(super) docked: bool,
    /// Whether the idle text of key hints is drawn.
    pub(super) hints: bool,
    /// Cells at the row's right end the shell draws over.
    pub(super) reserved: usize,
}

impl StatusCtx<'_> {
    /// `[Esc] close` where Esc closes the window, then the placement hint.
    pub(super) fn tail(&self, placement: Placement) -> Vec<String> {
        let close = placement.closes_on_esc().then(|| "[Esc] close".to_string());
        close.into_iter().chain(self.place.clone()).collect()
    }

    /// `[<assigned keys>] label`, the run cut with `…` to what `others` leave of a `fit`-wide row; `None` when no key fits.
    pub(super) fn keys_run(&self, label: &str, others: &[String], fit: usize) -> Option<String> {
        let used: usize = others.iter().map(|hint| hint.width() + 3).sum();
        let room = fit.saturating_sub(used + label.width() + 3);
        let keys = &self.chrome.assigned;
        let run: String = match keys.len() {
            0 => return None,
            n if n <= room => keys.iter().collect(),
            _ if room < 2 => return None,
            _ => keys.iter().take(room - 1).chain(&['…']).collect(),
        };
        Some(format!("[{run}] {label}"))
    }
}

/// `[a/b] label` over the keys that are bound; `None` when none is.
pub(super) fn hint(keys: &[Option<char>], label: &str) -> Option<String> {
    let keys: Vec<String> = keys.iter().flatten().map(char::to_string).collect();
    (!keys.is_empty()).then(|| format!("[{}] {label}", keys.join("/")))
}

/// A window: one component plus the rect the shell last laid it out in, which draw and hit-test share.
pub(super) struct Window {
    pub(super) kind: Kind,
    body: Body,
    rect: Rect,
    status: StatusRow,
}

impl Window {
    pub(super) fn list(&self) -> Option<&TrackList> {
        match &self.body {
            Body::List(list) => Some(list.as_ref()),
            _ => None,
        }
    }

    pub(super) fn list_mut(&mut self) -> Option<&mut TrackList> {
        match &mut self.body {
            Body::List(list) => Some(list.as_mut()),
            _ => None,
        }
    }

    pub(super) fn rect(&self) -> Rect {
        self.rect
    }

    /// A window with a status row offers both bottom corners in it.
    pub(super) const CORNERS: Corners = Corners::BOTH;

    /// Whether the window has a status row at all, which its kind alone decides: Vis draws its picture on every row.
    pub(super) fn shows_status(&self) -> bool {
        !matches!(self.body, Body::Vis(_))
    }

    /// The status row in screen coordinates, when the window has room to draw it.
    pub(super) fn status_rect(&self) -> Option<Rect> {
        let y = self.rect.height().checked_sub(1).filter(|&y| y > 0 && self.shows_status())?;
        Some(Rect::from_size((self.rect.left(), self.rect.top() + y), (self.rect.width(), 1)))
    }

    /// `rect()` without the status row: the rect the component lays out, draws and hit-tests in.
    fn content(&self) -> Rect {
        let row = usize::from(self.shows_status());
        Rect::from_size(self.rect.top_left(), (self.rect.width(), self.rect.height().saturating_sub(row)))
    }

    /// Takes the rect this layout pass gave the window and keeps its scroll state inside it.
    pub(super) fn relayout(&mut self, rect: Rect, s: &Session) {
        let resized = rect.height() != self.rect.height();
        self.rect = rect;
        let rect = self.content();
        match &mut self.body {
            Body::List(list) => list.relayout(resized, s, rect),
            Body::Help(help) => help.relayout(rect, s),
            Body::Files(files) => files.relayout(rect),
            _ => {}
        }
    }

    /// The window lost focus or closed.
    pub(super) fn blur(&mut self) {
        if let Body::Help(help) = &mut self.body {
            help.blur();
        }
        self.status.message = None;
    }

    /// Puts a message on the window's own status row until its next key.
    pub(super) fn set_status(&mut self, text: &str, refused: bool) {
        self.status.message = Some((text.to_string(), refused));
    }

    /// Brings a list's scroll window back around its cursor.
    pub(super) fn follow(&mut self) {
        let rect = self.content();
        if let Body::List(list) = &mut self.body {
            list.follow(rect);
        }
    }

    pub(super) fn frame(&self, ctx: &Ctx) -> WindowFrame {
        match &self.body {
            Body::List(list) => WindowFrame::List(list.frame(ctx, self.content())),
            Body::Settings(settings) => WindowFrame::Settings(settings.entries(ctx)),
            Body::Help(help) => WindowFrame::Help(help.built(ctx.s, self.content())),
            Body::Log(_) | Body::Vis(_) | Body::Files(_) => WindowFrame::Live,
        }
    }

    /// The status row's text when nothing was reported; `placement` is the window's.
    fn idle(&self, frame: &WindowFrame, placement: Placement, status: &StatusCtx, fit: usize) -> String {
        let pane = |keys: &str| {
            let hints = Some(keys.to_string()).filter(|keys| !keys.is_empty()).into_iter().chain(status.tail(placement));
            hints.collect::<Vec<_>>().join("   ")
        };
        match (&self.body, frame) {
            (Body::List(list), WindowFrame::List(frame)) => list.idle(frame, placement, status, fit),
            (Body::Help(help), WindowFrame::Help(built)) => help.idle(built, placement, status),
            (Body::Settings(_), _) => pane("[j/k] move   [Enter] toggle"),
            (Body::Log(_), _) => pane("[j/k] scroll   [PgUp/PgDn] page"),
            (Body::Files(files), _) => pane(files.idle()),
            _ => pane(""),
        }
    }

    /// Draws into `printer`'s window over `rect()`; `frame` is this window's own `frame()`, `placement` the window's.
    pub(super) fn draw(&self, printer: &Printer, focused: bool, frame: &WindowFrame, placement: Placement, status: &StatusCtx) {
        let printer = &printer.windowed(self.rect);
        let content = &printer.windowed(Rect::from_size((0, 0), self.content().size()));
        match (&self.body, frame) {
            (Body::List(list), WindowFrame::List(frame)) => list.draw(content, focused, frame),
            (Body::Settings(settings), WindowFrame::Settings(entries)) => settings.draw(content, entries, focused),
            (Body::Log(log), _) => log.draw(content, focused),
            (Body::Files(files), _) => files.draw(content, focused),
            (Body::Vis(vis), _) => vis.draw(content, focused),
            (Body::Help(help), WindowFrame::Help(built)) => help.draw(content, focused, built),
            (Body::List(_) | Body::Settings(_) | Body::Help(_), _) => {}
        }
        let Some(y) = printer.size.y.checked_sub(1).filter(|&y| y > 0) else { return };
        if !self.shows_status() {
            return;
        }
        let message = self.status.message.clone().or_else(|| status.flash.map(|flash| (flash.to_string(), false)));
        let idle = message.is_none();
        let room = printer.size.x.saturating_sub(status.reserved);
        let count = match (&self.body, frame) {
            (Body::List(list), WindowFrame::List(frame)) => list.count(frame),
            _ => None,
        };
        let fit = room.saturating_sub(count.as_ref().map_or(0, |count| count.width() + 2));
        let (text, refused) = message.unwrap_or_else(|| (if status.hints { self.idle(frame, placement, status, fit) } else { String::new() }, false));
        let style = if refused { ColorStyle::front(Color::Dark(BaseColor::Yellow)) } else { ColorStyle::primary() };
        // A message keeps its room; the idle hint gives way to the count.
        let count = count.filter(|count| idle || text.width() + count.width() + 2 <= room);
        let left = count.as_ref().map_or(room, |count| room.saturating_sub(count.width() + 2));
        printer.with_color(style, |p| {
            p.print((0, y), &pad(&text, left));
            if let Some(count) = &count {
                p.print((left, y), &pad_right_aligned(&format!("{count} "), room - left));
            }
        });
    }

    pub(super) fn on_event(&mut self, event: &Event, ctx: &Ctx) -> WindowOutcome {
        if !matches!(event, Event::Mouse { .. }) {
            self.status.message = None;
        }
        if let Event::Mouse { offset, position, .. } = event
            && position.checked_sub(*offset).is_some_and(|pos| self.rect.contains(pos) && !self.content().contains(pos))
        {
            return WindowOutcome::Consumed;
        }
        let rect = self.content();
        if let Body::Help(help) = &mut self.body {
            return help.on_event(event, ctx.s, rect);
        }
        if let Body::Files(files) = &mut self.body {
            return files.on_event(event, rect);
        }
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
    /// One window per `WINDOWS` entry, in its order; the `Home::Pane` ones are placed by `panes`.
    pub(super) fn new(log: Arc<LogBuf>, vis: Arc<Vis>, panes: Placement) -> Self {
        let placements = Placements { of: Vec::new(), generation: 0 };
        let mut windows = Self { items: Vec::new(), placements, log, vis };
        for startup in &WINDOWS {
            let placement = match startup.home {
                Home::Tab => Placement::Tabbed,
                Home::Pane => panes,
                Home::Float => Placement::Floating,
            };
            windows.add(startup, placement);
        }
        windows
    }

    fn add(&mut self, startup: &Startup, placement: Placement) -> WindowId {
        let kind = startup.kind;
        let body = match kind {
            Kind::Log => Body::Log(LogPane::new(self.log.clone())),
            Kind::Settings => Body::Settings(SettingsPane::default()),
            Kind::Vis => Body::Vis(self.vis.clone()),
            Kind::Help => Body::Help(HelpPane::default()),
            Kind::Files => Body::Files(FilesPane::new(std::env::current_dir().unwrap_or_else(|_| ".".into()))),
            Kind::List(list) => Body::List(Box::new(TrackList::new(list, startup.keyed_first))),
        };
        self.items.push(Window { kind, body, rect: Rect::from_size((0, 0), (0, 0)), status: StatusRow::default() });
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

    /// The window the `SwitchPlaylists` key switches to: the one that lists the keyed playlists first.
    pub(super) fn keyed_first(&self) -> Option<WindowId> {
        WINDOWS.iter().position(|startup| startup.keyed_first).map(WindowId)
    }

    /// The companion of a startup tab.
    pub(super) fn companion(&self, id: WindowId) -> Option<WindowId> {
        let name = WINDOWS[id.0].name;
        WINDOWS.iter().position(|startup| startup.companion_of == Some(name)).map(WindowId)
    }

    pub(super) fn is_companion(&self, id: WindowId) -> bool {
        WINDOWS[id.0].companion_of.is_some()
    }

    /// Where the placement key moves `id` next; a companion never goes back to the tab bar.
    pub(super) fn next_placement(&self, id: WindowId) -> Placement {
        let at = Placement::CYCLE.iter().position(|&placement| placement == self.placement(id)).unwrap_or(0);
        match Placement::CYCLE[(at + 1) % Placement::CYCLE.len()] {
            Placement::Tabbed if self.is_companion(id) => Placement::Docked,
            next => next,
        }
    }

    /// A startup tab: it stays in the tab bar.
    pub(super) fn is_startup_tab(&self, id: WindowId) -> bool {
        WINDOWS[id.0].home == Home::Tab
    }

    pub(super) fn name(&self, id: WindowId) -> &'static str {
        WINDOWS[id.0].name
    }

    /// The windows `:panes <mode>` without a name places: not a tab now, and not one whose job is to float.
    pub(super) fn panes(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.ids().filter(|&id| self.placement(id) != Placement::Tabbed && WINDOWS[id.0].home != Home::Float)
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
