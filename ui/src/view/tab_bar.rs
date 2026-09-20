use cursive::Printer;
use cursive::theme::{ColorStyle, Effect};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use core::{Command, TrackId};

use std::sync::Arc;

use super::memo::Memo;
use super::status_line::StatusLine;
use super::text::{active_style, in_span, scroll_title};
use super::transport::{TRANSPORT_GAP, Transport, transport_labels, transport_layout};

/// The resampled waveform of the last (track, width, envelope length).
pub(super) type WaveformMemo = Memo<(Option<TrackId>, usize, usize), Arc<[u8]>>;

/// Narrower than this the waveform is not drawn.
const WAVE_MIN: usize = 40;

/// Bars narrower than this draw no waveform.
const WAVE_MIN_BAR: usize = 80;

/// The least the title keeps when a waveform shares its room.
const TITLE_MIN: usize = 16;

/// Bars from one to eight eighths tall.
pub(super) const GLYPHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// `envelope` reduced to `width` columns by their max, each as 0..=8 eighths.
pub(super) fn resample(envelope: &[u8], width: usize) -> Arc<[u8]> {
    (0..width)
        .map(|x| {
            let lo = x * envelope.len() / width;
            let hi = ((x + 1) * envelope.len() / width).max(lo + 1).min(envelope.len());
            let peak = envelope[lo..hi].iter().copied().max().unwrap_or(0);
            (peak as usize * 8).div_ceil(255).min(8) as u8
        })
        .collect()
}

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
    /// A seek along the waveform, or along the title when there is none.
    Seek(Command),
}

/// The top row: a tab per tabbed window, the transport buttons and the now-playing marquee, with the track's waveform between them; the waveform is the scrubber when shown, else the marquee.
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
    /// `(start, width)` of the title, right-aligned in the room right of the buttons.
    title: (usize, usize),
    /// `(start, width)` of the waveform, when there is an envelope and room for it.
    wave: Option<(usize, usize)>,
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
        let collapsed = full_w + TRANSPORT_GAP + buttons_w + WAVE_MIN + TRANSPORT_GAP + TITLE_MIN > content_w;

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
        let detail_w = content_w.saturating_sub(detail_start);
        let want = self.status.now_playing.width();
        let has_wave = total_w >= WAVE_MIN_BAR && !self.status.waveform.is_empty() && detail_w >= WAVE_MIN + TRANSPORT_GAP + TITLE_MIN;
        let title_w = if has_wave { want.min((detail_w / 2).max(TITLE_MIN)).min(detail_w - TRANSPORT_GAP - WAVE_MIN) } else { want.min(detail_w) };
        let wave = has_wave.then(|| (detail_start, detail_w - title_w - if title_w > 0 { TRANSPORT_GAP } else { 0 }));
        let title = (detail_start + detail_w - title_w, title_w);
        Layout { collapsed, tabs, transport, title, wave }
    }

    /// The visible title and its start column, clipped to the layout's title width.
    fn title(&self, layout: &Layout) -> (usize, String) {
        let (start, w) = layout.title;
        let mut text = scroll_title(&self.status.now_playing, w, self.marquee_offset);
        while text.width() > w {
            text.pop();
        }
        (start + w - text.width(), text)
    }

    /// Draws the bar across `printer`, the marquee right-aligned in whatever room is left.
    pub(super) fn draw(&self, printer: &Printer, levels_memo: &WaveformMemo) {
        let layout = self.layout(printer.size.x);
        for &(i, start, w) in &layout.tabs {
            let text: String = tab_label(i + 1, &self.tabs[i], layout.collapsed).chars().take(w).collect();
            if i == self.active {
                let style = active_style();
                printer.with_color(style, |p| p.print((start, 0), &text));
            } else {
                printer.print((start, 0), &text);
            }
        }
        for ((_, label), &(_, start, _)) in transport_labels(&self.status.state).iter().zip(&layout.transport) {
            printer.print((start, 0), label);
        }
        let (start, text) = self.title(&layout);
        if let Some((wave_start, wave_w)) = layout.wave {
            let envelope = &self.status.waveform;
            let levels = levels_memo
                .get_or_build((self.status.now_playing_id, wave_w, envelope.len()), || resample(envelope, wave_w));
            self.draw_waveform(printer, wave_start, &levels);
        }
        if !text.is_empty() {
            let played = match self.status.duration_ms {
                d if d == 0 || layout.wave.is_some() => 0,
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

    fn draw_waveform(&self, printer: &Printer, start: usize, levels: &[u8]) {
        let played = match self.status.duration_ms {
            0 => 0,
            d => levels.len() * self.status.position_ms.min(d) as usize / d as usize,
        };
        for (x, &level) in levels.iter().enumerate() {
            if level > 0 {
                let glyph = GLYPHS[level as usize - 1];
                if x < played {
                    printer.with_color(ColorStyle::title_primary(), |p| {
                        p.with_effect(Effect::Underline, |p| p.print((start + x, 0), glyph));
                    });
                } else {
                    printer.print((start + x, 0), glyph);
                }
            }
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
        let (start, width) = match layout.wave {
            Some(wave) => wave,
            None => {
                let (start, text) = self.title(&layout);
                (start, text.width())
            }
        };
        let duration = self.status.duration_ms;
        if duration == 0 || !in_span(x, (start, width)) {
            return None;
        }
        let target_ms = ((x - start) as f64 / width as f64 * duration as f64).round() as i64;
        Some(TabBarHit::Seek(Command::Seek(target_ms - self.status.position_ms as i64)))
    }
}
