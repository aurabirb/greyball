//! Minimal modal directory browser, opened by `:open` with no arguments.
//!
//! Pure cursive glue: it walks the filesystem, and on a file submit hands a
//! one-element [`Command::AddFilesToPlaylist`] to the shared [`Session`], then
//! refreshes its own list in place. Directories navigate; `..` goes up; Esc or
//! the *Close* button dismiss it.

use std::path::{Path, PathBuf};

use cursive::{
    Cursive,
    event::Key,
    view::{Nameable, Resizable, Scrollable},
    views::{Dialog, LinearLayout, OnEventView, SelectView, TextView},
};

use core::{audio, Command, Dispatch, PlaylistId};

use crate::SessionHandle;

const LIST: &str = "medley_fb_list";
const PATH: &str = "medley_fb_path";
const INFO: &str = "medley_fb_info";

/// One row in the browser.
#[derive(Clone)]
enum Entry {
    /// Parent directory (`..`).
    Up(PathBuf),
    Dir(PathBuf),
    File(PathBuf),
}

/// Open the browser rooted at `start`, adding picked files to `playlist` —
/// or to the queue when `playlist` is `None` (no playlist open/selected).
pub fn open(siv: &mut Cursive, session: SessionHandle, playlist: Option<PlaylistId>, start: PathBuf) {
    let picker = {
        let session = session.clone();
        SelectView::<Entry>::new().on_submit(move |s, entry| match entry {
            Entry::Up(p) | Entry::Dir(p) => populate(s, p),
            Entry::File(p) => {
                let msg = add_file(&session, playlist, p);
                s.call_on_name(INFO, |v: &mut TextView| v.set_content(msg));
            }
        })
    };

    let body = LinearLayout::vertical()
        .child(TextView::new(start.display().to_string()).with_name(PATH))
        .child(picker.with_name(LIST).scrollable().min_size((48, 16)))
        .child(TextView::new("Enter: open dir / add file    Esc: close").with_name(INFO));

    let dialog = Dialog::around(body)
        .title("Add local files")
        .button("Close", |s| {
            s.pop_layer();
        });

    siv.add_layer(OnEventView::new(dialog).on_event(Key::Esc, |s| {
        s.pop_layer();
    }));
    populate(siv, &start);
}

/// Dispatch the add and return the message to show in the info line.
fn add_file(session: &SessionHandle, playlist: Option<PlaylistId>, path: &Path) -> String {
    let mut guard = match session.lock() {
        Ok(g) => g,
        Err(_) => return "session busy".to_string(),
    };
    match guard.dispatch(Command::AddFilesToPlaylist {
        playlist,
        paths: vec![path.to_path_buf()],
    }) {
        Ok(Dispatch::Done(m)) => m,
        Ok(_) => format!("added {}", path.display()),
        Err(e) => e.to_string(),
    }
}

/// Refresh the list to show `dir`'s contents.
fn populate(siv: &mut Cursive, dir: &Path) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let readable = match std::fs::read_dir(dir) {
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

    siv.call_on_name(PATH, |v: &mut TextView| {
        v.set_content(dir.display().to_string())
    });
    siv.call_on_name(LIST, |v: &mut SelectView<Entry>| {
        v.clear();
        if let Some(parent) = dir.parent() {
            v.add_item("../", Entry::Up(parent.to_path_buf()));
        }
        for p in dirs {
            v.add_item(format!("{}/", name_of(&p)), Entry::Dir(p));
        }
        for p in files {
            v.add_item(format!("♪ {}", name_of(&p)), Entry::File(p));
        }
    });
    if !readable {
        siv.call_on_name(INFO, |v: &mut TextView| {
            v.set_content(format!("cannot read {}", dir.display()))
        });
    }
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}
