//! `:genre-map` pane — a scatter plot of the library's tracks positioned by a 2D PCA projection
//! of each track's genre embedding (`sources/genre-embed`, `core::embedding`), colored by BPM
//! (`super::rows::bpm_color`, the same gradient list rows use). Unlike `Vis` (continuously
//! resampled audio levels on a background worker), the projection only changes when the set of
//! embedded tracks changes or the pane resizes, so it's computed lazily on `draw`/`frame` and
//! memoized rather than driven by a background thread.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Same click-timing window `TrackList` uses for its own double-click detection.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

pub(super) struct GenreMap {
    cache: Memo<(u64, usize, usize), Arc<GenreMapFrame>>,
    selected: Mutex<Option<TrackId>>,
    last_click: Mutex<Option<(Instant, TrackId)>>,
}

impl GenreMap {
    pub(super) fn new() -> Self {
        Self { cache: Memo::default(), selected: Mutex::new(None), last_click: Mutex::new(None) }
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

    /// Selects the plotted point nearest `(x, y)` (pane-content coordinates, i.e. before the `+1`
    /// title-row offset `draw` places points at) and returns it; `None` when nothing is plotted.
    fn select_at(&self, frame: &GenreMapFrame, x: usize, y: usize) -> Option<TrackId> {
        let nearest = frame.points.iter().min_by_key(|p| {
            let (dx, dy) = (p.x as isize - x as isize, p.y as isize - y as isize);
            dx * dx + dy * dy
        })?;
        *self.selected.lock().unwrap_or_else(|e| e.into_inner()) = Some(nearest.track_id);
        Some(nearest.track_id)
    }

    /// A mouse click at `(x, y)`: selects the nearest point like `select_at`, and returns it again
    /// when this click completes a double-click (same point, within `DOUBLE_CLICK_WINDOW`) so the
    /// caller can play it — the mouse's equivalent of arrow-nav-then-Enter.
    pub(super) fn click(&self, frame: &GenreMapFrame, x: usize, y: usize) -> Option<TrackId> {
        let id = self.select_at(frame, x, y)?;
        let now = Instant::now();
        let mut last_click = self.last_click.lock().unwrap_or_else(|e| e.into_inner());
        let double = last_click.is_some_and(|(t, last_id)| last_id == id && now.duration_since(t) <= DOUBLE_CLICK_WINDOW);
        *last_click = (!double).then_some((now, id));
        double.then_some(id)
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
        // Multiple tracks can land on the same cell (small pane, large library); track counts per
        // cell so a cluster gets a distinct glyph instead of silently drawing as one track.
        let mut cell_counts: std::collections::HashMap<(usize, usize), usize> = std::collections::HashMap::new();
        for point in &frame.points {
            *cell_counts.entry((point.x, point.y)).or_insert(0) += 1;
        }
        // Now-playing's pulsing reticle: sampled fresh from wall-clock time each draw, no stored
        // animation-phase field, so it's purely a function of "now" rather than app state.
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let ring = PLAYING_RING[(millis / 400) as usize % PLAYING_RING.len()];
        let pulse = (millis / 400).is_multiple_of(2);
        for point in &frame.points {
            let is_selected = Some(point.track_id) == selected;
            let is_playing = now_playing == Some(point.track_id);
            let clustered = cell_counts[&(point.x, point.y)] > 1;
            let white = Color::Dark(cursive::theme::BaseColor::White);
            let style = if is_selected { ColorStyle::new(white, point.color) } else { ColorStyle::new(point.color, Color::TerminalDefault) };
            let base = if is_selected { SELECTED_GLYPH } else if clustered { CLUSTERED_GLYPH } else { NORMAL_GLYPH };
            let mut glyph = base.to_string();
            let mut effect = if clustered { Effect::Bold } else { Effect::Simple };
            if is_playing {
                // A combining ring stacks onto the base glyph in the same cell (zero-width, doesn't
                // advance the cursor) and cycles shape; the effect also alternates for a light pulse.
                glyph.push(ring);
                effect = if pulse { Effect::Reverse } else { Effect::Bold };
            }
            printer.with_effect(effect, |p| p.with_color(style, |p| p.print((point.x, point.y + 1), &glyph)));
        }
    }
}

/// Unselected, un-clustered point.
const NORMAL_GLYPH: &str = "•";
/// A cell holding more than one track — a different silhouette (star) from a dot, not just a fuller circle.
const CLUSTERED_GLYPH: &str = "★";
/// The selected point — a square, distinguishable from the dot/star at a glance even without its
/// white-on-background styling.
const SELECTED_GLYPH: &str = "■";
/// Combining marks cycled to give the now-playing point a pulsing reticle ring.
const PLAYING_RING: [char; 3] = ['\u{20DD}', '\u{20DF}', '\u{20DE}']; // enclosing circle, diamond, square

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

    let placed = fit_to_pane(&proj, w, h);
    let points = decoded
        .iter()
        .zip(&placed)
        .map(|((track, _), &(x, y))| {
            let color = track.attrs.get("bpm").and_then(|bpm| bpm_color(bpm)).unwrap_or(NEUTRAL_COLOR);
            Point { track_id: track.id, title: track.title.clone(), artist: track.display_artist(), x, y, color }
        })
        .collect();
    GenreMapFrame { w, h, points }
}

/// Rotates `proj` to the orientation that best fills a `w`×`h` pane, scales uniformly (both axes by
/// the same factor, so relative distances/angles between points are preserved — a similarity
/// transform, not a stretch) and centers the result, returning each point's pane cell.
///
/// Rotation angle comes from rotating calipers over the point cloud's convex hull: for each hull
/// edge direction, the best axis-aligned fit (`min(pane axis / point-cloud extent)` over x and y) is
/// a candidate, and the edge whose fit needs the least shrinking wins. No point is ever discarded —
/// the final bounding box is the true extent of every rotated point, so nothing needs clipping.
fn fit_to_pane(proj: &[(f32, f32)], w: usize, h: usize) -> Vec<(usize, usize)> {
    let (w1, h1) = ((w.saturating_sub(1)) as f32, (h.saturating_sub(1)) as f32);
    if proj.is_empty() {
        return Vec::new();
    }
    if proj.len() == 1 {
        return vec![(w1 as usize / 2, h1 as usize / 2)];
    }

    let angle = best_angle(proj, w1, h1);
    let (cos_t, sin_t) = (angle.cos(), angle.sin());
    let rotated: Vec<(f32, f32)> = proj.iter().map(|&(x, y)| rotate(x, y, cos_t, sin_t)).collect();
    let (xlo, xhi) = min_max(rotated.iter().map(|p| p.0));
    let (ylo, yhi) = min_max(rotated.iter().map(|p| p.1));
    let (dx, dy) = (xhi - xlo, yhi - ylo);
    // Both extents near-0 means every point is within `fit_scale`'s 1e-6 threshold of the centroid
    // (e.g. duplicate embeddings): `fit_scale` would be infinite, and `0.0 * infinity` is NaN, so pin
    // it to a finite value — the multiplied term is negligible either way.
    let scale = fit_scale(dx, dy, w1, h1);
    let scale = if scale.is_finite() { scale } else { 1.0 };
    // Center: place the scaled cloud's midpoint at the pane's midpoint.
    let (cx, cy) = ((xlo + xhi) / 2.0, (ylo + yhi) / 2.0);
    let (pcx, pcy) = (w1 / 2.0, h1 / 2.0);
    rotated
        .iter()
        .map(|&(x, y)| {
            let px = (pcx + (x - cx) * scale).round().clamp(0.0, w1);
            let py = (pcy + (y - cy) * scale).round().clamp(0.0, h1);
            (px as usize, py as usize)
        })
        .collect()
}

/// Rotates `(x, y)` by `-θ` (`cos_t`/`sin_t` of `θ`), i.e. into the frame where `θ`'s direction lies
/// along the x-axis.
fn rotate(x: f32, y: f32, cos_t: f32, sin_t: f32) -> (f32, f32) {
    (x * cos_t + y * sin_t, -x * sin_t + y * cos_t)
}

/// How much of the pane a `dx`×`dy` bounding box can fill without overflowing either dimension
/// (`0.0` when the box is degenerate along an axis the pane isn't, since anything scales to fit a
/// zero-width extent).
fn fit_scale(dx: f32, dy: f32, w1: f32, h1: f32) -> f32 {
    let sx = if dx > 1e-6 { w1 / dx } else { f32::INFINITY };
    let sy = if dy > 1e-6 { h1 / dy } else { f32::INFINITY };
    sx.min(sy)
}

/// The rotation angle (radians) that best fits `points` into a `w1`×`h1` box, via rotating calipers
/// over the convex hull: candidate angles are each hull edge's direction, and the best one
/// maximizes `fit_scale` of the rotated point cloud's bounding box against the target box — the most
/// of the pane usable without overflowing either axis. This is deliberately the pane-aspect-fit
/// objective, not the classic minimum-area rotating-calipers problem (which optimizes `dx*dy`
/// regardless of the target's shape and would pick a worse angle for a non-square pane).
fn best_angle(points: &[(f32, f32)], w1: f32, h1: f32) -> f32 {
    let hull = convex_hull(points);
    let edges: Vec<(f32, f32)> = match hull.len() {
        0 | 1 => return 0.0,
        2 => vec![(hull[1].0 - hull[0].0, hull[1].1 - hull[0].1)],
        _ => (0..hull.len())
            .map(|i| {
                let a = hull[i];
                let b = hull[(i + 1) % hull.len()];
                (b.0 - a.0, b.1 - a.1)
            })
            .collect(),
    };

    let mut best = 0.0f32;
    let mut best_fit = f32::NEG_INFINITY;
    for (ex, ey) in edges {
        let len = (ex * ex + ey * ey).sqrt();
        if len < 1e-9 {
            continue; // duplicate consecutive hull points: no direction to test
        }
        let theta = ey.atan2(ex);
        // `fit_scale` isn't symmetric in dx/dy unless w1 == h1, so an edge's own direction and its
        // perpendicular generally score differently (unlike the classic minimum-area calipers
        // problem, where dx*dy is symmetric and only the edge direction itself needs testing) —
        // both must be candidates, or a hull with no edge near the long axis's true best angle gets
        // missed entirely.
        for candidate in [theta, theta + std::f32::consts::FRAC_PI_2] {
            let (cos_t, sin_t) = (candidate.cos(), candidate.sin());
            let rotated: Vec<(f32, f32)> = points.iter().map(|&(x, y)| rotate(x, y, cos_t, sin_t)).collect();
            let (xlo, xhi) = min_max(rotated.iter().map(|p| p.0));
            let (ylo, yhi) = min_max(rotated.iter().map(|p| p.1));
            let fit = fit_scale(xhi - xlo, yhi - ylo, w1, h1);
            if fit > best_fit {
                best_fit = fit;
                best = candidate;
            }
        }
    }
    best
}

fn min_max(vals: impl Iterator<Item = f32>) -> (f32, f32) {
    vals.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

/// 2D convex hull via Andrew's monotone chain, in counter-clockwise order with no repeated closing
/// point. Duplicate points collapse away naturally (a repeated point never survives the turn test).
/// Callers handle 0/1/2-point inputs themselves; this still degrades gracefully on them (returns the
/// input as-is) rather than panicking.
fn convex_hull(points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut sorted: Vec<(f32, f32)> = points.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    sorted.dedup();
    if sorted.len() < 3 {
        return sorted;
    }
    // Cross product of OA x OB; > 0 means a counter-clockwise (left) turn.
    let cross = |o: (f32, f32), a: (f32, f32), b: (f32, f32)| (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0);

    let mut lower: Vec<(f32, f32)> = Vec::new();
    for &p in &sorted {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<(f32, f32)> = Vec::new();
    for &p in sorted.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
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
