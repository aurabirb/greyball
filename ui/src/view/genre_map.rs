//! `:genre-map` pane — a scatter plot of the library's tracks positioned by a 2D PCA projection
//! of each track's genre embedding (`sources/genre-embed`, `core::embedding`), colored by BPM
//! (`super::rows::bpm_color`, the same gradient list rows use). Unlike `Vis` (continuously
//! resampled audio levels on a background worker), the projection only changes when the set of
//! embedded tracks changes or the pane resizes, so it's computed lazily on `draw`/`frame` and
//! memoized rather than driven by a background thread.

use std::sync::{Arc, Mutex};

use cursive::event::Key;
use cursive::theme::{Color, ColorStyle, Effect};
use cursive::{Printer, Rect};

use core::embedding::{GENRE_EMBEDDING_ATTR, decode_genre_embedding};
use core::{Command, Track, TrackId};

use super::memo::Memo;
use super::rows::bpm_color;
use super::window::Ctx;

const NEUTRAL_COLOR: Color = Color::Rgb(120, 120, 120);

/// One plotted track: its cell in the pane and the color it's drawn in.
struct Point {
    track_id: TrackId,
    title: String,
    artist: String,
    x: usize,
    y: usize,
    color: Color,
}

pub(super) struct GenreMapFrame {
    w: usize,
    h: usize,
    points: Vec<Point>,
}

impl GenreMapFrame {
    /// `{artist} - {title}` for the plotted track `id`, the app's usual track-label format
    /// (see `window_title_track_text`).
    pub(super) fn track_label(&self, id: TrackId) -> Option<String> {
        self.points.iter().find(|p| p.track_id == id).map(|p| format!("{} - {}", p.artist, p.title))
    }

    /// `Command::PlayContext` for pressing Enter on `id`: the same "play this, and let playback
    /// carry on through the rest of the list" shape a `TrackList` row's Enter uses
    /// (`TrackList::activate`), with the plotted points (in their plotted order) standing in for
    /// the list. `None` when `id` isn't actually one of the plotted points.
    pub(super) fn play_context(&self, id: TrackId) -> Option<Command> {
        let index = self.points.iter().position(|p| p.track_id == id)?;
        let tracks = self.points.iter().map(|p| p.track_id).collect();
        Some(Command::PlayContext { tracks, index, remote: None, local: None, name: Some("Genre Map".to_string()) })
    }
}

pub(super) struct GenreMap {
    cache: Memo<(u64, usize, usize), Arc<GenreMapFrame>>,
    selected: Mutex<Option<TrackId>>,
}

impl GenreMap {
    pub(super) fn new() -> Self {
        Self { cache: Memo::default(), selected: Mutex::new(None) }
    }

    /// The plot for `rect`'s width and one title row less of height, rebuilt only when the
    /// session revision or the pane size changed since the last call.
    pub(super) fn frame(&self, ctx: &Ctx, rect: Rect) -> Arc<GenreMapFrame> {
        let w = rect.width();
        let h = rect.height().saturating_sub(1);
        let key = (ctx.s.revision(), w, h);
        let frame = self.cache.get_or_build(key, || Arc::new(compute_frame(&ctx.s.tracks_with_embedding(), w, h)));
        let mut selected = self.selected.lock().unwrap_or_else(|e| e.into_inner());
        if !selected.is_some_and(|id| frame.points.iter().any(|p| p.track_id == id)) {
            *selected = frame.points.first().map(|p| p.track_id);
        }
        frame
    }

    pub(super) fn selected_track(&self) -> Option<TrackId> {
        *self.selected.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Selects `track`, if it's one of `frame`'s plotted points; a no-op (selection left as-is) when
    /// it has no embedding yet and so nothing to highlight.
    pub(super) fn select(&self, frame: &GenreMapFrame, track: TrackId) {
        if frame.points.iter().any(|p| p.track_id == track) {
            *self.selected.lock().unwrap_or_else(|e| e.into_inner()) = Some(track);
        }
    }

    /// Moves the selection to the nearest plotted point in `dir` from the current one. A no-op
    /// when nothing is plotted in that direction (the edge of the cluster) or at all.
    pub(super) fn nav(&self, frame: &GenreMapFrame, dir: Key) {
        let mut selected = self.selected.lock().unwrap_or_else(|e| e.into_inner());
        let Some(cur) = selected.and_then(|id| frame.points.iter().find(|p| p.track_id == id)) else { return };
        let (cx, cy) = (cur.x as isize, cur.y as isize);
        let cur_id = cur.track_id;
        let next = frame
            .points
            .iter()
            .filter(|p| p.track_id != cur_id)
            .filter(|p| in_direction(dir, cx, cy, p.x as isize, p.y as isize))
            .min_by_key(|p| direction_score(dir, cx, cy, p.x as isize, p.y as isize));
        if let Some(p) = next {
            *selected = Some(p.track_id);
        }
    }

    /// `now_playing` is a live overlay, not baked into the memoized `frame` — it's read fresh
    /// (`Session::now_playing_id`) on every draw so a track change highlights immediately without
    /// forcing a PCA recompute, which is only keyed on the embedded-track set and pane size.
    pub(super) fn draw(&self, printer: &Printer, focused: bool, frame: &GenreMapFrame, now_playing: Option<TrackId>) {
        let title = if focused { "[Genre Map]" } else { "Genre Map" };
        printer.with_color(ColorStyle::title_secondary(), |p| {
            p.print((0, 0), &crate::view::pad(title, p.size.x));
        });
        if printer.size.x == 0 || printer.size.y <= 1 || frame.w != printer.size.x || frame.h != printer.size.y.saturating_sub(1) {
            return; // not yet rebuilt for this size
        }
        let selected = self.selected_track();
        for point in &frame.points {
            let is_selected = Some(point.track_id) == selected;
            let is_playing = now_playing == Some(point.track_id);
            let white = Color::Dark(cursive::theme::BaseColor::White);
            let (style, glyph) = match (is_selected, is_playing) {
                (true, true) => (ColorStyle::new(white, point.color), "*"),
                (true, false) => (ColorStyle::new(white, point.color), "@"),
                (false, true) => (ColorStyle::new(point.color, Color::TerminalDefault), "*"),
                (false, false) => (ColorStyle::new(point.color, Color::TerminalDefault), "o"),
            };
            let effect = if is_playing { Effect::Bold } else { Effect::Simple };
            printer.with_effect(effect, |p| p.with_color(style, |p| p.print((point.x, point.y + 1), glyph)));
        }
    }
}

/// Whether `(x, y)` lies on the `dir` side of `(cx, cy)`.
fn in_direction(dir: Key, cx: isize, cy: isize, x: isize, y: isize) -> bool {
    match dir {
        Key::Up => y < cy,
        Key::Down => y > cy,
        Key::Left => x < cx,
        Key::Right => x > cx,
        _ => false,
    }
}

/// Lower is a better match for a `dir` press: squared distance along the pressed axis, with
/// perpendicular drift weighted heavier so a point roughly "in that direction" beats a distant
/// one that happens to be perfectly axis-aligned.
fn direction_score(dir: Key, cx: isize, cy: isize, x: isize, y: isize) -> isize {
    let (primary, perp) = match dir {
        Key::Up | Key::Down => (y - cy, x - cx),
        Key::Left | Key::Right => (x - cx, y - cy),
        _ => return isize::MAX,
    };
    primary * primary + perp * perp * 4
}

fn compute_frame(tracks: &[Track], w: usize, h: usize) -> GenreMapFrame {
    if w == 0 || h == 0 {
        return GenreMapFrame { w, h, points: Vec::new() };
    }
    let decoded: Vec<(&Track, Vec<f32>)> =
        tracks.iter().filter_map(|t| t.attrs.get(GENRE_EMBEDDING_ATTR).and_then(|s| decode_genre_embedding(s)).map(|e| (t, e))).collect();
    // Every real embedding is the model's fixed dimension, but a stale entry from before a model
    // change (or a corrupt attr that still happens to decode) could disagree — go with whatever
    // length most tracks agree on and drop the rest, rather than indexing off the first track's
    // length and risking an out-of-bounds panic on a mismatched one.
    let mut dim_votes: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (_, e) in &decoded {
        if !e.is_empty() {
            *dim_votes.entry(e.len()).or_insert(0) += 1;
        }
    }
    let Some(dim) = dim_votes.into_iter().max_by_key(|&(_, votes)| votes).map(|(dim, _)| dim) else {
        return GenreMapFrame { w, h, points: Vec::new() };
    };
    let decoded: Vec<(&Track, Vec<f32>)> = decoded.into_iter().filter(|(_, e)| e.len() == dim).collect();
    let n = decoded.len() as f32;
    let mean: Vec<f32> = (0..dim).map(|d| decoded.iter().map(|(_, e)| e[d]).sum::<f32>() / n).collect();
    let centered: Vec<Vec<f32>> = decoded.iter().map(|(_, e)| e.iter().zip(&mean).map(|(x, m)| x - m).collect()).collect();

    let (pc1, pc2) = pca_top2(&centered, dim);
    let proj: Vec<(f32, f32)> = centered.iter().map(|v| (dot(v, &pc1), dot(v, &pc2))).collect();

    let (xmin, xmax) = min_max(proj.iter().map(|p| p.0));
    let (ymin, ymax) = min_max(proj.iter().map(|p| p.1));
    let points = decoded
        .iter()
        .zip(&proj)
        .map(|((track, _), &(px, py))| {
            let nx = if xmax > xmin { (px - xmin) / (xmax - xmin) } else { 0.5 };
            let ny = if ymax > ymin { (py - ymin) / (ymax - ymin) } else { 0.5 };
            let x = (nx * (w.saturating_sub(1)) as f32).round() as usize;
            let y = (ny * (h.saturating_sub(1)) as f32).round() as usize;
            let color = track.attrs.get("bpm").and_then(|bpm| bpm_color(bpm)).unwrap_or(NEUTRAL_COLOR);
            Point { track_id: track.id, title: track.title.clone(), artist: track.display_artist(), x: x.min(w - 1), y: y.min(h - 1), color }
        })
        .collect();
    GenreMapFrame { w, h, points }
}

fn min_max(vals: impl Iterator<Item = f32>) -> (f32, f32) {
    vals.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// L2-normalizes `v` in place, returning its pre-normalization norm.
fn normalize(v: &mut [f32]) -> f32 {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    norm
}

/// Top eigenvector of `data`'s (never materialized) covariance matrix via power iteration: each
/// iteration is two O(N·d) passes computing `Σv` directly, rather than the O(N·d²) cost of
/// forming the d×d covariance matrix — the standard trick for PCA when only a couple of
/// components are needed out of a high-dimensional embedding.
fn power_iteration(data: &[Vec<f32>], dim: usize) -> Vec<f32> {
    let n = (data.len().max(1)) as f32;
    let mut v = vec![1.0f32; dim];
    normalize(&mut v);
    for _ in 0..40 {
        let mut next = vec![0.0f32; dim];
        for row in data {
            let s = dot(row, &v);
            for (nx, r) in next.iter_mut().zip(row) {
                *nx += s * r;
            }
        }
        for x in &mut next {
            *x /= n;
        }
        if normalize(&mut next) < 1e-9 {
            break; // degenerate (e.g. fewer than 2 distinct points): nothing left to converge to
        }
        v = next;
    }
    v
}

/// The top two principal components of `centered` (already mean-subtracted). PC2 is found by
/// deflation: projecting PC1's contribution out of every row once, then running power iteration
/// again on what's left, rather than a second implicit-matrix pass that would keep rediscovering
/// PC1 itself.
fn pca_top2(centered: &[Vec<f32>], dim: usize) -> (Vec<f32>, Vec<f32>) {
    if centered.is_empty() {
        return (vec![0.0; dim], vec![0.0; dim]);
    }
    let pc1 = power_iteration(centered, dim);
    let deflated: Vec<Vec<f32>> = centered
        .iter()
        .map(|row| {
            let proj = dot(row, &pc1);
            row.iter().zip(&pc1).map(|(r, d)| r - proj * d).collect()
        })
        .collect();
    let pc2 = power_iteration(&deflated, dim);
    (pc1, pc2)
}
