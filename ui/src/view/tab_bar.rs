use cursive::Printer;
use cursive::theme::{BaseColor, Color, ColorStyle};

use unicode_width::UnicodeWidthStr;

use core::PlayerState;

use super::{HIST, NOW_PLAYING, PLAYLISTS, QUEUE, SEARCH};
use super::text::scroll_title;
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
pub(super) fn tab_label(index: usize, name: &str, collapsed: bool) -> String {
    if collapsed {
        let letter = name.chars().next().unwrap_or('?');
        format!(" {letter} ")
    } else {
        format!(" [{}] {name} ", index + 1)
    }
}

/// Total width of every tab label plus the gaps between them, for the given `collapsed` mode.
fn tabs_width(collapsed: bool) -> usize {
    let gap = 1;
    TABS.iter().enumerate().map(|(i, &(_, label))| tab_label(i, label, collapsed).chars().count()).sum::<usize>()
        + gap * TABS.len().saturating_sub(1)
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

/// Which tab (if any) occupies column `x` of a tab bar `width` columns wide.
pub(super) fn tab_at_x(x: usize, width: usize, state: &PlayerState) -> Option<usize> {
    let collapsed = tab_bar_collapsed(width, state);
    tab_layout(collapsed)
        .into_iter()
        .find(|&(_, start, w)| x >= start && x < start + w && start < width)
        .map(|(screen, ..)| screen)
}

/// Whether the tab labels must collapse to single letters to leave room for the transport strip.
fn tab_bar_collapsed(content_w: usize, state: &PlayerState) -> bool {
    let gap = TRANSPORT_GAP;
    let transport_w = transport_layout(0, state).last().map_or(0, |&(_, s, w)| s + w);
    tabs_width(false) + gap + transport_w > content_w
}

/// Which transport button occupies column `x` of a tab bar `width` columns wide.
pub(super) fn transport_at_x(x: usize, width: usize, state: &PlayerState) -> Option<Transport> {
    let collapsed = tab_bar_collapsed(width, state);
    let tabs_end = tab_layout(collapsed).last().map_or(0, |&(_, start, w)| start + w);
    let start = tabs_end + TRANSPORT_GAP;
    let layout = transport_layout(start, state);
    let end = layout.last().map_or(start, |&(_, s, w)| s + w);
    if end > width {
        return None;
    }
    layout.into_iter().find(|&(_, s, w)| x >= s && x < s + w).map(|(b, ..)| b)
}

/// Row 0 of the whole screen.
pub(super) fn draw_tab_bar(
    printer: &Printer,
    active: usize,
    marquee: &str,
    marquee_offset: usize,
    player_state: &PlayerState,
) {
    let content_w = printer.size.x.saturating_sub(1);
    let collapsed = tab_bar_collapsed(content_w, player_state);
    let layout = tab_layout(collapsed);

    for (i, &(screen, start, w)) in layout.iter().enumerate() {
        if start >= content_w {
            continue;
        }
        let label = TABS[i].1;
        let text: String = tab_label(i, label, collapsed).chars().take(w.min(content_w - start)).collect();
        if screen == active {
            let style = ColorStyle::new(Color::Dark(BaseColor::White), ACTIVE_TAB_BG);
            printer.with_color(style, |p| p.print((start, 0), &text));
        } else {
            printer.print((start, 0), &text);
        }
    }

    let tabs_end = layout.last().map_or(0, |&(_, start, w)| start + w);
    let gap = TRANSPORT_GAP;
    let transport_start = tabs_end + gap;
    let transport_labels = transport_labels(player_state);
    let transport = transport_layout(transport_start, player_state);
    let transport_end = transport.last().map_or(transport_start, |&(_, s, w)| s + w);
    let detail_start = if transport_end <= content_w {
        for ((_, label), &(_, start, _)) in transport_labels.iter().zip(transport.iter()) {
            printer.print((start, 0), label);
        }
        transport_end + gap
    } else {
        transport_start
    };
    if detail_start >= content_w {
        return;
    }
    let avail = content_w - detail_start;
    let text = scroll_title(marquee, avail, marquee_offset);
    if text.is_empty() {
        return;
    }
    let start = content_w - text.width();
    printer.with_color(ColorStyle::title_primary(), |p| p.print((start, 0), &text));
}
