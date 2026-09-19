use cursive::Printer;
use cursive::theme::{BaseColor, Color, ColorStyle, Effect};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use core::Command;

use super::status_line::StatusLine;
use super::text::{in_span, scroll_title};
use super::transport::{TRANSPORT_GAP, Transport, transport_labels, transport_layout};

/// Background for the active tab only — every other tab uses the terminal's default colors, unstyled.
const ACTIVE_TAB_BG: Color = Color::Dark(BaseColor::Red);

/// A tab's rendered button text; `n` is its number key.
fn tab_label(n: usize, name: &str, collapsed: bool) -> String {
    if collapsed {
        let letter = name.chars().next().unwrap_or('?');
        format!(" {letter} ")
    } else {
        format!(" [{n}] {name} ")
    }
}

/// What a click on the tab bar landed on.
pub(super) enum TabBarHit {
    /// The tab at this index of `TabBar::tabs`.
    Tab(usize),
    Transport(Transport),
    /// A seek along the now-playing title.
    Title(Command),
}

/// Row 0 of the screen: a tab per tabbed window, the transport buttons, then the now-playing marquee, which doubles as a scrubber.
pub(super) struct TabBar<'a> {
    /// The tabbed windows' names, in tab order.
    pub(super) tabs: &'a [String],
    pub(super) active: usize,
    pub(super) status: &'a StatusLine,
    pub(super) marquee_offset: usize,
}

/// Where everything sits in a bar of a given width, for both drawing and hit-testing.
struct Layout {
    collapsed: bool,
    /// Each visible tab's index, start column and (clipped) width.
    tabs: Vec<(usize, usize, usize)>,
    /// Empty when the buttons don't fit.
    transport: Vec<(Transport, usize, usize)>,
    /// `(start, width)` left for the marquee.
    detail: (usize, usize),
}

impl TabBar<'_> {
    /// Each tab's index, start column and width, from column 0 with a 1-column gap between tabs.
    fn tab_layout(&self, collapsed: bool) -> Vec<(usize, usize, usize)> {
        let mut x = 0;
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let start = x;
                let w = tab_label(i + 1, name, collapsed).chars().count();
                x += w + 1;
                (i, start, w)
            })
            .collect()
    }

    fn layout(&self, total_w: usize) -> Layout {
        // The last column stays clear, matching the list's scrollbar gutter below.
        let content_w = total_w.saturating_sub(1);
        let buttons_w = transport_layout(0, &self.status.state).last().map_or(0, |&(_, s, w)| s + w);
        let full_w = self.tab_layout(false).last().map_or(0, |&(_, start, w)| start + w);
        let collapsed = full_w + TRANSPORT_GAP + buttons_w > content_w;

        let all_tabs = self.tab_layout(collapsed);
        let tabs_end = all_tabs.last().map_or(0, |&(_, start, w)| start + w);
        let tabs = all_tabs
            .into_iter()
            .filter(|&(_, start, _)| start < content_w)
            .map(|(i, start, w)| (i, start, w.min(content_w - start)))
            .collect();

        let transport_start = tabs_end + TRANSPORT_GAP;
        let transport = transport_layout(transport_start, &self.status.state);
        let transport_end = transport.last().map_or(transport_start, |&(_, s, w)| s + w);
        let (transport, detail_start) = if transport_end <= content_w {
            (transport, transport_end + TRANSPORT_GAP)
        } else {
            (Vec::new(), transport_start)
        };
        Layout { collapsed, tabs, transport, detail: (detail_start, content_w.saturating_sub(detail_start)) }
    }

    /// The visible title and its start column, right-aligned in the room the layout leaves.
    fn title(&self, layout: &Layout) -> (usize, String) {
        let (detail_start, detail_w) = layout.detail;
        let text = scroll_title(&self.status.now_playing, detail_w, self.marquee_offset);
        (detail_start + detail_w - text.width(), text)
    }

    /// Draws the bar across row 0 of `printer`, the marquee right-aligned in whatever room is left.
    pub(super) fn draw(&self, printer: &Printer) {
        let layout = self.layout(printer.size.x);
        for &(i, start, w) in &layout.tabs {
            let text: String = tab_label(i + 1, &self.tabs[i], layout.collapsed).chars().take(w).collect();
            if i == self.active {
                let style = ColorStyle::new(Color::Dark(BaseColor::White), ACTIVE_TAB_BG);
                printer.with_color(style, |p| p.print((start, 0), &text));
            } else {
                printer.print((start, 0), &text);
            }
        }
        for ((_, label), &(_, start, _)) in transport_labels(&self.status.state).iter().zip(&layout.transport) {
            printer.print((start, 0), label);
        }
        let (start, text) = self.title(&layout);
        if !text.is_empty() {
            let played = match self.status.duration_ms {
                0 => 0,
                d => text.width() * self.status.position_ms.min(d) as usize / d as usize,
            };
            let (mut split, mut cells) = (0, 0);
            for (i, c) in text.char_indices() {
                cells += c.width().unwrap_or(0);
                if cells > played && c.width().unwrap_or(0) > 0 {
                    break;
                }
                split = i + c.len_utf8();
            }
            let (done, rest) = text.split_at(split);
            printer.with_color(ColorStyle::title_primary(), |p| {
                p.with_effect(Effect::Underline, |p| p.print((start, 0), done));
                p.print((start + done.width(), 0), rest);
            });
        }
    }

    /// What a left click at column `x` of a `total_w`-wide bar landed on.
    pub(super) fn click(&self, x: usize, total_w: usize) -> Option<TabBarHit> {
        let layout = self.layout(total_w);
        if let Some(&(button, ..)) = layout.transport.iter().find(|&&(_, s, w)| in_span(x, (s, w))) {
            return Some(TabBarHit::Transport(button));
        }
        if let Some(&(i, ..)) = layout.tabs.iter().find(|&&(_, s, w)| in_span(x, (s, w))) {
            return Some(TabBarHit::Tab(i));
        }
        let (start, text) = self.title(&layout);
        let (duration, width) = (self.status.duration_ms, text.width());
        if duration == 0 || !in_span(x, (start, width)) {
            return None;
        }
        let target_ms = ((x - start) as f64 / width as f64 * duration as f64).round() as i64;
        Some(TabBarHit::Title(Command::Seek(target_ms - self.status.position_ms as i64)))
    }
}
