use std::sync::Arc;

use cursive::{Printer, Rect};
use cursive::event::{Event, Key, MouseButton, MouseEvent};
use cursive::theme::ColorStyle;

use core::{HotkeyTarget, Session};

use crate::items::{self, ITEMS, Section};
use crate::screen::Kind;

use super::memo::Memo;
use super::scroll::{LIST_JUMP_STEP, Nav, bound_offset, draw_scrollbar};
use super::text::{pad, pad_right_aligned, wrap};
use super::track_list::hotkey_target_name;
use super::window::WindowOutcome;

/// Blank columns between two lanes.
const GAP: usize = 2;

/// What Enter on a row can rebind, or why it can't.
enum Target {
    Bindable(HotkeyTarget),
    Refused(&'static str),
}

/// One item: its cursor stop is `first`, the line its summary starts on.
struct Row {
    first: usize,
    /// One past its last line.
    end: usize,
    name: String,
    target: Target,
}

enum Line {
    Blank,
    Title(String),
    /// The first line of `rows[_]`, the only kind the cursor stops on.
    First(usize, String),
    Rest(String),
}

/// The laid-out content for one width: a flat line list plus each item's place in it.
pub(super) struct Built {
    lines: Vec<Line>,
    rows: Vec<Row>,
    /// Each section's title line and first row.
    sections: Vec<(usize, usize)>,
}

/// An item before layout: its three cells.
struct Cells {
    command: String,
    summary: String,
    detail: String,
    shortcut: String,
    target: Target,
}

fn key_label(key: char) -> String {
    if key == ' ' { "Space".to_string() } else { key.to_string() }
}

fn table_cells(section: Section, s: &Session) -> Vec<Cells> {
    let mut cells: Vec<Cells> = ITEMS
        .iter()
        .filter(|item| item.section == section)
        .map(|item| {
            let (shortcut, target) = match item.key {
                items::Key::Builtin(action) => {
                    let target = HotkeyTarget::Builtin(action);
                    (s.effective_hotkey(&target).map(key_label).unwrap_or_default(), Target::Bindable(target))
                }
                items::Key::Fixed(keys) => (keys.to_string(), Target::Refused("a fixed key can't be rebound")),
                items::Key::None if item.args.is_empty() => (String::new(), Target::Refused("this command has no key")),
                items::Key::None => (String::new(), Target::Refused("a command that takes an argument can't have a key")),
            };
            Cells { command: item.command(), summary: item.summary.to_string(), detail: item.detail.to_string(), shortcut, target }
        })
        .collect();
    if section == Section::Commands {
        cells.extend(s.plugin_command_help().into_iter().map(|(word, help)| Cells {
            command: format!(":{word}"),
            summary: help,
            detail: String::new(),
            shortcut: String::new(),
            target: Target::Refused("a plugin command can't have a key"),
        }));
    }
    cells
}

/// The playlists that have a key, as rebindable rows.
fn playlist_cells(s: &Session) -> Vec<Cells> {
    let mut keyed: Vec<(char, Cells)> = s
        .hotkeys()
        .into_iter()
        .filter_map(|(key, target)| {
            if matches!(target, HotkeyTarget::Builtin(_)) {
                return None;
            }
            let cells = Cells {
                command: String::new(),
                summary: hotkey_target_name(s, &target),
                detail: String::new(),
                shortcut: key_label(key),
                target: Target::Bindable(target),
            };
            Some((key, cells))
        })
        .collect();
    keyed.sort_by_key(|&(key, _)| key);
    keyed.into_iter().map(|(_, cells)| cells).collect()
}

fn build(width: usize, s: &Session) -> Built {
    let mut built = Built { lines: Vec::new(), rows: Vec::new(), sections: Vec::new() };
    let titled = Section::ALL.iter().map(|&section| (section.title(), table_cells(section, s)));
    let sections = titled.chain(std::iter::once(("Playlist keys", playlist_cells(s))));
    for (title, cells) in sections.filter(|(_, cells)| !cells.is_empty()) {
        // Lane widths come from the section's widest cells, the command lane capped so the description keeps room.
        let widest = |cell: fn(&Cells) -> &String| cells.iter().map(|c| cell(c).chars().count()).max().unwrap_or(0);
        let shortcut_w = widest(|c| &c.shortcut).min(width / 4);
        let command_w = widest(|c| &c.command).min(width * 2 / 5);
        let lane = |w: usize| if w == 0 { 0 } else { w + GAP };
        let text_w = width.saturating_sub(lane(command_w) + lane(shortcut_w)).max(1);
        if !built.lines.is_empty() {
            built.lines.push(Line::Blank);
        }
        built.sections.push((built.lines.len(), built.rows.len()));
        built.lines.push(Line::Title(format!("[ {title} ]")));
        for cell in cells {
            built.lines.push(Line::Blank);
            let command = if command_w == 0 { Vec::new() } else { wrap(&cell.command, command_w) };
            let mut text = wrap(&cell.summary, text_w);
            if !cell.detail.is_empty() {
                text.extend(wrap(&cell.detail, text_w));
            }
            let first = built.lines.len();
            for i in 0..command.len().max(text.len()) {
                let part = |lines: &[String]| lines.get(i).cloned().unwrap_or_default();
                let mut line = String::new();
                if command_w > 0 {
                    line.push_str(&pad(&part(&command), lane(command_w)));
                }
                line.push_str(&pad(&part(&text), text_w));
                if i == 0 && shortcut_w > 0 {
                    line.push_str(&pad_right_aligned(&cell.shortcut, lane(shortcut_w)));
                }
                built.lines.push(if i == 0 { Line::First(built.rows.len(), line) } else { Line::Rest(line) });
            }
            built.rows.push(Row { first, end: built.lines.len(), name: cell.summary, target: cell.target });
        }
    }
    built
}

/// The help window, which is also where a key is rebound: an item cursor over a line-scrolled view.
#[derive(Default)]
pub(super) struct HelpPane {
    /// Index into `Built::rows`.
    cursor: usize,
    /// First visible line.
    offset: usize,
    /// The row awaiting its new key: its name and what the key will bind.
    capturing: Option<(String, HotkeyTarget)>,
    built: Memo<LayoutKey, Arc<Built>>,
    /// The layout and body height the scroll window was last fitted to.
    fitted: Memo<(LayoutKey, usize)>,
}

/// Body width, then the hotkeys, playlists and remote-playlists generations.
type LayoutKey = (usize, u64, u64, u64);

impl HelpPane {
    /// `rect` minus the title row and the scrollbar gutter.
    fn body(rect: Rect) -> Rect {
        Rect::from_size((rect.left(), rect.top() + 1), (rect.width().saturating_sub(1), rect.height().saturating_sub(1)))
    }

    /// Everything the layout shows that can change: the width, the keys, the playlist names.
    fn key(s: &Session, rect: Rect) -> LayoutKey {
        (Self::body(rect).width(), s.hotkeys_gen(), s.playlists_gen(), s.remote_playlists_gen())
    }

    pub(super) fn built(&self, s: &Session, rect: Rect) -> Arc<Built> {
        let key = Self::key(s, rect);
        self.built.get_or_build(key, || Arc::new(build(key.0, s)))
    }

    /// Keeps the cursor on a row and the whole item under it in view.
    fn follow(&mut self, built: &Built, view_h: usize) {
        self.cursor = self.cursor.min(built.rows.len().saturating_sub(1));
        if let Some(row) = built.rows.get(self.cursor) {
            if row.end > self.offset + view_h {
                self.offset = row.end.saturating_sub(view_h);
            }
            self.offset = self.offset.min(row.first);
        }
        self.offset = bound_offset(self.offset, built.lines.len(), view_h);
    }

    pub(super) fn relayout(&mut self, rect: Rect, s: &Session) {
        let (built, view_h) = (self.built(s, rect), Self::body(rect).height());
        // A rebuilt layout moved the lines under the cursor, which a wheel scroll otherwise leaves alone.
        if self.fitted.changed((Self::key(s, rect), view_h)) {
            self.follow(&built, view_h);
        }
        self.cursor = self.cursor.min(built.rows.len().saturating_sub(1));
        self.offset = bound_offset(self.offset, built.lines.len(), view_h);
    }

    /// A pending capture doesn't outlive the focus it was started under.
    pub(super) fn blur(&mut self) {
        self.capturing = None;
    }

    /// The hint row's text while this window has focus; only `over` the view are Tab and Esc its own.
    pub(super) fn hint(&self, over: bool) -> String {
        match &self.capturing {
            Some((name, _)) => format!("press a key for {name:?}   [Esc] cancel"),
            None if over => "[Enter] new key   [Backspace] default   [Tab/Shift-Tab] section   [?/Esc] leave".to_string(),
            None => "[Enter] new key   [Backspace] default   [Tab] next window   [?] leave".to_string(),
        }
    }

    /// Moves to the next or previous section: cursor on its first item, its title at the top.
    fn jump_section(&mut self, back: bool, built: &Built, view_h: usize) {
        let at = built.sections.iter().rposition(|&(_, row)| row <= self.cursor).unwrap_or(0);
        let next = if back { at.saturating_sub(1) } else { (at + 1).min(built.sections.len().saturating_sub(1)) };
        if let Some(&(title, row)) = built.sections.get(next) {
            self.cursor = row;
            self.offset = bound_offset(title, built.lines.len(), view_h);
        }
    }

    pub(super) fn on_event(&mut self, event: &Event, s: &Session, rect: Rect) -> WindowOutcome {
        let (built, body) = (self.built(s, rect), Self::body(rect));
        let view_h = body.height();
        if let Event::Mouse { offset, position, event: mouse } = event {
            let Some(pos) = position.checked_sub(*offset).filter(|&pos| rect.contains(pos)) else {
                return WindowOutcome::Ignored;
            };
            self.capturing = None;
            match (mouse, Nav::of(event)) {
                (_, Some(nav)) => {
                    let (up, step) = nav.step(LIST_JUMP_STEP);
                    let offset = if up { self.offset.saturating_sub(step) } else { self.offset + step };
                    self.offset = bound_offset(offset, built.lines.len(), view_h);
                }
                (MouseEvent::Press(MouseButton::Left), _) if body.contains(pos) => {
                    if let Some(Line::First(row, _)) = built.lines.get(self.offset + pos.y - body.top()) {
                        self.cursor = *row;
                    }
                }
                _ => {}
            }
            return WindowOutcome::Consumed;
        }
        // While a row awaits its key every key is its answer, so none reaches the shell or the tab beneath.
        if let Some((_, target)) = self.capturing.take() {
            return match *event {
                Event::Char(key) => WindowOutcome::Bind(target, key),
                _ => WindowOutcome::Flash("no key bound".to_string()),
            };
        }
        let Some(row) = built.rows.get(self.cursor) else { return WindowOutcome::Ignored };
        if let Some(nav) = Nav::of(event) {
            let (up, step) = nav.step(LIST_JUMP_STEP);
            let last = built.rows.len().saturating_sub(1);
            self.cursor = if up { self.cursor.saturating_sub(step) } else { self.cursor.saturating_add(step).min(last) };
            self.follow(&built, view_h);
            return WindowOutcome::Consumed;
        }
        match (event, &row.target) {
            (Event::Key(Key::Tab), _) => self.jump_section(false, &built, view_h),
            (Event::Shift(Key::Tab), _) => self.jump_section(true, &built, view_h),
            // The target is taken now, so a rebuild before the key arrives can't change what it binds.
            (Event::Key(Key::Enter), Target::Bindable(target)) => self.capturing = Some((row.name.clone(), target.clone())),
            (Event::Key(Key::Enter), Target::Refused(why)) => return WindowOutcome::Flash(format!("{}: {why}", row.name)),
            (Event::Key(Key::Backspace), Target::Bindable(target)) => return WindowOutcome::Unbind(target.clone()),
            _ => return WindowOutcome::Ignored,
        }
        WindowOutcome::Consumed
    }

    pub(super) fn draw(&self, printer: &Printer, focused: bool, built: &Built) {
        let title = if focused { format!("[{}]", Kind::Help.label()) } else { Kind::Help.label().to_string() };
        printer.with_color(ColorStyle::title_secondary(), |p| p.print((0, 0), &pad(&title, p.size.x)));
        let body = Rect::from_size((0, 1), (printer.size.x.saturating_sub(1), printer.size.y.saturating_sub(1)));
        let view = printer.windowed(body);
        let offset = bound_offset(self.offset, built.lines.len(), body.height());
        for (y, line) in built.lines.iter().skip(offset).take(body.height()).enumerate() {
            match line {
                Line::Blank => {}
                Line::Title(text) => view.with_color(ColorStyle::title_primary(), |p| p.print((0, y), text)),
                Line::First(i, text) if *i == self.cursor => {
                    view.with_color(ColorStyle::highlight(), |p| p.print((0, y), &pad(text, body.width())));
                }
                Line::First(_, text) | Line::Rest(text) => view.print((0, y), text),
            }
        }
        draw_scrollbar(&printer.windowed(Rect::from_size((0, 1), (printer.size.x, body.height()))), body.width(), body.height(), offset, built.lines.len());
    }
}
