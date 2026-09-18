use cursive::Printer;
use cursive::theme::{BaseColor, Color, ColorStyle};

use unicode_width::UnicodeWidthStr;

use core::PlayerState;

use super::{HIST, NOW_PLAYING, PLAYLISTS, QUEUE, SEARCH};
use super::text::{in_span, scroll_title};
use super::transport::{TRANSPORT_GAP, Transport, transport_labels, transport_layout};

/// The main content's row-0 tabs, `(screen, bare name)`, in both display and hotkey order.
const TABS: [(usize, &str); 5] = [
    (NOW_PLAYING, "Now Playing"),
    (PLAYLISTS, "Playlists"),
    (SEARCH, "Search"),
    (HIST, "History"),
    (QUEUE, "Queue"),
];

/// Background for the active tab only — every other tab uses the terminal's default colors, unstyled.
const ACTIVE_TAB_BG: Color = Color::Dark(BaseColor::Red);

/// `screen`'s bare tab name, shared by the tab strip and the docked Queue/History pane title.
pub(super) fn screen_name(screen: usize) -> &'static str {
    TABS.iter().find(|&&(s, _)| s == screen).map_or("", |&(_, name)| name)
}

/// A tab's rendered button text.
fn tab_label(index: usize, name: &str, collapsed: bool) -> String {
    if collapsed {
        let letter = name.chars().next().unwrap_or('?');
        format!(" {letter} ")
    } else {
        format!(" [{}] {name} ", index + 1)
    }
}

/// Each tab's screen, start column and width, from column 0 with a 1-column gap between tabs.
fn tab_layout(collapsed: bool) -> Vec<(usize, usize, usize)> {
    let widths: Vec<usize> =
        TABS.iter().enumerate().map(|(i, &(_, label))| tab_label(i, label, collapsed).chars().count()).collect();
    let gap = 1;
    let mut x = 0;
    TABS
        .iter()
        .zip(widths.iter())
        .enumerate()
        .map(|(i, (&(screen, _), &w))| {
            let start = x;
            x += w;
            if i + 1 < TABS.len() {
                x += gap;
            }
            (screen, start, w)
        })
        .collect()
}

/// What a click on the tab bar landed on.
pub(super) enum TabBarHit {
    Tab(usize),
    Transport(Transport),
}

/// Row 0 of the screen: the screen tabs, the transport buttons, then the now-playing marquee.
pub(super) struct TabBar<'a> {
    pub(super) active: usize,
    pub(super) state: &'a PlayerState,
}

/// Where everything sits in a bar of a given width, for both drawing and hit-testing.
struct Layout {
    collapsed: bool,
    /// Each visible tab's `TABS` index, start column and (clipped) width.
    tabs: Vec<(usize, usize, usize)>,
    /// Empty when the buttons don't fit.
    transport: Vec<(Transport, usize, usize)>,
    /// `(start, width)` left for the marquee.
    detail: (usize, usize),
}

impl TabBar<'_> {
    fn layout(&self, total_w: usize) -> Layout {
        // The last column stays clear, matching the list's scrollbar gutter below.
        let content_w = total_w.saturating_sub(1);
        let buttons_w = transport_layout(0, self.state).last().map_or(0, |&(_, s, w)| s + w);
        let full_w = tab_layout(false).last().map_or(0, |&(_, start, w)| start + w);
        let collapsed = full_w + TRANSPORT_GAP + buttons_w > content_w;

        let all_tabs = tab_layout(collapsed);
        let tabs_end = all_tabs.last().map_or(0, |&(_, start, w)| start + w);
        let tabs = all_tabs
            .into_iter()
            .enumerate()
            .filter(|&(_, (_, start, _))| start < content_w)
            .map(|(i, (_, start, w))| (i, start, w.min(content_w - start)))
            .collect();

        let transport_start = tabs_end + TRANSPORT_GAP;
        let transport = transport_layout(transport_start, self.state);
        let transport_end = transport.last().map_or(transport_start, |&(_, s, w)| s + w);
        let (transport, detail_start) = if transport_end <= content_w {
            (transport, transport_end + TRANSPORT_GAP)
        } else {
            (Vec::new(), transport_start)
        };
        Layout { collapsed, tabs, transport, detail: (detail_start, content_w.saturating_sub(detail_start)) }
    }

    /// Draws the bar across row 0 of `printer`, `marquee` right-aligned in whatever room is left.
    pub(super) fn draw(&self, printer: &Printer, marquee: &str, marquee_offset: usize) {
        let layout = self.layout(printer.size.x);
        for &(i, start, w) in &layout.tabs {
            let (screen, name) = TABS[i];
            let text: String = tab_label(i, name, layout.collapsed).chars().take(w).collect();
            if screen == self.active {
                let style = ColorStyle::new(Color::Dark(BaseColor::White), ACTIVE_TAB_BG);
                printer.with_color(style, |p| p.print((start, 0), &text));
            } else {
                printer.print((start, 0), &text);
            }
        }
        for ((_, label), &(_, start, _)) in transport_labels(self.state).iter().zip(&layout.transport) {
            printer.print((start, 0), label);
        }
        let (detail_start, detail_w) = layout.detail;
        let text = scroll_title(marquee, detail_w, marquee_offset);
        if !text.is_empty() {
            let start = detail_start + detail_w - text.width();
            printer.with_color(ColorStyle::title_primary(), |p| p.print((start, 0), &text));
        }
    }

    /// What a left click at column `x` of a `total_w`-wide bar landed on.
    pub(super) fn click(&self, x: usize, total_w: usize) -> Option<TabBarHit> {
        let layout = self.layout(total_w);
        if let Some(&(button, ..)) = layout.transport.iter().find(|&&(_, s, w)| in_span(x, (s, w))) {
            return Some(TabBarHit::Transport(button));
        }
        layout.tabs.iter().find(|&&(_, s, w)| in_span(x, (s, w))).map(|&(i, ..)| TabBarHit::Tab(TABS[i].0))
    }
}
