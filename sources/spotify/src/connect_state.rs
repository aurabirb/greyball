//! Minimal Spotify Connect device registration + real-time playback-state reporting, so plays
//! made through medley's own rodio player (never Spotify Connect itself) still land in Spotify's
//! "Recently Played". Deliberately not `librespot-connect`/`spirc`: no remote-command handling,
//! no context/transfer/queue logic — just dealer hello (`connection_id`) + `PutStateRequest` on
//! real playback events + deregister on stop.

use std::sync::Arc;

use librespot_core::Session;
use librespot_core::dealer::protocol::Message as DealerMessage;
use librespot_protocol::connect::{Capabilities, Device, DeviceInfo, MemberType, PutStateReason, PutStateRequest};
use librespot_protocol::devices::DeviceType;
use librespot_protocol::player::{ContextPlayerOptions, PlayOrigin, PlayerState, ProvidedTrack, Suppressions};
use protobuf::{EnumOrUnknown, MessageField};
use tokio_stream::StreamExt;

use crate::auth::MUSIC_CLIENT_ID;
use crate::link::Live;

const DEVICE_NAME: &str = "medley";
const CONNECTION_ID_TOPIC: &str = "hm://pusher/v1/connections/";
const CONNECTION_ID_HEADER: &str = "Spotify-Connection-Id";

#[derive(Clone, Copy)]
pub enum PlaybackState {
    Playing { position_ms: u32, duration_ms: u32 },
    Paused { position_ms: u32, duration_ms: u32 },
}

/// Serializes dealer-hello attempts so two playback events racing in don't both try to start the
/// dealer for the same `Live` generation.
struct Ready {
    generation: u32,
}

/// Fire-and-forget from any thread; the actual dealer/HTTP work runs on the session's own tokio
/// runtime (`Live::handle`). One instance is shared for the whole provider's lifetime — it just
/// re-hellos whenever the session's `generation` moves on from a reconnect.
#[derive(Default)]
pub struct ConnectReporter {
    ready: tokio::sync::Mutex<Option<Ready>>,
}

impl ConnectReporter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn report(self: &Arc<Self>, live: &Live, uri: &str, state: PlaybackState) {
        let this = self.clone();
        let live = live.clone();
        let uri = uri.to_string();
        let handle = live.handle.clone();
        handle.spawn(async move {
            if !this.ensure_ready(&live).await {
                return;
            }
            let request = build_request(&live.session, &uri, state);
            match live.session.spclient().put_connect_state_request(&request).await {
                Ok(_) => log::debug!("spotify connect: state PUT accepted ({uri})"),
                Err(e) => log::debug!("spotify connect: state PUT failed for {uri}: {e}"),
            }
        });
    }

    /// Track stopped / moved off Spotify: mark the device inactive so it doesn't linger as
    /// "playing" on spotify.com.
    pub fn stopped(self: &Arc<Self>, live: &Live) {
        let this = self.clone();
        let live = live.clone();
        let handle = live.handle.clone();
        handle.spawn(async move {
            if !this.ensure_ready(&live).await {
                return;
            }
            match live.session.spclient().put_connect_state_inactive(false).await {
                Ok(_) => log::debug!("spotify connect: marked inactive"),
                Err(e) => log::debug!("spotify connect: inactive PUT failed: {e}"),
            }
        });
    }

    /// Starts the dealer and fetches `connection_id` at most once per `Live` generation — the
    /// state PUT is rejected without one. A reconnect bumps `generation`, forcing a fresh hello.
    async fn ensure_ready(&self, live: &Live) -> bool {
        let mut guard = self.ready.lock().await;
        if guard.as_ref().is_some_and(|r| r.generation == live.generation) {
            return true;
        }
        *guard = None;
        if let Err(e) = live.session.dealer().start().await {
            log::warn!("spotify connect: dealer start failed: {e}");
            return false;
        }
        let mut sub = match live.session.dealer().listen_for(CONNECTION_ID_TOPIC, extract_connection_id) {
            Ok(sub) => sub,
            Err(e) => {
                log::warn!("spotify connect: subscribing for connection id failed: {e}");
                return false;
            }
        };
        let Some(Ok(connection_id)) = sub.next().await else {
            log::warn!("spotify connect: no connection id received from the dealer");
            return false;
        };
        live.session.set_connection_id(&connection_id);
        *guard = Some(Ready { generation: live.generation });
        log::debug!("spotify connect: registered device, connection id received");
        true
    }
}

fn extract_connection_id(msg: DealerMessage) -> Result<String, librespot_core::Error> {
    msg.headers
        .get(CONNECTION_ID_HEADER)
        .cloned()
        .ok_or_else(|| librespot_core::Error::failed_precondition("dealer hello had no connection id header"))
}

/// Advertises no remote-control support at all (`command_acks`/`supports_*_command` all false,
/// `hidden`+`connect_disabled` true) — this device only ever reports its own state, it never
/// takes commands from other Spotify apps, so it must not claim otherwise.
fn capabilities() -> Capabilities {
    Capabilities {
        can_be_player: true,
        is_observable: true,
        command_acks: false,
        hidden: true,
        disable_volume: true,
        connect_disabled: true,
        is_controllable: false,
        supports_logout: false,
        supports_rename: false,
        supports_playlist_v2: false,
        supports_external_episodes: false,
        supports_set_backend_metadata: false,
        supports_transfer_command: false,
        supports_command_request: false,
        supports_set_options_command: false,
        is_voice_enabled: false,
        needs_full_player_state: false,
        supports_gzip_pushes: false,
        supports_rooms: false,
        supports_dj: false,
        ..Default::default()
    }
}

fn device_info(session: &Session) -> DeviceInfo {
    DeviceInfo {
        can_play: true,
        name: DEVICE_NAME.to_string(),
        capabilities: MessageField::some(capabilities()),
        device_type: EnumOrUnknown::new(DeviceType::COMPUTER),
        device_id: session.device_id().to_string(),
        client_id: MUSIC_CLIENT_ID.to_string(),
        is_private_session: false,
        is_social_connect: false,
        ..Default::default()
    }
}

fn player_state(session: &Session, uri: &str, state: PlaybackState) -> PlayerState {
    let (position_ms, duration_ms, is_paused) = match state {
        PlaybackState::Playing { position_ms, duration_ms } => (position_ms, duration_ms, false),
        PlaybackState::Paused { position_ms, duration_ms } => (position_ms, duration_ms, true),
    };
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    PlayerState {
        session_id: session.session_id(),
        playback_speed: 1.0,
        play_origin: MessageField::some(PlayOrigin::new()),
        suppressions: MessageField::some(Suppressions::new()),
        options: MessageField::some(ContextPlayerOptions::new()),
        prev_tracks: Vec::new(),
        next_tracks: Vec::new(),
        track: MessageField::some(ProvidedTrack { uri: uri.to_string(), ..Default::default() }),
        position_as_of_timestamp: position_ms as i64,
        duration: duration_ms as i64,
        timestamp: now_ms,
        is_playing: !is_paused,
        is_paused,
        is_buffering: false,
        ..Default::default()
    }
}

fn build_request(session: &Session, uri: &str, state: PlaybackState) -> PutStateRequest {
    let device = Device {
        device_info: MessageField::some(device_info(session)),
        player_state: MessageField::some(player_state(session, uri, state)),
        ..Default::default()
    };
    PutStateRequest {
        device: MessageField::some(device),
        member_type: EnumOrUnknown::new(MemberType::CONNECT_STATE),
        is_active: true,
        put_state_reason: EnumOrUnknown::new(PutStateReason::PLAYER_STATE_CHANGED),
        client_side_timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        ..Default::default()
    }
}
