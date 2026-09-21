use cursive::Printer;
use core::ItemKind;

use super::rows::TITLE_MARGIN;
use super::text::{active_style, in_span};

/// Columns right of the kind bar reserved for the title or search entry.
const QUERY_MIN: usize = 12;

/// Which result kinds a list shows.
#[derive(Clone, Copy, Default, PartialEq)]
pub(super) enum KindFilter {
    #[default]
    All,
    Songs,
    Albums,
    Playlists,
}

impl KindFilter {
    pub(super) const ALL: [Self; 4] = [Self::All, Self::Songs, Self::Albums, Self::Playlists];

    pub(super) const COLLECTIONS: [Self; 3] = [Self::All, Self::Albums, Self::Playlists];

    /// The kind after `self` in `among`, wrapping.
    pub(super) fn next(self, among: &[Self]) -> Self {
        let at = among.iter().position(|&k| k == self).unwrap_or(0);
        among[(at + 1) % among.len()]
    }

    fn title(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Songs => "Songs",
            Self::Albums => "Albums",
            Self::Playlists => "Playlists",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Songs => "songs",
            Self::Albums => "albums",
            Self::Playlists => "playlists",
        }
    }

    /// The collection noun a Playlists top level counts under this filter.
    pub(super) fn unit(self, count: usize) -> &'static str {
        match (self, count == 1) {
            (Self::Albums, true) => "album",
            (Self::Albums, false) => "albums",
            (_, true) => "playlist",
            (_, false) => "playlists",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Songs => "Songs",
            Self::Albums => "Alb",
            Self::Playlists => "Pl",
        }
    }

    pub(super) fn shows_tracks(self) -> bool {
        matches!(self, Self::All | Self::Songs)
    }

    pub(super) fn admits(self, kind: ItemKind) -> bool {
        match self {
            Self::All => true,
            Self::Songs => false,
            Self::Albums => kind == ItemKind::Album,
            Self::Playlists => kind == ItemKind::Playlist,
        }
    }
}

/// The row prefix noun of a collection kind.
pub(super) fn noun(kind: ItemKind) -> &'static str {
    if kind == ItemKind::Album { "album" } else { "playlist" }
}

/// One laid-out segment: its kind, start column, width and text.
pub(super) struct Segment {
    pub(super) kind: KindFilter,
    start: usize,
    width: usize,
    text: String,
}

/// `kinds`' segments right-aligned in a `content_w` title row, dropping counts, then short labels, then the inactive kinds as room shrinks; `counts` runs parallel to `kinds`.
pub(super) fn layout(content_w: usize, kinds: &[KindFilter], counts: &[usize], active: KindFilter) -> Vec<Segment> {
    let room = content_w.saturating_sub(1 + TITLE_MARGIN + QUERY_MIN);
    let seg = |kind: KindFilter, n: usize, short: bool, count: bool| {
        let name = if short { kind.short() } else { kind.title() };
        if count { format!(" {name} {n} ") } else { format!(" {name} ") }
    };
    let width = |segs: &[(KindFilter, String)]| segs.iter().map(|(_, t)| t.chars().count()).sum::<usize>();
    let tiers = [(false, true), (false, false), (true, false)];
    let mut segs: Vec<(KindFilter, String)> = Vec::new();
    for (short, count) in tiers {
        segs = kinds.iter().zip(counts).map(|(&kind, &n)| (kind, seg(kind, n, short, count))).collect();
        if width(&segs) <= room {
            break;
        }
    }
    if width(&segs) > room {
        let i = kinds.iter().position(|&k| k == active).unwrap_or(0);
        segs = vec![(active, seg(active, counts.get(i).copied().unwrap_or(0), true, true))];
    }
    if width(&segs) > room {
        return Vec::new();
    }
    let mut x = content_w.saturating_sub(TITLE_MARGIN + width(&segs));
    segs.into_iter()
        .map(|(kind, text)| {
            let w = text.chars().count();
            x += w;
            Segment { kind, start: x - w, width: w, text }
        })
        .collect()
}

/// The bar's first column, if any segment is shown.
pub(super) fn start(segs: &[Segment]) -> Option<usize> {
    segs.first().map(|seg| seg.start)
}

pub(super) fn draw(printer: &Printer, segs: &[Segment], active: KindFilter) {
    for seg in segs {
        if seg.kind == active {
            printer.with_color(active_style(), |p| p.print((seg.start, 0), &seg.text));
        } else {
            printer.print((seg.start, 0), &seg.text);
        }
    }
}

/// The kind whose segment covers column `x`.
pub(super) fn hit(segs: &[Segment], x: usize) -> Option<KindFilter> {
    segs.iter().find(|seg| in_span(x, (seg.start, seg.width))).map(|seg| seg.kind)
}
