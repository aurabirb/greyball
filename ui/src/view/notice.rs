use core::{Dispatch, ScanMode};

/// Where a message is shown; the one place that decides.
pub(super) enum Notice {
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
            Dispatch::ScanMode(mode) => Notice::Flash(format!(
                "Scan: {}",
                match mode {
                    ScanMode::Active => "active",
                    ScanMode::CacheOnly => "cache-only",
                    ScanMode::Disabled => "off",
                }
            )),
            Dispatch::Report(msg) => Notice::Popup(msg),
        })
    }
}
