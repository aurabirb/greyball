//! `:genre-map` pane: tracks scattered by a 2D PCA projection of their genre embedding, colored by
//! BPM. The PCA projection is computed on a per-pane worker thread; `frame` fits the last finished one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
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
    computing: bool,
}

impl GenreMapFrame {
    pub(super) fn track_label(&self, id: TrackId) -> Option<String> {
        self.points.iter().find(|p| p.track_id == id).map(|p| format!("{} - {}", p.artist, p.title))
    }

    /// Plays `id` with the plotted points as the context, like a `TrackList` row's Enter.
    pub(super) fn play_context(&self, id: TrackId) -> Option<Command> {
        let index = self.points.iter().position(|p| p.track_id == id)?;
        let tracks = self.points.iter().map(|p| p.track_id).collect();
        Some(Command::PlayContext { tracks, index, remote: None, local: None, name: Some("Genre Map".to_string()) })
    }
}

/// Same click-timing window `TrackList` uses for its own double-click detection.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Embedded-tracks gen, embeddings gen, the finished projection's gen, `w`, `h`.
type FrameKey = (u64, u64, Option<u64>, usize, usize);

pub(super) struct GenreMap {
    cache: Memo<FrameKey, Arc<GenreMapFrame>>,
    projection: ProjectionWorker,
    selected: Mutex<Option<TrackId>>,
    /// Selected once plotted; outranks the first-point fallback until a current frame resolves it.
    pending: Mutex<Option<TrackId>>,
    last_click: Mutex<Option<(Instant, TrackId)>>,
}

impl GenreMap {
    pub(super) fn new() -> Self {
        Self {
            cache: Memo::default(),
            projection: ProjectionWorker::default(),
            selected: Mutex::new(None),
            pending: Mutex::new(None),
            last_click: Mutex::new(None),
        }
    }

    /// The plot for `rect` minus the title row, fitted from the newest finished projection (possibly stale).
    pub(super) fn frame(&self, ctx: &Ctx, rect: Rect) -> Arc<GenreMapFrame> {
        let (w, h) = (rect.width(), rect.height().saturating_sub(1));
        let emb_gen = ctx.s.genre_embeddings_gen();
        let tracks = ctx.s.tracks_with_embedding();
        let done = self.projection.latest(emb_gen, &tracks);
        let done_gen = done.as_ref().map(|(g, _)| *g);
        let frame = self.cache.get_or_build((ctx.s.embedded_tracks_gen(), emb_gen, done_gen, w, h), || {
            let points = done.as_ref().map(|(_, proj)| points(&tracks, &fit_to_pane(proj, w, h))).unwrap_or_default();
            Arc::new(GenreMapFrame { w, h, points, computing: done_gen != Some(emb_gen) })
        });
        let plotted = |id: TrackId| frame.points.iter().any(|p| p.track_id == id);
        let mut selected = self.selected.lock().unwrap_or_else(|e| e.into_inner());
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = *pending
            && (plotted(id) || !frame.computing)
        {
            *pending = None;
            if plotted(id) {
                *selected = Some(id);
            }
        }
        if pending.is_none() && !selected.is_some_and(plotted) {
            *selected = frame.points.first().map(|p| p.track_id);
        }
        frame
    }

    pub(super) fn selected_track(&self) -> Option<TrackId> {
        *self.selected.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read live rather than memoized: liking a track shouldn't rebuild the frame.
    pub(super) fn liked_ids(&self, ctx: &Ctx, frame: &GenreMapFrame) -> std::collections::HashSet<TrackId> {
        frame.points.iter().filter(|p| ctx.s.liked_mark(p.track_id).is_some()).map(|p| p.track_id).collect()
    }

    /// Applied by the next `frame` that plots `track`; dropped if a current frame doesn't.
    pub(super) fn select(&self, track: TrackId) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(track);
    }

    fn clear_pending(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Moves the selection to the nearest plotted point in `dir`.
    pub(super) fn nav(&self, frame: &GenreMapFrame, dir: Key) {
        self.clear_pending();
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

    /// `(x, y)` excludes the title row.
    fn select_at(&self, frame: &GenreMapFrame, x: usize, y: usize) -> Option<TrackId> {
        let nearest = frame.points.iter().min_by_key(|p| {
            let (dx, dy) = (p.x as isize - x as isize, p.y as isize - y as isize);
            dx * dx + dy * dy
        })?;
        self.clear_pending();
        *self.selected.lock().unwrap_or_else(|e| e.into_inner()) = Some(nearest.track_id);
        Some(nearest.track_id)
    }

    /// Selects the nearest point; returns it when this click completes a double-click on it.
    pub(super) fn click(&self, frame: &GenreMapFrame, x: usize, y: usize) -> Option<TrackId> {
        let id = self.select_at(frame, x, y)?;
        let now = Instant::now();
        let mut last_click = self.last_click.lock().unwrap_or_else(|e| e.into_inner());
        let double = last_click.is_some_and(|(t, last_id)| last_id == id && now.duration_since(t) <= DOUBLE_CLICK_WINDOW);
        *last_click = (!double).then_some((now, id));
        double.then_some(id)
    }

    /// `now_playing` is passed live so a track change doesn't rebuild the frame.
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
        if printer.size.x == 0 || printer.size.y <= 1 || frame.w != printer.size.x || frame.h != printer.size.y.saturating_sub(1) {
            return; // not yet rebuilt for this size
        }
        let selected = self.selected_track();
        // Draw one point per cell by priority, so a plain point can't paint over a highlighted one.
        let mut cells: std::collections::HashMap<(usize, usize), Vec<&Point>> = std::collections::HashMap::new();
        for point in &frame.points {
            cells.entry((point.x, point.y)).or_default().push(point);
        }
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let ring = PLAYING_RING[(millis / 400) as usize % PLAYING_RING.len()];
        for (&(x, y), points) in &cells {
            let clustered = points.len() > 1;
            let winner = points
                .iter()
                .max_by_key(|p| cell_priority(Some(p.track_id) == selected, now_playing == Some(p.track_id)))
                .expect("cell has at least one point");
            let is_selected = Some(winner.track_id) == selected;
            let is_playing = now_playing == Some(winner.track_id);
            let is_liked = liked.contains(&winner.track_id);
            let white = Color::Dark(cursive::theme::BaseColor::White);
            let style = if is_selected { ColorStyle::new(white, winner.color) } else { ColorStyle::new(winner.color, Color::TerminalDefault) };
            let base = if is_selected { SELECTED_GLYPH } else if is_liked { LIKED_GLYPH } else { UNLIKED_GLYPH };
            let mut glyph = base.to_string();
            let effect = if is_playing || clustered { Effect::Bold } else { Effect::Simple };
            if is_playing {
                // Combining mark: zero-width, so it stacks onto the base glyph's cell.
                glyph.push(ring);
            }
            printer.with_effect(effect, |p| p.with_color(style, |p| p.print((x, y + 1), &glyph)));
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

/// Lower is better; perpendicular drift weighs more so "roughly that way" beats far-but-aligned.
fn direction_score(dir: Key, cx: isize, cy: isize, x: isize, y: isize) -> isize {
    let (primary, perp) = match dir {
        Key::Up | Key::Down => (y - cy, x - cx),
        Key::Left | Key::Right => (x - cx, y - cy),
        _ => return isize::MAX,
    };
    primary * primary + perp * perp * 4
}

/// Each embedded track's 2D PCA coordinates, before fitting to a pane.
type Projection = Vec<(TrackId, (f32, f32))>;

/// The projection and the `Session::genre_embeddings_gen` it was computed for.
type Finished = Arc<Mutex<Option<(u64, Arc<Projection>)>>>;

/// Computes projections off the UI thread, spawned on first use; lives as long as the app.
#[derive(Default)]
struct ProjectionWorker {
    jobs: OnceLock<mpsc::Sender<(u64, Arc<Vec<Track>>)>>,
    requested: Mutex<Option<u64>>,
    done: Finished,
}

impl ProjectionWorker {
    fn spawn(done: Finished) -> mpsc::Sender<(u64, Arc<Vec<Track>>)> {
        let (jobs, rx) = mpsc::channel::<(u64, Arc<Vec<Track>>)>();
        thread::spawn(move || {
            while let Ok(mut job) = rx.recv() {
                // A scan burst queues many; only the newest matters.
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                let projection = Arc::new(project(&job.1));
                *done.lock().unwrap_or_else(|e| e.into_inner()) = Some((job.0, projection));
            }
        });
        jobs
    }

    /// The last finished projection; queues `emb_gen`'s if that isn't it. The baseline redraw picks it up.
    fn latest(&self, emb_gen: u64, tracks: &Arc<Vec<Track>>) -> Option<(u64, Arc<Projection>)> {
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut requested = self.requested.lock().unwrap_or_else(|e| e.into_inner());
        if done.as_ref().map(|(g, _)| *g) != Some(emb_gen) && *requested != Some(emb_gen) {
            let jobs = self.jobs.get_or_init(|| Self::spawn(self.done.clone()));
            if let Err(e) = jobs.send((emb_gen, tracks.clone())) {
                log::error!("genre map projection worker is gone: {e}");
            }
            *requested = Some(emb_gen);
        }
        done
    }
}

fn points(tracks: &[Track], cells: &HashMap<TrackId, (usize, usize)>) -> Vec<Point> {
    tracks
        .iter()
        .filter_map(|track| {
            let &(x, y) = cells.get(&track.id)?;
            let color = track.attrs.get("bpm").and_then(|bpm| bpm_color(bpm)).unwrap_or(NEUTRAL_COLOR);
            Some(Point { track_id: track.id, title: track.title.clone(), artist: track.display_artist(), x, y, color })
        })
        .collect()
}

fn project(tracks: &[Track]) -> Projection {
    let decoded: Vec<(&Track, Vec<f32>)> =
        tracks.iter().filter_map(|t| t.attrs.get(GENRE_EMBEDDING_ATTR).and_then(|s| decode_genre_embedding(s)).map(|e| (t, e))).collect();
    // Majority dimension wins so a stale/corrupt embedding of another length can't index out of bounds.
    let mut dim_votes: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (_, e) in &decoded {
        if !e.is_empty() {
            *dim_votes.entry(e.len()).or_insert(0) += 1;
        }
    }
    let Some(dim) = dim_votes.into_iter().max_by_key(|&(_, votes)| votes).map(|(dim, _)| dim) else {
        return Projection::new();
    };
    let decoded: Vec<(&Track, Vec<f32>)> = decoded.into_iter().filter(|(_, e)| e.len() == dim).collect();
    let n = decoded.len() as f32;
    let mean: Vec<f32> = (0..dim).map(|d| decoded.iter().map(|(_, e)| e[d]).sum::<f32>() / n).collect();
    let centered: Vec<Vec<f32>> = decoded.iter().map(|(_, e)| e.iter().zip(&mean).map(|(x, m)| x - m).collect()).collect();

    let (pc1, pc2) = pca_top2(&centered, dim);
    decoded.iter().zip(&centered).map(|((track, _), v)| (track.id, (dot(v, &pc1), dot(v, &pc2)))).collect()
}

/// Each projected track's pane cell: rotated to suit the pane's aspect, then each axis stretched
/// independently to fill it (by design).
fn fit_to_pane(projection: &Projection, w: usize, h: usize) -> HashMap<TrackId, (usize, usize)> {
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

/// Top covariance eigenvector without materializing the d×d matrix.
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
            break; // degenerate: fewer than 2 distinct points
        }
        // Covariance is PSD, so the iterate never flips sign.
        let delta = next.iter().zip(&v).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        v = next;
        if delta < 1e-5 {
            break;
        }
    }
    v
}

/// PC2 by deflating PC1 out of every row, else power iteration would rediscover PC1.
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
