//! Fallback for reading a Spotify playlist or album the normal Web API 403s
//! on (e.g. a playlist the authenticated user doesn't own, or no stored
//! credential pair has access to). Ported from the equivalent client in
//! `spotbye/SpotiFLAC` implementation.

use std::time::{SystemTime, UNIX_EPOCH};

use core::{Quality, RemotePage, SearchHit};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha1::Sha1;

/// Base32-encoded TOTP seed the web player's `/api/token` call authenticates
/// with. Known to rotate periodically along with `TOTP_VERSION` — if this
/// endpoint starts failing, both likely need updating from a current
/// `spotify_totp.go` upstream.
const TOTP_SECRET: &str = concat!("GM3TMMJTGYZTQNZVGM4DINJZHA4TGOBYGMZT", "CMRTGEYDSMJRHE4TEOBUG4YTCMRUGQ4DQOJUGQYTAMRRGA2TCMJSHE3TCMBY");
const TOTP_VERSION: u32 = 61;

/// Matches the web player's own UA string; some endpoints appear to gate on
/// this looking like a real browser.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

/// `fetchPlaylist`'s persisted-query hash, as sent by the current web
/// player. Rotates whenever Spotify changes this GraphQL query — a failure
/// here (`PersistedQueryNotFound` in the response) means it needs updating.
const FETCH_PLAYLIST_HASH: &str = concat!("bb67e0af06e8d6f52b531f97468ee4a", "cd44cd0f82b988e15c2ea47b1148efc77");

/// `getAlbum`'s persisted-query hash — found the same way `FETCH_PLAYLIST_HASH`
/// was, by pulling the current `open.spotify.com` web-player JS bundle
/// (`web-player.*.js`) and locating the `i.l("getAlbum", "query", <hash>, null)`
/// registration; cross-checked against the community-run
/// `Jigen-Ohtsusuki/spotify-gql-registry` hash tracker, which independently
/// scrapes the same bundle every few hours. Also rotates on web-player
/// deploys — a `PersistedQueryNotFound` response means it needs updating.
const FETCH_ALBUM_HASH: &str = concat!("6a74b456cd1735c9193d9e8ec8cc5184c", "ad7ce13572210315229db3975964361");

/// One playlist page's worth of tracks/total from the `fetchPlaylist`
/// persisted query, mapped into this codebase's own `RemotePage`/`SearchHit`
/// shapes so callers don't need to know which backend served the page.
pub fn fetch_playlist_page(
    http: &reqwest::blocking::Client,
    session: &mut Option<Session>,
    playlist_id: &str,
    offset: usize,
    limit: usize,
) -> Result<RemotePage, String> {
    if session.is_none() {
        *session = Some(Session::establish(http)?);
    }
    match query_playlist(http, session.as_ref().unwrap(), playlist_id, offset, limit) {
        Ok(page) => Ok(page),
        Err(e) => {
            // The cached session's access/client token may have simply
            // expired (both are short-lived) — re-establish once and retry
            // before giving up, rather than assuming every failure means a
            // rotated secret/hash.
            log::warn!("spotify web-player: query failed ({e}), re-establishing session and retrying once");
            *session = Some(Session::establish(http)?);
            query_playlist(http, session.as_ref().unwrap(), playlist_id, offset, limit)
        }
    }
}

/// One album page's worth of tracks/total from the `getAlbum` persisted
/// query — same shape and session-reuse as `fetch_playlist_page`, for the
/// normal Web API 403ing on an album.
pub fn fetch_album_page(
    http: &reqwest::blocking::Client,
    session: &mut Option<Session>,
    album_id: &str,
    offset: usize,
    limit: usize,
) -> Result<RemotePage, String> {
    if session.is_none() {
        *session = Some(Session::establish(http)?);
    }
    match query_album(http, session.as_ref().unwrap(), album_id, offset, limit) {
        Ok(page) => Ok(page),
        Err(e) => {
            log::warn!("spotify web-player: query failed ({e}), re-establishing session and retrying once");
            *session = Some(Session::establish(http)?);
            query_album(http, session.as_ref().unwrap(), album_id, offset, limit)
        }
    }
}

/// The web player's own short-lived credentials: an anonymous access token,
/// a device `Client-Token`, and the `clientVersion` both are tied to.
/// Re-used across pages of the same playlist walk rather than re-minted per
/// request — establishing one costs three round trips.
pub struct Session {
    access_token: String,
    client_token: String,
}

impl Session {
    fn establish(http: &reqwest::blocking::Client) -> Result<Self, String> {
        let client_version = scrape_client_version(http)?;
        let (access_token, client_id, device_id) = request_access_token(http)?;
        let client_token = request_client_token(http, &client_id, &client_version, &device_id)?;
        Ok(Self { access_token, client_token })
    }
}

/// Scrapes `clientVersion` out of `open.spotify.com`'s landing page — sent
/// live rather than hardcoded because, per upstream, this is the value that
/// changes most often (every web-player deploy).
fn scrape_client_version(http: &reqwest::blocking::Client) -> Result<String, String> {
    let html = http
        .get("https://open.spotify.com/")
        .header("User-Agent", USER_AGENT)
        .send()
        .map_err(|e| format!("spotify web-player: fetching open.spotify.com failed: {e}"))?
        .text()
        .map_err(|e| e.to_string())?;
    let marker = "<script id=\"appServerConfig\" type=\"text/plain\">";
    let start = html
        .find(marker)
        .ok_or("spotify web-player: appServerConfig script not found (page layout changed?)")?
        + marker.len();
    let end = html[start..]
        .find("</script>")
        .ok_or("spotify web-player: appServerConfig script has no closing tag")?;
    let decoded = base64_decode(html[start..start + end].trim())?;
    let cfg: Value = serde_json::from_slice(&decoded).map_err(|e| e.to_string())?;
    cfg.get("clientVersion")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "spotify web-player: appServerConfig has no clientVersion".to_string())
}

/// Mints an anonymous web-player access token via the TOTP-authenticated
/// `/api/token` call, trying a few time offsets to tolerate clock skew (the
/// same three windows upstream tries). Returns `(access_token, client_id,
/// device_id)` — `device_id` comes from the `sp_t` cookie the response sets.
fn request_access_token(http: &reqwest::blocking::Client) -> Result<(String, String, String), String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
    let offsets: [i64; 3] = [0, -30, 30];
    let mut last_err = String::new();
    for offset_secs in offsets {
        let t = now.as_secs() as i64 + offset_secs;
        let code = totp_code(t as u64);
        let url = format!(
            "https://open.spotify.com/api/token?reason=init&productType=web-player&totp={code}&totpVer={TOTP_VERSION}&totpServer={code}"
        );
        let resp = match http
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/json;charset=UTF-8")
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        let device_id = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| c.split(';').next().and_then(|kv| kv.strip_prefix("sp_t=")))
            .unwrap_or_default()
            .to_string();
        let status = resp.status();
        let body: Value = match resp.json() {
            Ok(b) => b,
            Err(e) => {
                last_err = format!("HTTP {status}: {e}");
                continue;
            }
        };
        match (body.get("accessToken").and_then(Value::as_str), body.get("clientId").and_then(Value::as_str)) {
            (Some(token), Some(client_id)) => return Ok((token.to_string(), client_id.to_string(), device_id)),
            _ => {
                last_err = format!("HTTP {status}: unexpected /api/token response {body}");
            }
        }
    }
    Err(format!("spotify web-player: /api/token failed on every clock-skew window: {last_err}"))
}

/// Mints the device `Client-Token` every `pathfinder` call needs alongside
/// the access token, via a spoofed (but static, not fingerprinted-per-device)
/// `js_sdk_data` payload — matches what upstream sends.
fn request_client_token(
    http: &reqwest::blocking::Client,
    client_id: &str,
    client_version: &str,
    device_id: &str,
) -> Result<String, String> {
    let payload = serde_json::json!({
        "client_data": {
            "client_version": client_version,
            "client_id": client_id,
            "js_sdk_data": {
                "device_brand": "unknown",
                "device_model": "unknown",
                "os": "windows",
                "os_version": "NT 10.0",
                "device_id": device_id,
                "device_type": "computer",
            },
        },
    });
    let resp: Value = http
        .post("https://clienttoken.spotify.com/v1/clienttoken")
        .header("Authority", "clienttoken.spotify.com")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&payload)
        .send()
        .map_err(|e| format!("spotify web-player: clienttoken request failed: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    if resp.get("response_type").and_then(Value::as_str) != Some("RESPONSE_GRANTED_TOKEN_RESPONSE") {
        return Err(format!("spotify web-player: unexpected clienttoken response: {resp}"));
    }
    resp.get("granted_token")
        .and_then(|g| g.get("token"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "spotify web-player: clienttoken response missing granted_token.token".to_string())
}

fn query_playlist(
    http: &reqwest::blocking::Client,
    session: &Session,
    playlist_id: &str,
    offset: usize,
    limit: usize,
) -> Result<RemotePage, String> {
    let payload = serde_json::json!({
        "variables": {
            "uri": format!("spotify:playlist:{playlist_id}"),
            "offset": offset,
            "limit": limit,
            "enableWatchFeedEntrypoint": false,
        },
        "operationName": "fetchPlaylist",
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": FETCH_PLAYLIST_HASH,
            },
        },
    });
    let resp: Value = http
        .post("https://api-partner.spotify.com/pathfinder/v2/query")
        .header("Authorization", format!("Bearer {}", session.access_token))
        .header("Client-Token", &session.client_token)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&payload)
        .send()
        .map_err(|e| format!("spotify web-player: pathfinder request failed: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    if let Some(errors) = resp.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        return Err(format!("spotify web-player: pathfinder returned errors: {errors:?}"));
    }
    let content = resp
        .pointer("/data/playlistV2/content")
        .ok_or_else(|| format!("spotify web-player: unexpected pathfinder response shape: {resp}"))?;
    let total = content.get("totalCount").and_then(Value::as_u64).unwrap_or(0) as usize;
    let items = content.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let consumed = items.len();
    let hits = items.into_iter().filter_map(|item| track_from_item(&item)).collect();
    Ok(RemotePage { hits, total, consumed })
}

fn track_from_item(item: &Value) -> Option<SearchHit> {
    let data = item.pointer("/itemV2/data")?;
    let uri = data.get("uri").and_then(Value::as_str)?.to_string();
    let title = data.get("name").and_then(Value::as_str)?.to_string();
    let artists = data
        .pointer("/artists/items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|a| a.pointer("/profile/name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let album = data.pointer("/albumOfTrack/name").and_then(Value::as_str).map(str::to_string);
    let duration_ms = data
        .pointer("/trackDuration/totalMilliseconds")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    Some(SearchHit {
        source: crate::source_id(),
        uri,
        title,
        artists,
        duration_ms,
        isrc: None,
        album,
        quality: Quality::Lossy { kbps: None },
    })
}

fn query_album(
    http: &reqwest::blocking::Client,
    session: &Session,
    album_id: &str,
    offset: usize,
    limit: usize,
) -> Result<RemotePage, String> {
    let payload = serde_json::json!({
        "variables": {
            "uri": format!("spotify:album:{album_id}"),
            "locale": "",
            "offset": offset,
            "limit": limit,
        },
        "operationName": "getAlbum",
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": FETCH_ALBUM_HASH,
            },
        },
    });
    let resp: Value = http
        .post("https://api-partner.spotify.com/pathfinder/v2/query")
        .header("Authorization", format!("Bearer {}", session.access_token))
        .header("Client-Token", &session.client_token)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&payload)
        .send()
        .map_err(|e| format!("spotify web-player: pathfinder request failed: {e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    if let Some(errors) = resp.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        return Err(format!("spotify web-player: pathfinder returned errors: {errors:?}"));
    }
    let album = resp
        .pointer("/data/albumUnion")
        .ok_or_else(|| format!("spotify web-player: unexpected pathfinder response shape: {resp}"))?;
    let album_name = album.get("name").and_then(Value::as_str).map(str::to_string);
    let tracks_v2 = album
        .get("tracksV2")
        .ok_or_else(|| format!("spotify web-player: album response missing tracksV2: {resp}"))?;
    let total = tracks_v2.get("totalCount").and_then(Value::as_u64).unwrap_or(0) as usize;
    let items = tracks_v2.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let consumed = items.len();
    let hits = items
        .into_iter()
        .filter_map(|item| track_from_album_item(&item, album_name.as_deref()))
        .collect();
    Ok(RemotePage { hits, total, consumed })
}

fn track_from_album_item(item: &Value, album_name: Option<&str>) -> Option<SearchHit> {
    let data = item.get("track")?;
    let uri = data.get("uri").and_then(Value::as_str)?.to_string();
    let title = data.get("name").and_then(Value::as_str)?.to_string();
    let artists = data
        .pointer("/artists/items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|a| a.pointer("/profile/name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let duration_ms = data
        .pointer("/duration/totalMilliseconds")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    Some(SearchHit {
        source: crate::source_id(),
        uri,
        title,
        artists,
        duration_ms,
        isrc: None,
        album: album_name.map(str::to_string),
        quality: Quality::Lossy { kbps: None },
    })
}

/// Standard TOTP (HMAC-SHA1, 30s step, 6 digits) at unix time `t`, matching
/// the Go reference's use of `github.com/pquerna/otp/totp` with default
/// options.
fn totp_code(t: u64) -> String {
    let counter = (t / 30).to_be_bytes();
    let key = base32_decode(TOTP_SECRET);
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(&counter);
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let bin = ((digest[offset] as u32 & 0x7f) << 24)
        | ((digest[offset + 1] as u32) << 16)
        | ((digest[offset + 2] as u32) << 8)
        | (digest[offset + 3] as u32);
    format!("{:06}", bin % 1_000_000)
}

/// RFC 4648 base32 (no padding needed — `TOTP_SECRET` has none).
fn base32_decode(input: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits = 0u32;
    let mut bit_count = 0u32;
    let mut out = Vec::new();
    for c in input.bytes() {
        let Some(val) = ALPHABET.iter().position(|&b| b == c.to_ascii_uppercase()) else { continue };
        bits = (bits << 5) | val as u32;
        bit_count += 5;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    out
}

/// Minimal standard-alphabet base64 decode (the `appServerConfig` blob) —
/// avoids pulling in a whole crate for one one-shot decode.
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut bit_count = 0u32;
    let mut out = Vec::new();
    for c in input.bytes() {
        if c == b'=' {
            break;
        }
        let val = ALPHABET
            .iter()
            .position(|&b| b == c)
            .ok_or_else(|| format!("invalid base64 byte {c}"))?;
        bits = (bits << 6) | val as u32;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    Ok(out)
}
