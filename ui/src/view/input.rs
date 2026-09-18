use std::sync::Arc;
use std::thread;

use cursive::Cursive;
use cursive::event::{Event, EventResult, Key, MouseEvent};
use cursive::views::Dialog;

use core::{Command, CoreEvent, Dispatch, HotkeyTarget, Plugin, SourceId, TrackId};

use crate::command::{self, Pane};
use crate::keybindings::Action;

use super::{HIST, MedleyView, PLAYLISTS, SEARCH};
use super::help::HelpModal;
use super::panes::PANE_LAYOUT_CYCLE;
use super::playlist_picker::PlaylistPicker;

#[derive(Clone, PartialEq)]
pub(super) enum Editing {
    None,
    Search,
    CommandLine,
    /// Collecting a `SetupKind::TextInput` value for the warnings-panel plugin selected.
    PluginSetup(SourceId),
    /// Screen-local fuzzy filter (`/` on any track-list screen other than Search itself).
    Filter,
}

pub(super) fn popup(msg: impl Into<String>) -> EventResult {
    let msg = msg.into();
    EventResult::with_cb(move |c: &mut Cursive| {
        c.add_layer(Dialog::info(msg.clone()));
    })
}

pub(super) fn key_name(event: &Event) -> Option<String> {
    match event {
        Event::Char(' ') => Some("Space".to_string()),
        Event::Char(c) => Some(c.to_string()),
        Event::Key(Key::Enter) => Some("Enter".to_string()),
        _ => None,
    }
}

impl MedleyView {
    /// Run a plugin-registered `:`-command (e.g. `:spotify addlogin`) on a background thread.
    fn run_plugin_command(&self, plugin: Arc<dyn Plugin>, word: String, arg: Option<String>) -> EventResult {
        let feedback = self.with_session(|s| s.plugin_command_result_handle());
        let bus = self.with_session(|s| s.bus.clone());
        thread::spawn(move || {
            let msg = plugin.run_command(&word, arg);
            bus.send(CoreEvent::PluginStatusChanged);
            *feedback.lock().unwrap() = Some(msg);
            bus.send(CoreEvent::PluginCommandResult);
        });
        EventResult::consumed()
    }

    pub(super) fn commit_edit(&mut self) -> EventResult {
        let kind = std::mem::replace(&mut self.editing, Editing::None);
        let text = std::mem::take(&mut self.buffer);
        match kind {
            Editing::Search => {
                self.last_query = if text.trim().is_empty() { None } else { Some(text.clone()) };
                self.run(Command::Search(text))
            }
            Editing::CommandLine => {
                let parsed = match command::parse(&text) {
                    Ok(p) => p,
                    Err(e) => return popup(e),
                };
                let open = self.playlists.open;
                if parsed == command::Parsed::Help {
                    self.help = Some(HelpModal::new(self.help_lines()));
                    return EventResult::consumed();
                }
                // `Screen` mode: fullscreen, one at a time.
                if let command::Parsed::TogglePane(pane) = parsed {
                    self.toggle_pane(pane);
                    return self.vis_fps_cb();
                }
                if command::Parsed::Vis == parsed {
                    self.toggle_pane(Pane::Vis);
                    return self.vis_fps_cb();
                }
                if command::Parsed::History == parsed {
                    self.screen = HIST;
                    self.playlists.leave();
                    self.filter.query = None;
                    self.clamp_scroll();
                    return EventResult::consumed();
                }
                if command::Parsed::Keys == parsed {
                    self.open_hotkey_menu();
                    return EventResult::consumed();
                }
                if let command::Parsed::SetPaneLayout(patch) = parsed {
                    // `side`/`stack` stay shared layout geometry regardless of `patch.pane`.
                    if let Some(side) = patch.side {
                        self.panes.cfg.side = side;
                    }
                    if let Some(stack) = patch.stack {
                        self.panes.cfg.stack = stack;
                    }
                    if let Some(mode) = patch.mode {
                        match patch.pane {
                            Some(pane) => {
                                self.panes.mode_overrides.insert(pane, mode);
                            }
                            None => self.panes.cfg.mode = mode,
                        }
                    }
                    self.clamp_focus();
                    return EventResult::consumed();
                }
                if parsed == command::Parsed::OpenBrowse {
                    let session = self.session.clone();
                    let start =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    return EventResult::with_cb(move |siv| {
                        crate::filebrowser::open(siv, session.clone(), open, start.clone());
                    });
                }
                if let command::Parsed::Open(arg) = parsed {
                    return self.open_arg(arg, open);
                }
                if let command::Parsed::PluginCommand { word, arg } = parsed {
                    let plugin = self.with_session(|s| s.plugin_for_command(&word));
                    return match plugin {
                        Some(plugin) => self.run_plugin_command(plugin, word, arg),
                        None => popup(format!("unknown command: {word}")),
                    };
                }
                let cmd = self.with_session(|s| {
                    let sel = self.selected_track(s, self.active_screen());
                    command::resolve(parsed, s, sel)
                });
                match cmd {
                    Ok(c) => self.run(c),
                    Err(e) => popup(e),
                }
            }
            Editing::PluginSetup(id) => {
                self.run_plugin_setup(id, Some(text));
                EventResult::consumed()
            }
            Editing::Filter => {
                self.filter.query = if text.trim().is_empty() { None } else { Some(text) };
                EventResult::consumed()
            }
            Editing::None => EventResult::Ignored,
        }
    }

    pub(super) fn run(&mut self, cmd: Command) -> EventResult {
        // `Previous` only grows the queue when it actually wedges the just-played track back onto the front.
        let tracks_queue_len = matches!(cmd, Command::Enqueue(_) | Command::Wedge(_) | Command::Previous)
            .then(|| self.with_session(|s| s.queue_len()));
        let feedback_kind = match &cmd {
            Command::Enqueue(_) => Some("Queued"),
            Command::Wedge(_) => Some("Wedged"),
            Command::Previous => Some("Wedged"),
            _ => None,
        };
        let is_toggle_shuffle = matches!(cmd, Command::ToggleShuffle);
        let is_toggle_scan = matches!(cmd, Command::ToggleScan);
        let res = self.with_session_mut(|s| s.dispatch(cmd));
        if matches!(res, Ok(Dispatch::Ok)) {
            if let (Some(before), Some(kind)) = (tracks_queue_len, feedback_kind) {
                let after = self.with_session(|s| s.queue_len());
                if kind == "Queued" || after > before {
                    self.queue_feedback = Some(format!("  {kind}: {after} tracks"));
                }
            }
            // Flash feedback for the two clickable status-line tags.
            if is_toggle_shuffle {
                let on = self.with_session(|s| s.shuffle());
                self.queue_feedback = Some(format!("  Shuffle: {}", if on { "on" } else { "off" }));
            } else if is_toggle_scan {
                let label = self.with_session(|s| s.scan.as_ref().map(|scan| scan.mode()));
                let label = match label {
                    Some(core::ScanMode::Active) => "active",
                    Some(core::ScanMode::CacheOnly) => "cache-only",
                    Some(core::ScanMode::Disabled) | None => "off",
                };
                self.queue_feedback = Some(format!("  Scan: {label}"));
            }
        }
        match res {
            Ok(Dispatch::Ok) => EventResult::consumed(),
            Ok(Dispatch::Quit) => EventResult::with_cb(|c: &mut Cursive| c.quit()),
            Ok(Dispatch::Modal(m)) => popup(m),
            Err(e) => popup(e.to_string()),
        }
    }

    /// `F` (`Action::ConfirmUnlike`): a Yes/No cursive dialog.
    fn confirm_unlike(&mut self, id: TrackId) -> EventResult {
        let name = self
            .with_session(|s| s.store.get_track(id).ok().flatten())
            .map(|t| format!("{} - {}", t.display_artist(), t.title))
            .unwrap_or_else(|| "this track".to_string());
        let session = self.session.clone();
        EventResult::with_cb(move |c: &mut Cursive| {
            let session = session.clone();
            let dialog = Dialog::text(format!("Remove {name:?} from Liked Songs?"))
                .title("Unlike")
                .button("Remove", move |c| {
                    let _ = session.lock().unwrap().dispatch(Command::Unlike(id));
                    c.pop_layer();
                })
                .dismiss_button("Cancel");
            c.add_layer(dialog);
        })
    }

    pub(super) fn handle_action(&mut self, action: Action) -> EventResult {
        match action {
            Action::Command(c) => self.run(c),
            // On the Search screen itself, same as switching to the Search tab (focuses the input too).
            Action::FocusSearch => {
                if self.screen == SEARCH {
                    self.handle_action(Action::Screen(SEARCH))
                } else {
                    self.editing = Editing::Filter;
                    self.buffer.clear();
                    EventResult::consumed()
                }
            }
            Action::CommandLine => {
                self.editing = Editing::CommandLine;
                self.buffer.clear();
                EventResult::consumed()
            }
            Action::Screen(n) => {
                let was_playlists = self.screen == PLAYLISTS;
                self.screen = n;
                // Leaving the Playlists screen for anything else.
                if n != PLAYLISTS {
                    self.playlists.leave();
                } else if !was_playlists && self.playlists.at_top_level() {
                    // Switching back into Playlists fresh: restore the remembered playlist.
                    let playlists = self.with_session(|s| s.playlists());
                    self.playlists.restore(&playlists);
                }
                // A different screen's list — any filter over the old one is meaningless now.
                self.filter.query = None;
                // Switching to Search focuses the input immediately, same as `/`.
                if n == SEARCH {
                    self.editing = Editing::Search;
                    self.buffer.clear();
                }
                self.clamp_scroll();
                EventResult::consumed()
            }
            Action::Activate => self.activate(),
            // The `Event::Key(Key::Enter)` handler already special-cases this and never forwards it here.
            Action::PlayFromContext(id) => self.run(Command::Play(id)),
            Action::OpenHotkeyMenu => {
                self.open_hotkey_menu();
                EventResult::consumed()
            }
            Action::OpenHelp => {
                self.help = Some(HelpModal::new(self.help_lines()));
                EventResult::consumed()
            }
            Action::AddToPlaylistPrompt(id) => {
                let playlists = self.with_session(|s| s.playlists());
                self.playlist_picker = Some(PlaylistPicker::new(id, playlists));
                EventResult::consumed()
            }
            Action::NewPlaylistPrompt => {
                self.editing = Editing::CommandLine;
                self.buffer = "newplaylist ".to_string();
                EventResult::consumed()
            }
            Action::ConfirmUnlike(id) => self.confirm_unlike(id),
            Action::CyclePaneLayout => {
                let cur = (self.panes.cfg.side, self.panes.cfg.stack);
                let next = PANE_LAYOUT_CYCLE.iter().position(|&c| c == cur).map_or(0, |i| (i + 1) % PANE_LAYOUT_CYCLE.len());
                (self.panes.cfg.side, self.panes.cfg.stack) = PANE_LAYOUT_CYCLE[next];
                EventResult::consumed()
            }
            Action::None => EventResult::Ignored,
        }
    }

    /// Drops the text being typed; a cancelled filter shows the full list again.
    fn cancel_edit(&mut self) {
        if self.editing == Editing::Filter {
            self.filter.query = None;
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
        // A digit as Search's first keystroke is a screen switch, not a query.
        let leading_digit = self.editing == Editing::Search
            && self.buffer.is_empty()
            && matches!(event, Event::Char(c) if c.is_ascii_digit());
        if outside_click || leading_digit {
            self.cancel_edit();
            return None;
        }
        let edited = match event {
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
        // The filter narrows live as you type.
        if edited && self.editing == Editing::Filter {
            self.reset_filter_selection();
        }
        Some(EventResult::consumed())
    }

    /// The command/hint row: the text being typed, else transient feedback, else a key hint.
    pub(super) fn hint_line(&self, membership_feedback: Option<String>) -> String {
        match &self.editing {
            Editing::Search => format!("/{}", self.buffer),
            Editing::CommandLine => format!(":{}", self.buffer),
            Editing::PluginSetup(_) => format!("> {}", self.buffer),
            Editing::Filter => format!("/{}", self.buffer),
            // `queue_feedback` (this keypress only) wins over `membership_feedback`.
            Editing::None => self
                .queue_feedback
                .clone()
                .or(membership_feedback.map(|m| format!("  {m}")))
                .or(self.hotkeys.feedback.clone().map(|m| format!("  {m}")))
                .unwrap_or_else(|| {
                    // The Playlists screen's own hint replaces the generic one when a row/open playlist can take a hotkey.
                    if self.screen == PLAYLISTS
                        && self.with_session(|s| self.selected_hotkey_target(s)).is_some()
                    {
                        return "  [`] set hotkey".to_string();
                    }
                    let help_key = self
                        .with_session(|s| s.effective_hotkey(&HotkeyTarget::Builtin(core::BuiltinAction::OpenHelp)))
                        .map(String::from)
                        .unwrap_or_default();
                    format!("  [{help_key}] help")
                }),
        }
    }
}
