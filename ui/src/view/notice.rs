use core::{CoreEvent, Dispatch, MembershipOutcome, ScanMode};

/// Where a message is shown; the one place that decides. Background failures never come here: `Session::warn` lists them.
pub(crate) enum Notice {
    /// The focused window's status row, until the next input event: what a key did, or why it is blocked right now.
    Flash(String),
    /// The focused window's own status row until its next key: a bind's result, or why it was refused.
    Status { text: String, refused: bool },
    /// A dialog the user dismisses: a plugin's report, or the failure of something the user asked for.
    Popup(String),
}

impl Notice {
    /// Something the user asked for failed.
    pub(super) fn failed(msg: impl Into<String>) -> Notice {
        Notice::Popup(msg.into())
    }

    pub(super) fn nothing_playing() -> Notice {
        Notice::Flash("nothing is playing".to_string())
    }

    pub(super) fn not_in_list() -> Notice {
        Notice::Flash("the playing track is not in this list".to_string())
    }

    pub(super) fn of_dispatch(result: core::Result<Dispatch>) -> Option<Notice> {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => return Some(Notice::failed(e.to_string())),
        };
        Some(match outcome {
            Dispatch::Ok | Dispatch::Quit | Dispatch::PlaylistCreated(_) => return None,
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
            Dispatch::LinkPending => Notice::Flash("link: pick a second row".to_string()),
            Dispatch::Done(msg) | Dispatch::Refused(msg) => Notice::Flash(msg),
        })
    }

    pub(crate) fn of_event(event: &CoreEvent) -> Option<Notice> {
        match event {
            CoreEvent::MembershipResult(MembershipOutcome::Changed(msg) | MembershipOutcome::Blocked(msg)) => {
                Some(Notice::Flash(msg.clone()))
            }
            CoreEvent::MembershipResult(MembershipOutcome::Failed(msg)) => Some(Notice::failed(msg)),
            CoreEvent::PluginReport(msg) => Some(Notice::Popup(msg.clone())),
            _ => None,
        }
    }
}
