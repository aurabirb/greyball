use cursive::Printer;
use cursive::theme::{BaseColor, Color, ColorStyle};

use unicode_width::UnicodeWidthStr;

use core::PlayerState;

use crate::screen::Screen;

use super::text::{in_span, scroll_title};
use super::transport::{TRANSPORT_GAP, Transport, transport_labels, transport_layout};

/// Background for the active tab only — every other tab uses the terminal's default colors, unstyled.
const ACTIVE_TAB_BG: Color = Color::Dark(BaseColor::Red);

/// A tab's rendered button text.
fn tab_label(screen: Screen, collapsed: bool) -> String {
    let name = screen.label();
    if collapsed {
        let letter = name.chars().next().unwrap_or('?');
        format!(" {letter} ")
    } else {
        format!(" [{}] {name} ", screen.digit())
    }
}

/// Each tab's screen, start column and width, from column 0 with a 1-column gap between tabs.
fn tab_layout(collapsed: bool) -> Vec<(Screen, usize, usize)> {
    let mut x = 0;
    Screen::ALL
        .iter()
        .map(|&screen| {
            let start = x;
            let w = tab_label(screen, collapsed).chars().count();
            x += w + 1;
            (screen, start, w)
        })
        .collect()
}

/// What a click on the tab bar landed on.
pub(super) enum TabBarHit {
    Tab(Screen),
    Transport(Transport),
}

/// Row 0 of the screen: the screen tabs, the transport buttons, then the now-playing marquee.
pub(super) struct TabBar<'a> {
    pub(super) active: Screen,
    pub(super) state: &'a PlayerState,
}

/// Where everything sits in a bar of a given width, for both drawing and hit-testing.
struct Layout {
    collapsed: bool,
    /// Each visible tab's screen, start column and (clipped) width.
    tabs: Vec<(Screen, usize, usize)>,
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
            .filter(|&(_, start, _)| start < content_w)
            .map(|(screen, start, w)| (screen, start, w.min(content_w - start)))
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
        for &(screen, start, w) in &layout.tabs {
            let text: String = tab_label(screen, layout.collapsed).chars().take(w).collect();
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
        layout.tabs.iter().find(|&&(_, s, w)| in_span(x, (s, w))).map(|&(screen, ..)| TabBarHit::Tab(screen))
    }
}
