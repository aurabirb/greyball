//! Web interface: playback controls and search over a loopback HTTP + WebSocket server, frontend embedded from `web/dist`.

use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use include_dir::{Dir, include_dir};
use medley_core::{Command, CoreEvent, PlayerState, Session, Track, TrackId};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::media_keys_common::{seek_delta_ms, volume_delta_percent};

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

static DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/../web/dist");

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientRequest {
    PlayPause,
    Next,
    Previous,
    Seek { position_ms: u32 },
    Volume { volume: f64 },
    Search { text: String },
    Play { track: TrackId },
    Enqueue { track: TrackId },
}

#[derive(Serialize)]
struct TrackView {
    id: TrackId,
    title: String,
    artists: Vec<String>,
    album: Option<String>,
    duration_ms: u32,
    sources: Vec<String>,
}

impl From<Track> for TrackView {
    fn from(t: Track) -> Self {
        let mut sources: Vec<String> = Vec::new();
        for r in &t.renditions {
            if !sources.iter().any(|s| s == r.source.as_str()) {
                sources.push(r.source.to_string());
            }
        }
        Self { id: t.id, title: t.title, artists: t.artists, album: t.album, duration_ms: t.duration_ms, sources }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    State { state: &'static str, position_ms: u32, duration_ms: u32, volume: f32, buffering: bool, track: Option<TrackView> },
    SearchStarted { search: u64, sources: Vec<String> },
    SearchHit { search: u64, track: TrackView },
    SearchDone { search: u64, source: String },
}

/// A serialized reply; `search` set limits it to the connection owning that search.
#[derive(Clone)]
struct Out {
    search: Option<u64>,
    json: Arc<str>,
}

impl Out {
    fn new(search: Option<u64>, reply: &Reply) -> Self {
        Self { search, json: serde_json::to_string(reply).expect("replies serialize").into() }
    }
}

#[derive(Clone)]
struct Shared {
    session: Arc<Mutex<Session>>,
    out: broadcast::Sender<Out>,
    port: u16,
}

impl Shared {
    fn state(&self) -> Out {
        Self::state_of(&self.session.lock().unwrap())
    }

    fn state_of(s: &Session) -> Out {
        let st = s.player_status();
        let state = match st.state {
            PlayerState::Playing => "playing",
            PlayerState::Paused => "paused",
            PlayerState::Stopped => "stopped",
        };
        let track = s.now_playing().map(TrackView::from);
        Out::new(None, &Reply::State { state, position_ms: st.position_ms, duration_ms: st.duration_ms, volume: st.volume, buffering: st.buffering, track })
    }

    fn publish(&self, ev: CoreEvent) {
        if self.out.receiver_count() == 0 {
            return;
        }
        let out = match ev {
            CoreEvent::SearchHit { search, track } => {
                let found = self.session.lock().unwrap().tracks_for(&[track]);
                let Some(track) = found.into_iter().next() else { return };
                Out::new(Some(search), &Reply::SearchHit { search, track: track.into() })
            }
            CoreEvent::SearchDone { search, source } => Out::new(Some(search), &Reply::SearchDone { search, source: source.to_string() }),
            CoreEvent::Player(_) | CoreEvent::QueueChanged => self.state(),
            _ => return,
        };
        let _ = self.out.send(out);
    }

    fn handle(&self, req: ClientRequest, search: &mut Option<u64>) -> Option<Out> {
        let mut s = self.session.lock().unwrap();
        let volume_changed = matches!(req, ClientRequest::Volume { .. });
        let cmd = match req {
            ClientRequest::PlayPause => Command::PlayPause,
            ClientRequest::Next => Command::Next,
            ClientRequest::Previous => Command::Previous,
            ClientRequest::Seek { position_ms } => Command::Seek(seek_delta_ms(i64::from(position_ms), s.player_status().position_ms)),
            ClientRequest::Volume { volume } => Command::Volume(volume_delta_percent(volume, s.player_status().volume)),
            ClientRequest::Search { text } => {
                *search = s.search(&text, search.take());
                return search.map(|id| {
                    let sources = s.source_ids().iter().map(|id| id.to_string()).collect();
                    Out::new(Some(id), &Reply::SearchStarted { search: id, sources })
                });
            }
            ClientRequest::Play { track } | ClientRequest::Enqueue { track } if s.tracks_for(&[track]).is_empty() => return None,
            ClientRequest::Play { track } => Command::Play(track),
            ClientRequest::Enqueue { track } => Command::Enqueue(track),
        };
        if let Err(e) = s.dispatch(cmd) {
            log::warn!("web: command failed: {e}");
        }
        // Volume emits no event; a seek's position lags, so the periodic tick reports it.
        if volume_changed {
            let _ = self.out.send(Self::state_of(&s));
        }
        None
    }
}

/// Handle to the web worker thread, fed events from `main.rs`'s loop.
#[derive(Clone)]
pub struct WebManager {
    tx: mpsc::UnboundedSender<CoreEvent>,
}

impl WebManager {
    pub fn spawn(session: Arc<Mutex<Session>>, port: u16) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("web".into())
            .spawn(move || match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(run(Shared { session, out: broadcast::channel(4096).0, port }, rx)),
                Err(e) => log::warn!("web: cannot start tokio runtime: {e}"),
            })
            .expect("spawn web worker thread");
        Self { tx }
    }

    pub fn notify(&self, events: &[CoreEvent]) {
        for ev in events {
            if matches!(ev, CoreEvent::SearchHit { .. } | CoreEvent::SearchDone { .. } | CoreEvent::Player(_) | CoreEvent::QueueChanged) {
                let _ = self.tx.send(ev.clone());
            }
        }
    }
}

async fn run(shared: Shared, mut rx: mpsc::UnboundedReceiver<CoreEvent>) {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", shared.port)).await {
        Ok(l) => l,
        Err(e) => {
            shared.session.lock().unwrap().warn("web", &format!("cannot listen on 127.0.0.1:{}: {e}", shared.port));
            return;
        }
    };
    log::info!("web: serving on http://127.0.0.1:{}", shared.port);
    let app = axum::Router::new()
        .route("/ws", get(upgrade))
        .fallback(get(asset))
        .layer(middleware::from_fn_with_state(shared.clone(), guard))
        .with_state(shared.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::warn!("web: server stopped: {e}");
        }
    });
    // Volume/seek changes made from the TUI emit no event either.
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut last = shared.state().json;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => shared.publish(ev),
                None => break,
            },
            _ = tick.tick() => {
                if shared.out.receiver_count() == 0 {
                    continue;
                }
                let now = shared.state();
                if now.json != last {
                    last = now.json.clone();
                    let _ = shared.out.send(now);
                }
            }
        }
    }
}

/// Refuses any `Host` other than loopback on our port (DNS rebinding) and any cross-origin `Origin`.
async fn guard(State(shared): State<Shared>, req: Request, next: Next) -> Response {
    let host = header_str(req.headers(), header::HOST);
    let ours = |h: &str| h == format!("127.0.0.1:{}", shared.port) || h == format!("localhost:{}", shared.port);
    let origin_ok = header_str(req.headers(), header::ORIGIN)
        .is_none_or(|o| o.strip_prefix("http://").is_some_and(|authority| Some(authority) == host && ours(authority)));
    if host.is_some_and(ours) && origin_ok {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

async fn asset(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let Some(file) = DIST.get_file(if path.is_empty() { "index.html" } else { path }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ty = match file.path().extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, ty)], file.contents()).into_response()
}

async fn upgrade(ws: WebSocketUpgrade, State(shared): State<Shared>) -> Response {
    ws.max_message_size(MAX_MESSAGE_BYTES).max_frame_size(MAX_MESSAGE_BYTES).on_upgrade(move |socket| connection(socket, shared))
}

async fn connection(mut socket: WebSocket, shared: Shared) {
    let mut rx = shared.out.subscribe();
    let mut search: Option<u64> = None;
    let mut next = Some(shared.state());
    loop {
        if let Some(out) = next.take()
            && socket.send(Message::Text(out.json.to_string().into())).await.is_err()
        {
            break;
        }
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientRequest>(&text) {
                    Ok(req) => next = shared.handle(req, &mut search),
                    Err(e) => log::debug!("web: bad request {text:?}: {e}"),
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            out = rx.recv() => match out {
                Ok(out) if out.search.is_none_or(|id| Some(id) == search) => next = Some(out),
                Ok(_) => {}
                // Dropped hits would leave the search pending forever; a reconnect resets the client.
                Err(_) => break,
            },
        }
    }
    if let Some(id) = search {
        shared.session.lock().unwrap().forget_search(id);
    }
}
