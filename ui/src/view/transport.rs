use unicode_width::UnicodeWidthStr;

use core::{Command, PlayerState};

use crate::keybindings::Action;

/// One of the top-bar's transport buttons, drawn right after the tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Transport {
    Prev,
    PlayPause,
    Next,
    Shuffle,
    Like,
}

impl Transport {
    pub(super) fn action(self) -> Action {
        Action::Command(match self {
            Transport::Prev => Command::Previous,
            Transport::PlayPause => Command::PlayPause,
            Transport::Next => Command::Next,
            Transport::Shuffle => Command::ToggleShuffle,
            Transport::Like => return Action::LikePlaying,
        })
    }
}

/// Prev/next glyphs, shared by the top-bar transport strip and the bottom status line.
pub(super) const PREV_ICON: &str = "⏮";

pub(super) const NEXT_ICON: &str = "⏭";

pub(super) const SHUFFLE_ICON: &str = "ϟ";

pub(super) const LIKED_ICON: &str = "♥";

pub(super) const UNLIKED_ICON: &str = "♡";

/// Gap on either side of the top-bar transport cluster.
pub(super) const TRANSPORT_GAP: usize = 2;

/// The transport buttons' text, space-padded like `tab_label`.
pub(super) fn transport_labels(state: &PlayerState, liked: Option<bool>) -> [(Transport, String); 5] {
    [
        (Transport::Shuffle, format!(" {SHUFFLE_ICON} ")),
        (Transport::Prev, format!(" {PREV_ICON} ")),
        (Transport::PlayPause, format!(" {} ", player_action_glyph(state))),
        (Transport::Next, format!(" {NEXT_ICON} ")),
        (Transport::Like, format!(" {} ", if liked.is_some() { LIKED_ICON } else { UNLIKED_ICON })),
    ]
}

/// Transport buttons' start column and width, packed left-to-right from `start` with no gap between them.
pub(super) fn transport_layout(start: usize, state: &PlayerState, liked: Option<bool>) -> Vec<(Transport, usize, usize)> {
    let mut x = start;
    transport_labels(state, liked)
        .into_iter()
        .map(|(button, label)| {
            let w = label.width();
            let s = x;
            x += w;
            (button, s, w)
        })
        .collect()
}

/// `▶`/`⏸`/`⏹` for the given playback state.
pub fn player_state_icon(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "▶",
        PlayerState::Paused => "⏸",
        PlayerState::Stopped => "⏹",
    }
}

/// The play/pause *button*'s icon: the action a press would take, not the state it's in.
fn player_action_icon(state: &PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "⏸",
        PlayerState::Paused | PlayerState::Stopped => "▶",
    }
}

/// `player_action_icon` with `player_state_glyph`'s leading-space padding.
pub(super) fn player_action_glyph(state: &PlayerState) -> String {
    format!(" {}", player_action_icon(state))
}

/// `player_state_icon`, with an extra leading space.
pub fn player_state_glyph(state: &PlayerState) -> String {
    format!(" {}", player_state_icon(state))
}
