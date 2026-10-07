//! `:genre-map` pane: tracks scattered by a 2D PCA projection of their genre embedding, colored by
//! BPM. The PCA projection is computed on a per-pane worker thread; `frame` fits the last finished one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cursive::event::Key;
use cursive::theme::{Color, ColorStyle, Effect};
use cursive::{Printer, Rect};

use core::embedding::EmbeddedTrack;
use core::{Command, MutexExt, Session, Track, TrackId};

use super::memo::Memo;
use super::rows::{Row, bpm_color, single_track_row};
use super::window::Ctx;

const NEUTRAL_COLOR: Color = Color::Rgb(120, 120, 120);

/// One plotted track: its cell in the pane and the color it's drawn in.
struct Point {
    track: Arc<Track>,
    x: usize,
    y: usize,
    color: Color,
}

/// The points fitted to one pane size, grouped by the cell they land in.
struct Plot {
    w: usize,
    h: usize,
    points: Vec<Point>,
    index: HashMap<TrackId, usize>,
    /// Point indices sorted by cell, so each cell's points are adjacent.
    by_cell: Vec<usize>,
}

impl Plot {
    fn new(tracks: &[EmbeddedTrack], fitted: &Fitted, w: usize, h: usize) -> Self {
        let points: Vec<Point> = tracks
            .iter()
            .filter_map(|e| {
                let &(x, y) = fitted.get(&e.track.id)?;
                let color = e.track.attrs.get("bpm").and_then(|bpm| bpm_color(bpm)).unwrap_or(NEUTRAL_COLOR);
                Some(Point { track: e.track.clone(), x, y, color })
            })
            .collect();
        let index = points.iter().enumerate().map(|(i, p)| (p.track.id, i)).collect();
        let mut by_cell: Vec<usize> = (0..points.len()).collect();
        by_cell.sort_by_key(|&i| (points[i].y, points[i].x));
        Self { w, h, points, index, by_cell }
    }

    fn cell(&self, i: usize) -> (usize, usize) {
        (self.points[i].y, self.points[i].x)
    }

    fn point(&self, id: TrackId) -> Option<&Point> {
        self.index.get(&id).map(|&i| &self.points[i])
    }
}

pub(super) struct GenreMapFrame {
    plot: Arc<Plot>,
    /// The plot is from an older projection than the current embeddings'.
    computing: bool,
}

impl GenreMapFrame {
    /// The selected point's track as a list row.
    pub(super) fn track_row(&self, s: &Session, id: TrackId, playing: bool) -> Option<Row> {
        Some(single_track_row(s, &self.plot.point(id)?.track, playing))
    }

    /// Plays `id` with the plotted points as the context, like a `TrackList` row's Enter.
    pub(super) fn play_context(&self, id: TrackId) -> Option<Command> {
        let index = *self.plot.index.get(&id)?;
        let tracks = self.plot.points.iter().map(|p| p.track.id).collect();
        Some(Command::PlayContext { tracks, index, remote: None, local: None, name: Some("Genre Map".to_string()) })
    }
}

/// Same click-timing window `TrackList` uses for its own double-click detection.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// The finished projection's embeddings gen, `w`, `h`.
type FitKey = (Option<u64>, usize, usize);

/// Each fitted track's pane cell.
type Fitted = HashMap<TrackId, (usize, usize)>;

#[derive(Default)]
struct Selection {
    selected: Option<TrackId>,
    /// Selected once plotted; outranks the first-point fallback until a current frame resolves it.
    pending: Option<TrackId>,
    last_click: Option<(Instant, TrackId)>,
}

pub(super) struct GenreMap {
    fit: Memo<FitKey, Arc<Fitted>>,
    /// Keyed on the embedded-tracks gen plus `fit`'s key.
    plot: Memo<(u64, FitKey), Arc<Plot>>,
    projection: ProjectionWorker,
    selection: Mutex<Selection>,
}

impl GenreMap {
    pub(super) fn new() -> Self {
        Self { fit: Memo::default(), plot: Memo::default(), projection: ProjectionWorker::default(), selection: Mutex::default() }
    }

    /// The plot for `rect` minus the title row, fitted from the newest finished projection (possibly stale).
    pub(super) fn frame(&self, ctx: &Ctx, rect: Rect) -> GenreMapFrame {
        let (w, h) = (rect.width(), rect.height().saturating_sub(1));
        let emb_gen = ctx.s.genre_embeddings_gen();
        let tracks = ctx.s.tracks_with_embedding();
        let done = self.projection.latest(emb_gen, &tracks);
        let fit_key = (done.as_ref().map(|(g, _)| *g), w, h);
        let plot = self.plot.get_or_build((ctx.s.embedded_tracks_gen(), fit_key), || {
            let fitted = self.fit.get_or_build(fit_key, || {
                Arc::new(done.as_ref().map(|(_, proj)| fit_to_pane(proj, w, h)).unwrap_or_default())
            });
            Arc::new(Plot::new(&tracks, &fitted, w, h))
        });
        let frame = GenreMapFrame { plot, computing: fit_key.0 != Some(emb_gen) };
        let plotted = |id: TrackId| frame.plot.index.contains_key(&id);
        let mut sel = self.selection.locked();
        if let Some(id) = sel.pending
            && (plotted(id) || !frame.computing)
        {
            sel.pending = None;
            if plotted(id) {
                sel.selected = Some(id);
            }
        }
        if sel.pending.is_none() && !sel.selected.is_some_and(plotted) {
            sel.selected = frame.plot.points.first().map(|p| p.track.id);
        }
        drop(sel);
        frame
    }

    pub(super) fn selected_track(&self) -> Option<TrackId> {
        self.selection.locked().selected
    }

    /// Applied by the next `frame` that plots `track`; dropped if a current frame doesn't.
    pub(super) fn select(&self, track: TrackId) {
        self.selection.locked().pending = Some(track);
    }

    /// Moves the selection to the nearest plotted point in `dir` at least `reach` cells away, else the farthest one that way.
    pub(super) fn nav(&self, frame: &GenreMapFrame, dir: Key, reach: isize) {
        let mut sel = self.selection.locked();
        sel.pending = None;
        let Some(cur) = sel.selected.and_then(|id| frame.plot.point(id)) else { return };
        let (cx, cy) = (cur.x as isize, cur.y as isize);
        let cur_id = cur.track.id;
        let ahead: Vec<_> = frame
            .plot
            .points
            .iter()
            .filter(|p| p.track.id != cur_id)
            .map(|p| (p, axes(dir, cx, cy, p.x as isize, p.y as isize)))
            .filter(|(_, (primary, _))| *primary > 0)
            .collect();
        let score = |&&(_, (primary, perp)): &&(&Point, (isize, isize))| primary * primary + perp * perp * 4;
        let next = ahead.iter().filter(|(_, (primary, _))| *primary >= reach).min_by_key(score).or_else(|| ahead.iter().max_by_key(|(_, (primary, _))| *primary));
        if let Some((p, _)) = next {
            sel.selected = Some(p.track.id);
        }
    }

    /// Selects the nearest point to `(x, y)` (which excludes the title row); returns it when this
    /// click completes a double-click on it.
    pub(super) fn click(&self, frame: &GenreMapFrame, x: usize, y: usize) -> Option<TrackId> {
        let id = frame
            .plot
            .points
            .iter()
            .min_by_key(|p| {
                let (dx, dy) = (p.x as isize - x as isize, p.y as isize - y as isize);
                dx * dx + dy * dy
            })?
            .track
            .id;
        let mut sel = self.selection.locked();
        sel.pending = None;
        sel.selected = Some(id);
        let now = Instant::now();
        let double = sel.last_click.is_some_and(|(t, last_id)| last_id == id && now.duration_since(t) <= DOUBLE_CLICK_WINDOW);
        sel.last_click = (!double).then_some((now, id));
        double.then_some(id)
    }

    /// `now_playing` and `liked` are passed live so a track or like change doesn't rebuild the frame.
    pub(super) fn draw(
        &self,
        printer: &Printer,
        focused: bool,
        frame: &GenreMapFrame,
        now_playing: Option<TrackId>,
        liked: &std::collections::HashSet<TrackId>,
    ) {
        let title = if focused { "[Genre Map]" } else { "Genre Map" };
        let title = if frame.computing { format!("{title} (computing…)") } else { title.to_string() };
        printer.with_color(ColorStyle::title_secondary(), |p| {
            p.print((0, 0), &crate::view::pad(&title, p.size.x));
        });
        let plot = &frame.plot;
        if printer.size.x == 0 || printer.size.y <= 1 || plot.w != printer.size.x || plot.h != printer.size.y.saturating_sub(1) {
            return; // not yet rebuilt for this size
        }
        let selected = self.selected_track();
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let flash_on = (millis / FLASH_MS) % 2 == 1;
        for members in plot.by_cell.chunk_by(|&a, &b| plot.cell(a) == plot.cell(b)) {
            // Draw one point per cell by priority, so a plain point can't paint over a highlighted one.
            let winner = members
                .iter()
                .map(|&i| &plot.points[i])
                .max_by_key(|p| cell_priority(Some(p.track.id) == selected, now_playing == Some(p.track.id)))
                .expect("cell has at least one point");
            let id = winner.track.id;
            let is_selected = Some(id) == selected;
            let is_playing = now_playing == Some(id);
            let white = Color::Dark(cursive::theme::BaseColor::White);
            let style = if is_selected { ColorStyle::new(white, winner.color) } else { ColorStyle::new(winner.color, Color::TerminalDefault) };
            let glyph = if is_playing && flash_on {
                PLAYING_GLYPH
            } else if is_selected {
                SELECTED_GLYPH
            } else if liked.contains(&id) {
                LIKED_GLYPH
            } else {
                UNLIKED_GLYPH
            };
            let effect = if is_playing || members.len() > 1 { Effect::Bold } else { Effect::Simple };
            printer.with_effect(effect, |p| p.with_color(style, |p| p.print((winner.x, winner.y + 1), glyph)));
        }
    }
}

/// Playing outranks selected: the cursor is transient, what's playing should always show.
fn cell_priority(is_selected: bool, is_playing: bool) -> u8 {
    match (is_playing, is_selected) {
        (true, true) => 3,
        (true, false) => 2,
        (false, true) => 1,
        (false, false) => 0,
    }
}

/// An unselected point for a track the user has liked.
const LIKED_GLYPH: &str = "•";
/// An unselected point for a track that isn't liked.
const UNLIKED_GLYPH: &str = "★";
/// The selected point, regardless of liked status.
const SELECTED_GLYPH: &str = "■";
/// The now-playing point's flash, alternating with its normal glyph.
const PLAYING_GLYPH: &str = "●";
const FLASH_MS: u128 = 500;

/// `(distance toward dir, perpendicular drift)` from `(cx, cy)` to `(x, y)`; a point on the `dir` side has a positive first.
fn axes(dir: Key, cx: isize, cy: isize, x: isize, y: isize) -> (isize, isize) {
    match dir {
        Key::Up => (cy - y, x - cx),
        Key::Down => (y - cy, x - cx),
        Key::Left => (cx - x, y - cy),
        Key::Right => (x - cx, y - cy),
        _ => (0, 0),
    }
}

/// Each embedded track's 2D PCA coordinates, before fitting to a pane.
type Projection = Vec<(TrackId, (f32, f32))>;

/// The projection and the `Session::genre_embeddings_gen` it was computed for.
type Finished = Arc<Mutex<Option<(u64, Arc<Projection>)>>>;

type Job = (u64, Arc<Vec<EmbeddedTrack>>);

/// Computes projections off the UI thread, spawned on first use; lives as long as the app.
#[derive(Default)]
struct ProjectionWorker {
    worker: Mutex<Worker>,
    done: Finished,
}

#[derive(Default)]
enum Worker {
    #[default]
    Idle,
    /// Its job queue and the embeddings gen last sent on it.
    Running(mpsc::Sender<Job>, u64),
    /// Not respawned: it only dies by panicking in `project`, which would recur.
    Dead,
}

impl ProjectionWorker {
    fn spawn(done: Finished) -> mpsc::Sender<Job> {
        let (jobs, rx) = mpsc::channel::<Job>();
        thread::spawn(move || {
            let mut basis = None;
            while let Ok(mut job) = rx.recv() {
                // A scan burst queues many; only the newest matters.
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                let projection = Arc::new(project(&job.1, &mut basis));
                *done.locked() = Some((job.0, projection));
            }
        });
        jobs
    }

    /// The last finished projection; queues `emb_gen`'s if that isn't it. The baseline redraw picks it up.
    fn latest(&self, emb_gen: u64, tracks: &Arc<Vec<EmbeddedTrack>>) -> Option<(u64, Arc<Projection>)> {
        let done = self.done.locked().clone();
        if done.as_ref().is_some_and(|(g, _)| *g == emb_gen) {
            return done;
        }
        let mut worker = self.worker.locked();
        let tx = match &*worker {
            Worker::Running(_, sent) if *sent == emb_gen => return done,
            Worker::Dead => return done,
            Worker::Running(tx, _) => tx.clone(),
            Worker::Idle => Self::spawn(self.done.clone()),
        };
        *worker = match tx.send((emb_gen, tracks.clone())) {
            Ok(()) => Worker::Running(tx, emb_gen),
            Err(e) => {
                log::error!("genre map projection worker is gone: {e}");
                Worker::Dead
            }
        };
        done
    }
}

/// `basis` is the previous job's PC1/PC2, warm-starting this one's and replaced by them unless either vanished.
fn project(tracks: &[EmbeddedTrack], basis: &mut Option<(Vec<f32>, Vec<f32>)>) -> Projection {
    // Majority dimension wins so a stale/corrupt embedding of another length can't index out of bounds.
    let mut dim_votes: HashMap<usize, usize> = HashMap::new();
    for e in tracks {
        *dim_votes.entry(e.embedding.len()).or_insert(0) += 1;
    }
    let Some(dim) = dim_votes.into_iter().max_by_key(|&(_, votes)| votes).map(|(dim, _)| dim) else {
        return Projection::new();
    };
    let rows: Vec<(TrackId, &[f32])> =
        tracks.iter().filter(|e| e.embedding.len() == dim).map(|e| (e.track.id, &*e.embedding)).collect();
    let mut mean = vec![0.0f32; dim];
    for (_, row) in &rows {
        for (m, x) in mean.iter_mut().zip(*row) {
            *m += x;
        }
    }
    for m in &mut mean {
        *m /= rows.len() as f32;
    }
    let (init1, init2) = basis.take().filter(|(pc1, _)| pc1.len() == dim).unwrap_or_else(|| (vec![1.0; dim], vec![1.0; dim]));
    let (pc1, ok1) = power_iteration(&rows, &mean, init1, None);
    let (pc2, ok2) = power_iteration(&rows, &mean, init2, Some(&pc1));
    let (m1, m2) = (dot(&mean, &pc1), dot(&mean, &pc2));
    let projection = rows.iter().map(|&(id, r)| (id, (dot(r, &pc1) - m1, dot(r, &pc2) - m2))).collect();
    // A vanished PC (e.g. a zero PC2 from identical embeddings) would pin every later job to it.
    *basis = (ok1 && ok2).then_some((pc1, pc2));
    projection
}

/// Each projected track's pane cell: rotated to suit the pane's aspect, then each axis stretched
/// independently to fill it (by design).
fn fit_to_pane(projection: &Projection, w: usize, h: usize) -> Fitted {
    let (w1, h1) = ((w.saturating_sub(1)) as f32, (h.saturating_sub(1)) as f32);
    if w == 0 || h == 0 || projection.is_empty() {
        return HashMap::new();
    }
    if let [(id, _)] = projection.as_slice() {
        return HashMap::from([(*id, (w1 as usize / 2, h1 as usize / 2))]);
    }

    let proj: Vec<(f32, f32)> = projection.iter().map(|&(_, p)| p).collect();
    let angle = best_angle(&proj, w1, h1);
    let (cos_t, sin_t) = (angle.cos(), angle.sin());
    let rotated: Vec<(f32, f32)> = proj.iter().map(|&(x, y)| rotate(x, y, cos_t, sin_t)).collect();
    let (xlo, xhi) = min_max(rotated.iter().map(|p| p.0));
    let (ylo, yhi) = min_max(rotated.iter().map(|p| p.1));
    let (dx, dy) = (xhi - xlo, yhi - ylo);
    // A collapsed axis (duplicate embeddings) would divide by ~0.
    let sx = if dx > 1e-6 { w1 / dx } else { 1.0 };
    let sy = if dy > 1e-6 { h1 / dy } else { 1.0 };
    let (cx, cy) = ((xlo + xhi) / 2.0, (ylo + yhi) / 2.0);
    let (pcx, pcy) = (w1 / 2.0, h1 / 2.0);
    projection
        .iter()
        .zip(&rotated)
        .map(|(&(id, _), &(x, y))| {
            let px = (pcx + (x - cx) * sx).round().clamp(0.0, w1);
            let py = (pcy + (y - cy) * sy).round().clamp(0.0, h1);
            (id, (px as usize, py as usize))
        })
        .collect()
}

/// Rotates `(x, y)` by `-θ`.
fn rotate(x: f32, y: f32, cos_t: f32, sin_t: f32) -> (f32, f32) {
    (x * cos_t + y * sin_t, -x * sin_t + y * cos_t)
}

/// How far a `dx`×`dy` box can scale without overflowing the pane.
fn fit_scale(dx: f32, dy: f32, w1: f32, h1: f32) -> f32 {
    let sx = if dx > 1e-6 { w1 / dx } else { f32::INFINITY };
    let sy = if dy > 1e-6 { h1 / dy } else { f32::INFINITY };
    sx.min(sy)
}

/// Samples over `0..π`, the bounding box's full period; golden-section refines the best one.
const ANGLE_SAMPLES: usize = 64;

/// The optimum can sit at a crossover of the two ratios, not on a hull edge, hence a dense sweep.
fn best_angle(points: &[(f32, f32)], w1: f32, h1: f32) -> f32 {
    let step = std::f32::consts::PI / ANGLE_SAMPLES as f32;
    let mut best = 0.0f32;
    let mut best_fit = f32::NEG_INFINITY;
    for i in 0..ANGLE_SAMPLES {
        let theta = i as f32 * step;
        let fit = fit_at_angle(points, theta, w1, h1);
        if fit > best_fit {
            best_fit = fit;
            best = theta;
        }
    }
    // ±2 steps: the peak can straddle a ±1-step window's edge.
    golden_section_refine(points, best - 2.0 * step, best + 2.0 * step, w1, h1)
}

/// `fit_scale` of `points` rotated by `theta`.
fn fit_at_angle(points: &[(f32, f32)], theta: f32, w1: f32, h1: f32) -> f32 {
    let (cos_t, sin_t) = (theta.cos(), theta.sin());
    let rotated = points.iter().map(|&(x, y)| rotate(x, y, cos_t, sin_t));
    let (mut xlo, mut xhi) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut ylo, mut yhi) = (f32::INFINITY, f32::NEG_INFINITY);
    for (x, y) in rotated {
        xlo = xlo.min(x);
        xhi = xhi.max(x);
        ylo = ylo.min(y);
        yhi = yhi.max(y);
    }
    fit_scale(xhi - xlo, yhi - ylo, w1, h1)
}

/// Golden-section search maximizing `fit_at_angle` over `[lo, hi]`.
fn golden_section_refine(points: &[(f32, f32)], mut lo: f32, mut hi: f32, w1: f32, h1: f32) -> f32 {
    let gr = (5f32.sqrt() - 1.0) / 2.0;
    let mut c = hi - gr * (hi - lo);
    let mut d = lo + gr * (hi - lo);
    for _ in 0..20 {
        if fit_at_angle(points, c, w1, h1) < fit_at_angle(points, d, w1, h1) {
            lo = c;
        } else {
            hi = d;
        }
        c = hi - gr * (hi - lo);
        d = lo + gr * (hi - lo);
    }
    (lo + hi) / 2.0
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

/// Top covariance eigenvector orthogonal to `ortho`, and false if the iterate vanished.
fn power_iteration(rows: &[(TrackId, &[f32])], mean: &[f32], mut v: Vec<f32>, ortho: Option<&[f32]>) -> (Vec<f32>, bool) {
    let deflate = |v: &mut [f32]| {
        if let Some(p) = ortho {
            let d = dot(v, p);
            for (x, q) in v.iter_mut().zip(p) {
                *x -= d * q;
            }
        }
    };
    let n = rows.len().max(1) as f32;
    deflate(&mut v);
    normalize(&mut v);
    let mut next = vec![0.0f32; v.len()];
    for _ in 0..40 {
        next.fill(0.0);
        let mv = dot(mean, &v);
        let mut total = 0.0f32;
        for (_, row) in rows {
            let s = dot(row, &v) - mv;
            total += s;
            for (nx, r) in next.iter_mut().zip(*row) {
                *nx += s * r;
            }
        }
        // Σ s·row − (Σ s)·mean is Σ s·(row − mean).
        for (nx, m) in next.iter_mut().zip(mean) {
            *nx = (*nx - total * m) / n;
        }
        deflate(&mut next);
        if normalize(&mut next) < 1e-9 {
            return (v, false); // no variance left along the iterate
        }
        // Covariance is PSD, so the iterate never flips sign.
        let delta = next.iter().zip(&v).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        std::mem::swap(&mut v, &mut next);
        if delta < 1e-5 {
            break;
        }
    }
    (v, true)
}

