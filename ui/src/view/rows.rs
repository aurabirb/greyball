
use cursive::{Printer, Rect};
use cursive::theme::{Color, ColorStyle, Effect, Style};

use unicode_width::UnicodeWidthStr;

use core::{HotkeyMembership, PendingRows, Session, waveform};

use crate::row::RowItem;

use super::scroll::draw_scrollbar;
use super::tab_bar::{GLYPHS, resample};
use super::text::{pad, pad_right_aligned, truncate};

/// A run of one cell's text sharing a style; `color: None` draws in the row's own color.
#[derive(Clone)]
struct Span {
    text: String,
    color: Option<Color>,
    italic: bool,
}

/// One column's rendered cell, from `render_cell`.
#[derive(Clone)]
pub(super) struct Cell {
    spans: Vec<Span>,
}

impl Cell {
    pub(super) fn plain(text: impl Into<String>) -> Self {
        Self::colored(text, None)
    }

    fn colored(text: impl Into<String>, color: Option<Color>) -> Self {
        Self { spans: vec![Span { text: text.into(), color, italic: false }] }
    }

    pub(super) fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn styled(&self) -> bool {
        self.spans.iter().any(|s| s.italic || s.color.is_some())
    }
}

/// A rendered list row; every column is a `Cell` produced by `render_cell`.
pub(super) struct Row {
    /// A track's attribute tags.
    tags: Cell,
    main: Cell,
    wave: Cell,
    /// One letter per hotkey-bound playlist holding this track, italic while that membership is pending; a top-level playlist's key.
    pub(super) hotkeys: Cell,
    source: Cell,
    duration: Cell,
    current: bool,
    /// This row's own presence in the list on screen is still settling (a remote add/remove in flight).
    pending: bool,
}

/// A `Row` with only its main column set — placeholder messages and the Playlists top level.
pub(super) fn plain_row(main: impl Into<String>) -> Row {
    Row {
        tags: Cell::plain(""),
        main: main_cell(" ", false, &main.into()),
        wave: Cell::plain(""),
        source: Cell::plain(""),
        duration: Cell::plain(""),
        hotkeys: Cell::plain(""),
        current: false,
        pending: false,
    }
}

/// Which `Row` column a `render_cell` call is producing.
enum Column {
    Tags,
    Main,
    Wave,
    Hotkeys,
    Source,
    Duration,
}

/// The main column: the one-cell liked-marker slot in the tags/title gap, then the title.
fn main_cell(mark: &str, italic: bool, title: &str) -> Cell {
    Cell {
        spans: vec![
            Span { text: mark.to_string(), color: None, italic },
            Span { text: title.to_string(), color: None, italic: false },
        ],
    }
}

/// The single per-column cell renderer — a pure function of the track plus the state handed in.
fn render_cell(
    col: Column,
    t: &core::Track,
    cached: bool,
    liked: Option<bool>,
    visible: &[String],
    hotkeys: &[HotkeyMembership],
) -> Cell {
    match col {
        Column::Tags => {
            let color = visible.iter().find_map(|attr| t.attrs.get(attr).and_then(|v| tag_color(attr, v)));
            Cell::colored(pad_right_aligned(&t.tags(visible), TAGS_COL_W), color)
        }
        Column::Main => {
            let dot = if liked.is_some() { LIKED_MARK } else { " " };
            main_cell(dot, liked == Some(true), &t.main())
        }
        Column::Wave => Cell::plain(wave_text(t)),
        Column::Source => Cell::plain(t.source(cached)),
        Column::Duration => Cell::plain(t.duration()),
        Column::Hotkeys => Cell {
            spans: hotkeys
                .iter()
                .filter_map(|m| {
                    let italic = m.pending.contains(&t.id);
                    (italic || m.members.contains(&t.id))
                        .then(|| Span { text: m.key.to_string(), color: None, italic })
                })
                .collect(),
        },
    }
}

/// The one track-row builder every list and docked pane goes through.
pub(super) fn tracks_to_rows(s: &Session, tracks: Vec<core::Track>, pending: &PendingRows, offset: usize, playing: Option<usize>) -> Vec<Row> {
    let visible = &s.cfg.visible_track_attrs;
    let hotkeys = s.hotkey_memberships();
    tracks
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let cached = s.is_track_cached(&t);
            let liked = s.liked_mark(t.id);
            Row {
                tags: render_cell(Column::Tags, &t, cached, liked, visible, &hotkeys),
                main: render_cell(Column::Main, &t, cached, liked, visible, &hotkeys),
                wave: render_cell(Column::Wave, &t, cached, liked, visible, &hotkeys),
                hotkeys: render_cell(Column::Hotkeys, &t, cached, liked, visible, &hotkeys),
                source: render_cell(Column::Source, &t, cached, liked, visible, &hotkeys),
                duration: render_cell(Column::Duration, &t, cached, liked, visible, &hotkeys),
                current: playing == Some(offset + i),
                pending: pending.has(t.id, offset + i),
            }
        })
        .collect()
}

/// Per-tag-attr cell renderer, keyed by attr name (a `Config::visible_track_attrs` entry).
fn tag_color(attr: &str, value: &str) -> Option<Color> {
    match attr {
        "bpm" => bpm_color(value),
        _ => None,
    }
}

/// Colors bpm on a blue→green→red gradient clamped to 60-180 bpm.
fn bpm_color(bpm: &str) -> Option<Color> {
    const LOW: f64 = 60.0;
    const MID: f64 = 120.0;
    const HIGH: f64 = 180.0;
    const COOL: (u8, u8, u8) = (60, 110, 220);
    const NEUTRAL: (u8, u8, u8) = (90, 200, 90);
    const HOT: (u8, u8, u8) = (220, 60, 60);

    let bpm: f64 = bpm.parse().ok()?;
    let bpm = bpm.clamp(LOW, HIGH);
    let (a, b, t) = if bpm < MID {
        (COOL, NEUTRAL, (bpm - LOW) / (MID - LOW))
    } else {
        (NEUTRAL, HOT, (bpm - MID) / (HIGH - MID))
    };
    let lerp = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    Some(Color::Rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2)))
}

/// Rows every list view spends on its title, in the main area and docked panes alike.
pub(super) const LIST_TITLE_ROWS: usize = 1;

/// A title row plus a window of rows and a scrollbar, shared by the main list and docked panes.
pub(super) fn draw_row_list(printer: &Printer, title: &str, back: bool, rows: &[Row], offset: usize, sel: usize, total: usize) {
    // Reserve the rightmost column of the list body as a scrollbar gutter.
    let content_w = printer.size.x.saturating_sub(1);
    let indent = main_col_start(content_w);
    printer.with_color(ColorStyle::title_primary(), |p| {
        p.print((0, 0), &pad(&format!("{:indent$}{title}", ""), content_w));
    });
    if back && back_button_fits(content_w) {
        printer.with_color(ColorStyle::title_primary(), |p| p.print((0, 0), BACK_LABEL));
    }
    let body_h = printer.size.y.saturating_sub(LIST_TITLE_ROWS);
    let body = printer.windowed(Rect::from_size((0, LIST_TITLE_ROWS), (printer.size.x, body_h)));
    draw_list_body(&body, rows, offset, sel, total);
}

/// The title row's clickable back button, over the tags column; hidden when it would run into the title.
pub(super) const BACK_LABEL: &str = "<back";

pub(super) fn back_button_fits(content_w: usize) -> bool {
    main_col_start(content_w) > BACK_LABEL.len()
}

/// Fills every row of `printer` with `rows` plus a scrollbar gutter — no title row of its own.
fn draw_list_body(printer: &Printer, rows: &[Row], offset: usize, sel: usize, total: usize) {
    let content_w = printer.size.x.saturating_sub(1);
    let list_h = printer.size.y;
    let layout = column_layout(content_w.saturating_sub(ROW_MARK_W));
    for (y, row) in rows.iter().enumerate() {
        let selected = y + offset == sel;
        let mark = if row.current { "> " } else { "  " };
        let cells = [&row.tags, &row.main, &row.wave, &row.hotkeys, &row.source, &row.duration];
        let [tags, main, wave, hotkeys, source, duration] = cells.map(Cell::text);
        let cols = columns([&tags, &main, &wave, &hotkeys, &source, &duration], &layout, content_w.saturating_sub(ROW_MARK_W));
        let line = pad(&format!("{mark}{cols}"), content_w);
        let mut row_style = Style::from(if selected {
            ColorStyle::highlight()
        } else if row.current {
            ColorStyle::secondary()
        } else {
            ColorStyle::primary()
        });
        if row.pending {
            row_style = row_style.combine(Effect::Italic).combine(Effect::Dim);
        }
        printer.with_style(row_style, |p| p.print((0, y), &line));
        // The selection/now-playing color takes the whole line; a span's own color only shows on a plain row.
        let plain = !selected && !row.current;
        for (&(start, width, right_aligned), cell) in layout.iter().zip(cells).filter_map(|(l, c)| Some((l.as_ref()?, c))) {
            if !cell.styled() {
                continue;
            }
            let end = ROW_MARK_W + start + width;
            let indent = if right_aligned { width.saturating_sub(cell.text().width()) } else { 0 };
            let mut x = ROW_MARK_W + start + indent;
            for span in &cell.spans {
                let text = truncate(&span.text, end.saturating_sub(x));
                let mut style = row_style;
                if plain && let Some(color) = span.color {
                    style = style.combine(ColorStyle::front(color));
                }
                // Combining an effect twice toggles it back off.
                if span.italic && !row.pending {
                    style = style.combine(Effect::Italic);
                }
                printer.with_style(style, |p| p.print((x, y), &text));
                x += text.width();
            }
        }
    }
    draw_scrollbar(printer, content_w, list_h, offset, total);
}

/// Each `columns` column's `(start, width, right-aligned)` in `Row` field order; `None` when hidden.
type Layout = [Option<(usize, usize, bool)>; 6];

fn column_layout(width: usize) -> Layout {
    let show_source = width + ROW_MARK_W + 1 >= SOURCE_MIN_LIST_W;
    let source_w = if show_source { SOURCE_COL_W + 1 } else { 0 };
    let fixed = TAGS_COL_W + HOTKEYS_COL_W + 1 + source_w + DURATION_COL_W + 1;
    if width <= fixed {
        return [None; 6];
    }
    let show_wave = width - fixed >= WAVE_COL_W + 1 + WAVE_MAIN_MIN;
    let wave_w = if show_wave { WAVE_COL_W + 1 } else { 0 };
    let main_w = width - fixed - wave_w;
    let main_start = TAGS_COL_W;
    let wave_start = main_start + main_w + 1;
    let hotkeys_start = main_start + main_w + wave_w + 1;
    let source_start = hotkeys_start + HOTKEYS_COL_W + 1;
    let duration_start = if show_source { source_start + SOURCE_COL_W + 1 } else { source_start };
    [
        Some((0, TAGS_COL_W, true)),
        Some((main_start, main_w, false)),
        show_wave.then_some((wave_start, WAVE_COL_W, false)),
        Some((hotkeys_start, HOTKEYS_COL_W, false)),
        show_source.then_some((source_start, SOURCE_COL_W, false)),
        Some((duration_start, DURATION_COL_W, false)),
    ]
}

/// The track's envelope as bar glyphs across the waveform column; blank when it has none.
fn wave_text(t: &core::Track) -> String {
    let Some(hex) = t.attrs.get(waveform::ATTR) else { return String::new() };
    let envelope = waveform::decode(hex);
    if envelope.is_empty() {
        return String::new();
    }
    resample(&envelope, WAVE_COL_W).iter().map(|&l| if l == 0 { " " } else { GLYPHS[l as usize - 1] }).collect()
}

const WAVE_COL_W: usize = 12;

/// The least the title keeps when the waveform column is shown.
const WAVE_MAIN_MIN: usize = 30;

/// Narrowest list (including mark and scrollbar gutter) that still shows the source column.
const SOURCE_MIN_LIST_W: usize = 80;

/// Width of a row's leading now-playing marker (`"> "`/`"  "`).
const ROW_MARK_W: usize = 2;

/// Column a row's title text starts at (after the liked-marker slot) — what the title row is indented by.
fn main_col_start(content_w: usize) -> usize {
    let layout = column_layout(content_w.saturating_sub(ROW_MARK_W));
    ROW_MARK_W + layout[1].map_or(0, |(start, ..)| start + LIKED_MARK_W)
}

/// Fixed widths for the tags/hotkeys/source/duration columns of a track row (see `ui::row::RowItem`);
const TAGS_COL_W: usize = 3;

const LIKED_MARK_W: usize = 1;

const LIKED_MARK: &str = "·";

const SOURCE_COL_W: usize = 5;

const DURATION_COL_W: usize = 6;

const HOTKEYS_COL_W: usize = 6;

fn columns(cells: [&str; 6], layout: &Layout, width: usize) -> String {
    if layout[1].is_none() {
        return truncate(cells[1], width);
    }
    let mut line = String::new();
    for (l, cell) in layout.iter().zip(cells) {
        let Some(&(start, w, right)) = l.as_ref() else { continue };
        line.push_str(&" ".repeat(start.saturating_sub(line.width())));
        line.push_str(&if right { pad_right_aligned(cell, w) } else { pad(cell, w) });
    }
    line
}
