use std::time::{Duration, Instant};

use crate::queue::Queue;
use crate::traits::BrowseNode;
use crate::types::SourceId;
use crate::view_cache::{RemoteCtx, ViewCache};

/// How long a remote collection may take to load before its pending enqueue gives up.
pub(crate) const ENQUEUE_TIMEOUT: Duration = Duration::from_secs(60);

/// The most tracks one collection appends to the queue.
pub(crate) const ENQUEUE_CAP: usize = 500;

/// A remote collection being appended to the queue as its pages land.
pub(crate) struct PendingEnqueue {
    pub(crate) source: SourceId,
    pub(crate) node: BrowseNode,
    pub(crate) name: String,
    pub(crate) appended: usize,
    pub(crate) deadline: Instant,
}

pub(crate) fn queued_note(name: &str, n: usize, capped: bool) -> String {
    let cut = if capped { format!(", first {ENQUEUE_CAP} only") } else { String::new() };
    format!("queued {name} ({n} tracks{cut})")
}

impl PendingEnqueue {
    /// Appends what the list has loaded since; the message once the job has ended.
    pub(crate) fn advance(&mut self, view: &ViewCache, ctx: RemoteCtx, queue: &Queue, now: Instant) -> Option<String> {
        // Settled state first: a snapshot taken after it is complete.
        let errored = view.remote_playlist_errored(&self.source, &self.node);
        let loading = view.remote_playlist_loading(&self.source, &self.node);
        let ids = view.remote_playlist_confirmed_ids(&self.source, &self.node, ctx);
        let take = ids.len().min(ENQUEUE_CAP);
        if take > self.appended {
            queue.append_many(&ids[self.appended..take]);
            self.appended = take;
        }
        let (queued, name) = (self.appended, &self.name);
        if take == ENQUEUE_CAP && ids.len() > ENQUEUE_CAP {
            Some(queued_note(name, queued, true))
        } else if errored {
            Some(format!("could not load {name} ({queued} tracks queued)"))
        } else if !loading {
            Some(if queued == 0 { format!("nothing to queue from {name}") } else { queued_note(name, queued, false) })
        } else if now >= self.deadline {
            Some(format!("gave up loading {name} ({queued} tracks queued)"))
        } else {
            None
        }
    }
}
