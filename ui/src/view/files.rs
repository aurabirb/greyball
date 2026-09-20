use std::path::{Path, PathBuf};

use cursive::{Printer, Rect};
use cursive::event::{Event, Key};
use cursive::theme::ColorStyle;

use core::audio;

use super::scroll::{ListEvent, ListState, draw_scrollbar};
use super::text::pad;
use super::window::WindowOutcome;

#[derive(Clone)]
enum Entry {
    Up(PathBuf),
    Dir(PathBuf),
    File(PathBuf),
}

impl Entry {
    fn label(&self) -> String {
        match self {
            Entry::Up(_) => "../".to_string(),
            Entry::Dir(p) => format!("{}/", name_of(p)),
            Entry::File(p) => format!("♪ {}", name_of(p)),
        }
    }
}

/// The file browser window: one directory's subdirectories and audio files.
pub(super) struct FilesPane {
    dir: PathBuf,
    entries: Vec<Entry>,
    readable: bool,
    list: ListState,
}

impl FilesPane {
    pub(super) fn new(start: PathBuf) -> Self {
        let mut pane = Self { dir: PathBuf::new(), entries: Vec::new(), readable: true, list: ListState::default() };
        pane.enter_dir(start);
        pane
    }

    fn enter_dir(&mut self, dir: PathBuf) {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        self.readable = match std::fs::read_dir(&dir) {
            Ok(rd) => {
                for ent in rd.flatten() {
                    let p = ent.path();
                    match ent.file_type() {
                        Ok(ft) if ft.is_dir() => dirs.push(p),
                        Ok(_) if audio::audio_ext(&p.to_string_lossy()) => files.push(p),
                        _ => {}
                    }
                }
                true
            }
            Err(_) => false,
        };
        dirs.sort();
        files.sort();
        self.entries.clear();
        if let Some(parent) = dir.parent() {
            self.entries.push(Entry::Up(parent.to_path_buf()));
        }
        self.entries.extend(dirs.into_iter().map(Entry::Dir));
        self.entries.extend(files.into_iter().map(Entry::File));
        self.list = ListState::default();
        self.dir = dir;
    }

    /// Goes to `target`, leaving the cursor on the directory just left when `target` is its parent.
    fn navigate(&mut self, target: PathBuf) {
        let from = std::mem::replace(&mut self.dir, PathBuf::new());
        self.enter_dir(target);
        if let Some(at) = self.entries.iter().position(|e| matches!(e, Entry::Dir(p) if *p == from)) {
            self.list.cursor = at;
        }
    }

    fn activate(&mut self) -> WindowOutcome {
        match self.entries.get(self.list.cursor).cloned() {
            Some(Entry::Up(p)) => self.navigate(p),
            Some(Entry::Dir(p)) => self.enter_dir(p),
            Some(Entry::File(p)) => return WindowOutcome::AddFile(p),
            None => {}
        }
        WindowOutcome::Consumed
    }

    /// The list rows' rect inside the window's content `rect`: below the path row, left of the scrollbar.
    fn body(rect: Rect) -> Rect {
        Rect::from_size((rect.left(), rect.top() + 1), (rect.width().saturating_sub(1), rect.height().saturating_sub(1)))
    }

    pub(super) fn relayout(&mut self, rect: Rect) {
        self.list.relayout(false, self.entries.len(), Self::body(rect).height());
    }

    pub(super) fn on_event(&mut self, event: &Event, rect: Rect) -> WindowOutcome {
        let body = Self::body(rect);
        if *event == Event::Key(Key::Backspace) {
            if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                self.navigate(parent);
            }
            return WindowOutcome::Consumed;
        }
        match self.list.on_event(event, self.entries.len(), body) {
            ListEvent::Activate | ListEvent::Clicked => self.activate(),
            ListEvent::Moved => WindowOutcome::Consumed,
            ListEvent::Close | ListEvent::Unhandled => WindowOutcome::Ignored,
        }
    }

    pub(super) fn draw(&self, printer: &Printer, focused: bool) {
        let mut title = self.dir.display().to_string();
        if !self.readable {
            title.push_str(" (cannot read)");
        }
        if focused {
            title = format!("[{title}]");
        }
        printer.with_color(ColorStyle::title_secondary(), |p| p.print((0, 0), &pad(&title, p.size.x)));
        let body = Self::body(Rect::from_size((0, 0), printer.size));
        let lines: Vec<String> = self.entries.iter().map(Entry::label).collect();
        self.list.draw(&printer.windowed(body), &lines);
        let gutter = printer.windowed(Rect::from_size((0, 1), (printer.size.x, body.height())));
        draw_scrollbar(&gutter, body.width(), body.height(), self.list.offset, lines.len(), None);
    }

    pub(super) fn idle(&self) -> &'static str {
        "[Enter] open dir / add file   [Bksp] up"
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}
