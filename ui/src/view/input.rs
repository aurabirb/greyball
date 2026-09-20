use std::sync::Arc;
use std::thread;

use cursive::Cursive;
use cursive::event::{Event, EventResult, Key, MouseEvent};
use cursive::view::Nameable;
use cursive::views::{Dialog, OnEventView, ScrollView, TextView};

use core::{Command, CoreEvent, Dispatch, HotkeyTarget, PlaylistId, Plugin, SourceId};

use crate::command;
use crate::keybindings::{self, Action};
use crate::screen::{Kind, ListKind};

use super::MedleyView;
use super::modal::Modal;
use super::notice::Notice;
use super::panes::PANE_LAYOUT_CYCLE;
use super::playlist_picker::PlaylistPicker;
use super::track_list::TrackList;
use super::window::WindowId;

#[derive(Clone, PartialEq)]
pub(super) enum Editing {
    None,
    /// Typing this Search window's query in its title row.
    Search(WindowId),
    CommandLine,
    /// Collecting the answers to a warnings-panel plugin's `setup_prompt`s, one per Enter.
    PluginSetup(SourceId, Vec<String>),
    /// Typing the media cache directory (Settings).
    CacheDir,
    /// Screen-local fuzzy filter (`/` on any track-list screen other than Search itself).
    Filter,
}

/// The open notice dialog's text view, which further popup notices append to.
const NOTICE_TEXT: &str = "notice";

/// A Yes/No dialog over everything, so no key gets past it: Enter or `y` runs `yes` on the shell, Esc or `n` cancels.
pub(super) fn confirm(
    title: &'static str,
    question: String,
    button: &'static str,
    yes: impl Fn(&mut MedleyView) -> EventResult + Clone + Send + Sync + 'static,
) -> EventResult {
    EventResult::with_cb(move |c: &mut Cursive| {
        let yes = yes.clone();
        let run = move |c: &mut Cursive| {
            c.pop_layer();
            crate::on_root(c, yes.clone());
        };
        let cancel = |c: &mut Cursive| {
            c.pop_layer();
        };
        let dialog = Dialog::text(question.clone()).title(title).button(button, run.clone()).dismiss_button("Cancel");
        c.add_layer(OnEventView::new(dialog).on_event('y', run).on_event('n', cancel).on_event(Key::Esc, cancel));
    })
}

impl MedleyView {
    /// Run a plugin-registered `:`-command (e.g. `:spotify addlogin`) on a background thread.
    fn run_plugin_command(&self, plugin: Arc<dyn Plugin>, word: String, arg: Option<String>) -> EventResult {
        let bus = self.with_session(|s| s.bus.clone());
        thread::spawn(move || {
            let msg = plugin.run_command(&word, arg);
            bus.send(CoreEvent::PluginStatusChanged);
            bus.send(CoreEvent::PluginReport(msg));
        });
        EventResult::consumed()
    }

    pub(super) fn commit_edit(&mut self) -> EventResult {
        let kind = std::mem::replace(&mut self.editing, Editing::None);
        let text = std::mem::take(&mut self.buffer);
        match kind {
            Editing::Search(id) => {
                if let Some(list) = self.windows[id].list_mut() {
                    list.set_input(None);
                }
                self.start_search(id, &text);
                EventResult::consumed()
            }
            Editing::CommandLine => {
                let parsed = match command::parse(&text) {
                    Ok(p) => p,
                    Err(e) => return self.notify(Notice::failed(e)),
                };
                let open = self.active_list().and_then(TrackList::open_local);
                if let command::Parsed::ToggleWindow(name) = parsed {
                    return self.handle_action(Action::ToggleWindow(name));
                }
                if let command::Parsed::Search(query) = parsed {
                    if let Some(id) = self.search_window() {
                        self.show(id);
                        self.start_search(id, &query);
                    }
                    return EventResult::consumed();
                }
                if let command::Parsed::Builtin(action) = parsed {
                    let (selected, collection) = self.with_session(|s| {
                        let list = self.active_list();
                        (list.and_then(|list| list.selected_track(s)), list.and_then(|list| list.selected_collection(s)))
                    });
                    return match keybindings::builtin_action(action, selected, collection) {
                        Action::None => self.notify(Notice::failed("no track selected")),
                        action => self.handle_action(action),
                    };
                }
                if let command::Parsed::SetPaneLayout(patch) = parsed {
                    // `side`/`stack` are shared dock geometry whatever window is named.
                    if let Some(side) = patch.side {
                        self.pane_cfg.side = side;
                    }
                    if let Some(stack) = patch.stack {
                        self.pane_cfg.stack = stack;
                    }
                    if let Some(placement) = patch.mode {
                        let ids: Vec<_> = match patch.window {
                            Some(name) => self.windows.named(name).into_iter().collect(),
                            None => self.windows.panes().collect(),
                        };
                        for id in ids {
                            self.set_placement(id, placement, false);
                        }
                    }
                    return EventResult::consumed();
                }
                if parsed == command::Parsed::OpenBrowse {
                    if let Some(id) = self.windows.named(crate::screen::FILES) {
                        self.show(id);
                    }
                    return EventResult::consumed();
                }
                if let command::Parsed::Open(arg) = parsed {
                    return self.open_arg(arg, open);
                }
                if let command::Parsed::PluginCommand { word, arg } = parsed {
                    let plugin = self.with_session(|s| s.plugin_for_command(&word));
                    return match plugin {
                        Some(plugin) => self.run_plugin_command(plugin, word, arg),
                        None => self.notify(Notice::failed(format!("unknown command: {word}"))),
                    };
                }
                let cmd = self.with_session(|s| {
                    let sel = self.active_list().and_then(|list| list.selected_track(s));
                    command::resolve(parsed, s, sel)
                });
                match cmd {
                    Ok(c) => self.run(c),
                    Err(e) => self.notify(Notice::failed(e)),
                }
            }
            Editing::PluginSetup(id, mut answers) => {
                answers.push(text);
                let more = self.with_session(|s| s.plugin(&id).and_then(|p| p.setup_prompt(&answers))).is_some();
                if more {
                    self.editing = Editing::PluginSetup(id, answers);
                } else {
                    self.run_plugin_setup(id, answers);
                }
                EventResult::consumed()
            }
            Editing::CacheDir => match self.with_session_mut(|s| s.set_media_cache_dir(&text)) {
                Ok(dir) => self.notify(Notice::Flash(format!("cache dir {}: restart to move and use it", core::tilde(&dir)))),
                Err(e) => self.notify(Notice::failed(e)),
            },
            Editing::Filter => {
                self.set_filter(Some(text.as_str()).filter(|t| !t.trim().is_empty()));
                EventResult::consumed()
            }
            Editing::None => EventResult::Ignored,
        }
    }

    pub(crate) fn run(&mut self, cmd: Command) -> EventResult {
        match self.with_session_mut(|s| s.dispatch(cmd)) {
            Ok(Dispatch::Quit) => EventResult::with_cb(|c: &mut Cursive| c.quit()),
            Ok(Dispatch::PlaylistCreated(id)) => {
                for window in self.windows.ids() {
                    if let Some(list) = self.windows[window].list_mut() {
                        list.select_playlist(HotkeyTarget::Local(id));
                    }
                }
                EventResult::consumed()
            }
            result => Notice::of_dispatch(result).map_or_else(EventResult::consumed, |n| self.notify(n)),
        }
    }

    /// The Search window a search goes to: the active list when it is one, else a shown one, else the tab.
    pub(super) fn search_window(&self) -> Option<WindowId> {
        let is_search = |id: &WindowId| self.windows[*id].kind == Kind::List(ListKind::Search);
        Some(self.active_list_id())
            .filter(is_search)
            .or_else(|| self.visible().into_iter().find(is_search))
            .or_else(|| self.windows.named("search"))
    }

    /// Searches `text` from window `id`, dropping what it listed; blank text only drops.
    fn start_search(&mut self, id: WindowId, text: &str) {
        let Some(previous) = self.windows[id].list().map(TrackList::search) else { return };
        let started = self.with_session_mut(|s| s.search(text, previous));
        if let Some(list) = self.windows[id].list_mut() {
            list.set_search(started);
        }
    }

    pub(crate) fn seek(&mut self, ms: i64) -> EventResult {
        let result = self.run(Command::Seek(ms));
        self.reveal_playing(false);
        result
    }

    /// Cursor onto the playing track in the active list; only the hotkey (`announce`) says when it can't.
    pub(crate) fn reveal_playing(&mut self, announce: bool) -> EventResult {
        let id = self.active_list_id();
        let session = self.session.clone();
        let found = {
            let s = session.lock().unwrap();
            match s.now_playing_id() {
                None => Err(Notice::nothing_playing()),
                Some(_) => match self.windows[id].list_mut().is_some_and(|list| list.reveal(&s)) {
                    true => Ok(()),
                    false => Err(Notice::not_in_list()),
                },
            }
        };
        match found {
            Ok(()) => {
                self.clamp_scroll();
                EventResult::consumed()
            }
            Err(notice) if announce => self.notify(notice),
            Err(_) => EventResult::consumed(),
        }
    }

    pub(crate) fn notify(&mut self, notice: Notice) -> EventResult {
        match notice {
            Notice::Flash(text) => {
                self.set_flash(text);
                EventResult::consumed()
            }
            Notice::Status { text, refused } => {
                let target = self.status_id();
                self.windows[target].set_status(&text, refused);
                EventResult::consumed()
            }
            // One notice dialog at most: a batch of failures reads as one list, dismissed once.
            Notice::Popup(msg) => EventResult::with_cb(move |c: &mut Cursive| {
                if c.call_on_name(NOTICE_TEXT, |text: &mut TextView| text.append(format!("\n\n{msg}"))).is_none() {
                    let text = ScrollView::new(TextView::new(msg.clone()).with_name(NOTICE_TEXT));
                    c.add_layer(Dialog::around(text).dismiss_button("Ok"));
                }
            }),
        }
    }

    /// Runs `cmd`, asking first when it would remove a track from a playlist or Liked Songs.
    pub(super) fn run_confirmed(&mut self, cmd: Command) -> EventResult {
        match self.with_session(|s| s.removal_prompt(&cmd)) {
            Some(question) => confirm("Remove", question, "Remove", move |view| view.run(cmd.clone())),
            None => self.run(cmd),
        }
    }

    pub(super) fn handle_action(&mut self, action: Action) -> EventResult {
        match action {
            Action::Command(c) => self.run_confirmed(c),
            // A list that can't be filtered locally sends `/` to a Search window's query instead.
            Action::FocusSearch => {
                let id = self.active_list_id();
                if self.windows[id].list().is_some_and(|list| !list.is_results()) {
                    self.editing = Editing::Filter;
                    self.buffer.clear();
                } else if let Some(id) = self.search_window() {
                    self.show(id);
                    self.edit_search(id);
                }
                EventResult::consumed()
            }
            Action::Seek(ms) => self.seek(ms),
            Action::RevealPlaying => self.reveal_playing(true),
            Action::LikePlaying => match self.with_session(|s| s.now_playing_id()) {
                Some(id) => self.run_confirmed(Command::Like(id)),
                None => self.notify(Notice::nothing_playing()),
            },
            Action::ToggleWindow(name) => {
                let Some(id) = self.windows.named(name) else { return EventResult::Ignored };
                self.toggle_window(id);
                if self.visible().contains(&id) {
                    self.edit_search(id);
                }
                EventResult::consumed()
            }
            Action::ShowWindow(name) => {
                let Some(id) = self.windows.named(name) else { return EventResult::Ignored };
                self.show(id);
                self.edit_search(id);
                EventResult::consumed()
            }
            Action::CommandLine => {
                self.editing = Editing::CommandLine;
                self.buffer.clear();
                EventResult::consumed()
            }
            Action::Tab(n) => {
                if let Some(&id) = self.tabs.get(n) {
                    self.show(id);
                    self.edit_search(id);
                }
                EventResult::consumed()
            }
            Action::CycleKindFilter => {
                let id = self.active_list_id();
                if let Some(list) = self.windows[id].list_mut() {
                    list.cycle_kinds();
                }
                EventResult::consumed()
            }
            Action::CyclePlacement => {
                self.cycle_placement();
                EventResult::consumed()
            }
            Action::SwitchPlaylists => {
                self.switch_playlists();
                EventResult::consumed()
            }
            Action::OpenHelp => {
                self.toggle_help();
                EventResult::consumed()
            }
            Action::AddToPlaylistPrompt(id) => {
                self.modal = Some(Modal::Picker(self.with_session(|s| PlaylistPicker::new(id, s))));
                EventResult::consumed()
            }
            Action::Prompt(name) => {
                self.editing = Editing::CommandLine;
                self.buffer = format!("{name} ");
                EventResult::consumed()
            }
            Action::ExportPlaylist => {
                let target = self.with_session(|s| self.active_list().and_then(|list| list.selected_hotkey_target(s)));
                match target {
                    Some(HotkeyTarget::Local(id)) => self.run(Command::ExportM3u(id)),
                    _ => EventResult::Ignored,
                }
            }
            Action::CyclePaneLayout => {
                let cur = (self.pane_cfg.side, self.pane_cfg.stack);
                let next = PANE_LAYOUT_CYCLE.iter().position(|&c| c == cur).map_or(0, |i| (i + 1) % PANE_LAYOUT_CYCLE.len());
                (self.pane_cfg.side, self.pane_cfg.stack) = PANE_LAYOUT_CYCLE[next];
                EventResult::consumed()
            }
            Action::None => EventResult::Ignored,
        }
    }

    /// The `/`-filter of the list being filtered: the active one, as focus can't move while typing.
    fn set_filter(&mut self, query: Option<&str>) {
        let id = self.active_list_id();
        if let Some(list) = self.windows[id].list_mut() {
            list.set_query(query);
        }
    }

    /// Takes the query input of Search window `id`, typed in its title row; any other window is left alone.
    pub(super) fn edit_search(&mut self, id: WindowId) {
        if self.windows[id].kind == Kind::List(ListKind::Search) {
            self.editing = Editing::Search(id);
            self.buffer.clear();
            self.write_search_input(id);
        }
    }

    fn write_search_input(&mut self, id: WindowId) {
        if let Some(list) = self.windows[id].list_mut() {
            list.set_input(Some(&self.buffer));
        }
    }

    /// Drops the text being typed; a cancelled filter shows the full list again.
    fn cancel_edit(&mut self) {
        match self.editing {
            Editing::Filter => self.set_filter(None),
            Editing::Search(id) => {
                if let Some(list) = self.windows[id].list_mut() {
                    list.set_input(None);
                }
            }
            _ => {}
        }
        self.editing = Editing::None;
        self.buffer.clear();
    }

    /// The active text field captures every event; `None` when idle, or when `event` cancels it and still needs handling.
    pub(super) fn on_edit_event(&mut self, event: &Event) -> Option<EventResult> {
        if self.editing == Editing::None {
            return None;
        }
        let outside_click = matches!(event, Event::Mouse { event: MouseEvent::Press(_), .. });
        let search = matches!(self.editing, Editing::Search(_));
        // A digit as Search's first keystroke is a screen switch, not a query.
        let leading_digit = search && self.buffer.is_empty() && matches!(event, Event::Char(c) if c.is_ascii_digit());
        if outside_click || leading_digit {
            self.cancel_edit();
            return None;
        }
        let edited = match event {
            // The `/` that opened the query is not part of it.
            Event::Char('/') if search && self.buffer.is_empty() => false,
            Event::Char(c) => {
                self.buffer.push(*c);
                true
            }
            Event::Key(Key::Backspace) => {
                self.buffer.pop();
                true
            }
            Event::Key(Key::Esc) => {
                self.cancel_edit();
                false
            }
            Event::Key(Key::Enter) => return Some(self.commit_edit()),
            _ => false,
        };
        // The filter narrows, and the search window's title follows, live as you type.
        match self.editing {
            Editing::Filter if edited => {
                let query = self.buffer.clone();
                self.set_filter(Some(&query));
            }
            Editing::Search(id) if edited => self.write_search_input(id),
            _ => {}
        }
        Some(EventResult::consumed())
    }

    /// The text being typed in the corner slot, prompt included; a search query is typed in its window's title row instead.
    pub(super) fn input_line(&self) -> Option<String> {
        match &self.editing {
            Editing::Filter => Some(format!("/{}", self.buffer)),
            Editing::CommandLine => Some(format!(":{}", self.buffer)),
            Editing::PluginSetup(..) => Some(format!("> {}", self.buffer)),
            Editing::CacheDir => Some(format!("cache dir> {}", self.buffer)),
            Editing::None | Editing::Search(_) => None,
        }
    }

    /// `:open <url>`: a browsable link opens as a remote list, any other recognized URL is added as a track.
    fn open_playlist_uri(&mut self, uri: String, open: Option<PlaylistId>, browse: Option<(core::SourceId, core::BrowseNode)>) -> EventResult {
        match browse {
            Some((sid, node)) => {
                let name = match &node {
                    core::BrowseNode::Path(id) => id.clone(),
                    core::BrowseNode::Root => String::new(),
                };
                if let Some(id) = self.windows.named("playlists") {
                    self.show(id);
                    if let Some(list) = self.windows[id].list_mut() {
                        list.open_remote(sid, name, node);
                    }
                }
                EventResult::consumed()
            }
            None => self.run(Command::AddUrl { url: uri, playlist: open }),
        }
    }

    /// `:open <url-or-path>`'s argument case.
    fn open_arg(&mut self, arg: String, open: Option<PlaylistId>) -> EventResult {
        let url = arg.trim().to_string();
        let found = self.with_session(|s| {
            let source = s.source_for_uri(&url)?;
            Some(source.browse_uri(&url).map(|node| (source.id(), node)))
        });
        if let Some(browse) = found {
            return self.open_playlist_uri(url, open, browse);
        }
        // A URL no source claims is not a path to add.
        if arg.contains("://") && !arg.starts_with("file://") {
            return self.notify(Notice::failed(format!("no source can open {arg:?}")));
        }
        let lower = arg.to_ascii_lowercase();
        if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
            return self.run(Command::ImportM3u(std::path::PathBuf::from(arg.trim())));
        }
        let paths = command::split_paths(&arg);
        self.run(Command::AddFilesToPlaylist { playlist: open, paths })
    }

}
