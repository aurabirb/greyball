//! MPRIS (`org.mpris.MediaPlayer2`) D-Bus server — the mechanism desktop
//! environments use to route hardware media keys (and lock-screen/notification
//! widgets) to whichever app currently owns the interface. Linux/BSD-only
//! (D-Bus); simply absent everywhere else, and soft-fails (logs, doesn't
//! crash) if no session bus is reachable at runtime.
//!
//! `Command` is toggle/relative-oriented (`PlayPause`, `Seek(relative_ms)`,
//! `Volume(relative_pct)`) while MPRIS is absolute-oriented (`Play`, `Pause`,
//! `SetPosition(abs_us)`, `Volume` as an absolute 0.0..=1.0 property) — the
//! pure conversion functions below bridge that gap and are unit-tested
//! without any D-Bus connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use medley_core::{Command, PlayerState, Session};
use tokio::sync::mpsc;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, Value};
use zbus::connection;

use crate::media_keys_common::{pause_command, play_command};

// ---- pure command/value mapping (unit-tested) ----

/// `SetPosition` is absolute microseconds; `Command::Seek` is relative
/// milliseconds.
fn set_position_delta_ms(target_position_us: i64, current_position_ms: u32) -> i64 {
    target_position_us / 1000 - i64::from(current_position_ms)
}

/// The `Volume` property setter is absolute (0.0..=1.0); `Command::Volume`
/// is a relative percent delta. Rounds to the nearest percent; clamped to
/// fit `i8` (it always will in practice, `Volume` being 0.0..=1.0).
fn volume_delta_percent(target_volume: f64, current_volume: f32) -> i8 {
    let target_pct = (target_volume.clamp(0.0, 1.0) * 100.0).round();
    let current_pct = f64::from(current_volume.clamp(0.0, 1.0)) * 100.0;
    (target_pct - current_pct).clamp(f64::from(i8::MIN), f64::from(i8::MAX)) as i8
}

fn playback_status_str(state: PlayerState) -> &'static str {
    match state {
        PlayerState::Playing => "Playing",
        PlayerState::Paused => "Paused",
        PlayerState::Stopped => "Stopped",
    }
}

// ---- D-Bus interfaces ----

struct MprisRoot;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "medley"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        // OpenUri isn't implemented (out of scope, see module docs).
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }

    fn raise(&self) {}
    fn quit(&self) {}
}

struct MprisPlayer {
    session: Arc<Mutex<Session>>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        let s = self.session.lock().unwrap();
        playback_status_str(s.player_status().state).to_string()
    }

    // Repeat/shuffle aren't wired up (out of scope) — report a fixed,
    // harmless value some MPRIS clients still expect to see present.
    #[zbus(property)]
    fn loop_status(&self) -> String {
        "None".to_string()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        let s = self.session.lock().unwrap();
        let mut hm = HashMap::new();
        let Some(track) = s.now_playing() else {
            hm.insert(
                "mpris:trackid".to_string(),
                Value::ObjectPath(ObjectPath::from_static_str_unchecked(
                    "/org/mpris/MediaPlayer2/TrackList/NoTrack",
                )),
            );
            return hm;
        };
        hm.insert(
            "mpris:trackid".to_string(),
            Value::ObjectPath(
                ObjectPath::try_from(format!("/org/medley/track/{}", track.id.0.simple()))
                    .unwrap_or_else(|_| {
                        ObjectPath::from_static_str_unchecked(
                            "/org/mpris/MediaPlayer2/TrackList/NoTrack",
                        )
                    })
                    .into_owned(),
            ),
        );
        hm.insert(
            "mpris:length".to_string(),
            Value::I64(i64::from(track.duration_ms) * 1000),
        );
        hm.insert("xesam:title".to_string(), Value::Str(track.title.clone().into()));
        hm.insert(
            "xesam:artist".to_string(),
            Value::Array(track.artists.clone().into()),
        );
        if let Some(album) = &track.album {
            hm.insert("xesam:album".to_string(), Value::Str(album.clone().into()));
        }
        hm
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        f64::from(self.session.lock().unwrap().player_status().volume)
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        let mut s = self.session.lock().unwrap();
        let delta = volume_delta_percent(volume, s.player_status().volume);
        let _ = s.dispatch(Command::Volume(delta));
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        i64::from(self.session.lock().unwrap().player_status().position_ms) * 1000
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    fn next(&self) {
        let _ = self.session.lock().unwrap().dispatch(Command::Next);
    }

    fn previous(&self) {
        let _ = self.session.lock().unwrap().dispatch(Command::Previous);
    }

    fn pause(&self) {
        let mut s = self.session.lock().unwrap();
        if let Some(cmd) = pause_command(s.player_status().state) {
            let _ = s.dispatch(cmd);
        }
    }

    fn play_pause(&self) {
        let _ = self.session.lock().unwrap().dispatch(Command::PlayPause);
    }

    /// No `Command` maps cleanly onto "stop" (`ClearQueue` is destructive
    /// and would drop the user's queue) — no-op rather than approximate it.
    fn stop(&self) {
        log::debug!("MPRIS Stop(): no equivalent Command in medley, ignoring");
    }

    fn play(&self) {
        let mut s = self.session.lock().unwrap();
        if let Some(cmd) = play_command(s.player_status().state) {
            let _ = s.dispatch(cmd);
        }
    }

    fn seek(&self, offset: i64) {
        let _ = self.session.lock().unwrap().dispatch(Command::Seek(offset / 1000));
    }

    fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
        let mut s = self.session.lock().unwrap();
        let delta = set_position_delta_ms(position, s.player_status().position_ms);
        let _ = s.dispatch(Command::Seek(delta));
    }

    #[zbus(signal)]
    async fn seeked(context: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

/// Requests from the sync main loop to the async MPRIS worker: emit a
/// `PropertiesChanged` signal for the given property group. Sent, never
/// awaited — `mpsc::UnboundedSender::send` is non-blocking.
enum Signal {
    PlaybackStatus,
    Metadata,
}

/// Handle to the background MPRIS worker thread. Cloneable; every clone
/// shares the same worker.
#[derive(Clone)]
pub struct MprisManager {
    tx: mpsc::UnboundedSender<Signal>,
}

impl MprisManager {
    /// Spawns the worker thread unconditionally. If the session bus can't be
    /// reached (no D-Bus, headless box, ...) the worker logs a warning and
    /// exits; subsequent `notify_*` calls then silently no-op (the channel's
    /// other end is gone) — this must never be fatal to the app.
    pub fn spawn(session: Arc<Mutex<Session>>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("mpris".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::warn!("mpris: cannot start tokio runtime: {e}");
                        return;
                    }
                };
                rt.block_on(run(session, rx));
            })
            .expect("spawn mpris worker thread");
        Self { tx }
    }

    /// A track started/paused/stopped — reflect the new `PlaybackStatus`.
    pub fn notify_playback_status(&self) {
        let _ = self.tx.send(Signal::PlaybackStatus);
    }

    /// `now_playing` changed — reflect the new `Metadata`.
    pub fn notify_metadata(&self) {
        let _ = self.tx.send(Signal::Metadata);
    }
}

async fn run(session: Arc<Mutex<Session>>, mut rx: mpsc::UnboundedReceiver<Signal>) {
    let root = MprisRoot;
    let player = MprisPlayer { session };

    let conn = match connection::Builder::session()
        .and_then(|b| b.name(instance_bus_name()))
        .and_then(|b| b.serve_at("/org/mpris/MediaPlayer2", root))
        .and_then(|b| b.serve_at("/org/mpris/MediaPlayer2", player))
    {
        Ok(builder) => match builder.build().await {
            Ok(conn) => conn,
            Err(e) => {
                log::warn!("mpris: no D-Bus session bus available, media keys disabled: {e}");
                return;
            }
        },
        Err(e) => {
            log::warn!("mpris: failed to configure D-Bus server, media keys disabled: {e}");
            return;
        }
    };

    let object_server = conn.object_server();
    let iface_ref = match object_server
        .interface::<_, MprisPlayer>("/org/mpris/MediaPlayer2")
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log::warn!("mpris: failed to register player interface: {e}");
            return;
        }
    };
    let player_iface = iface_ref.get().await;

    while let Some(sig) = rx.recv().await {
        let ctx = iface_ref.signal_emitter();
        let result = match sig {
            Signal::PlaybackStatus => player_iface.playback_status_changed(ctx).await,
            Signal::Metadata => player_iface.metadata_changed(ctx).await,
        };
        if let Err(e) = result {
            log::warn!("mpris: failed to emit signal: {e}");
        }
    }
}

/// Per-instance bus name, per the MPRIS spec's multi-instance policy:
/// <https://specifications.freedesktop.org/mpris-spec/2.2/#Bus-Name-Policy>
fn instance_bus_name() -> String {
    format!("org.mpris.MediaPlayer2.medley.instance{}", std::process::id())
}

