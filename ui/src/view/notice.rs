use core::{CoreEvent, Dispatch, ScanMode};

/// Where a message is shown; the one place that decides.
pub(crate) enum Notice {
    /// The hint row, until the next input event.
    Flash(String),
    /// A dialog the user dismisses.
    Popup(String),
}

impl Notice {
    pub(super) fn of_dispatch(outcome: Dispatch) -> Option<Notice> {
        Some(match outcome {
            Dispatch::Ok | Dispatch::Quit => return None,
            Dispatch::Queued(n) => Notice::Flash(format!("Queued: {n} tracks")),
            Dispatch::Wedged(n) => Notice::Flash(format!("Wedged: {n} tracks")),
            Dispatch::ShuffleSet(on) => Notice::Flash(format!("Shuffle: {}", if on { "on" } else { "off" })),
            Dispatch::MembershipSet { track, playlist, added: true } => Notice::Flash(format!("Added {track:?} to {playlist:?}")),
            Dispatch::MembershipSet { track, playlist, added: false } => {
                Notice::Flash(format!("Removed {track:?} from {playlist:?}"))
            }
            Dispatch::ScanMode(mode) => Notice::Flash(format!(
                "Scan: {}",
                match mode {
                    ScanMode::Active => "active",
                    ScanMode::CacheOnly => "cache-only",
                    ScanMode::Disabled => "off",
                }
            )),
            Dispatch::Report(msg) => Notice::Popup(msg),
            Dispatch::Refused(msg) => Notice::Flash(msg),
        })
    }

    pub(crate) fn of_event(event: &CoreEvent) -> Option<Notice> {
        match event {
            CoreEvent::MembershipResult(msg) => Some(Notice::Flash(msg.clone())),
            CoreEvent::PluginCommandResult(msg) => Some(Notice::Popup(msg.clone())),
            _ => None,
        }
    }
}
