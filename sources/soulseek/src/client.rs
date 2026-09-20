//! Thin REST client for `slskd` (<https://github.com/slskd/slskd>) — the
//! commonly-run headless Soulseek daemon with a documented HTTP API. Talks
//! to whatever instance is configured (default `http://localhost:5030`,
//! matching slskd's own default port and out-of-the-box `slskd`/`slskd`
//! credentials); there's no other widely-deployed daemon literally named
//! `soulseekd` to target instead.
//!
//! Endpoints used (see slskd's `Search`/`Transfers` API controllers):
//! - `POST /api/v0/session` — login, returns a bearer token (or `X-API-Key`
//!   instead, if configured, skipping login entirely).
//! - `POST /api/v0/searches` — only starts a search server-side and returns
//!   immediately (slskd's own web UI learns of completion via a SignalR
//!   push, then fetches responses separately) — so we poll
//!   `GET .../{id}?includeResponses=true` until `isComplete` or our own
//!   timeout, then use whatever responses that poll returned.
//! - `POST /api/v0/transfers/downloads/batches` — enqueue a download; polled
//!   via `GET .../batches/{id}` until the one transfer in it reaches a
//!   terminal state.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Ordinary calls (login, poll, cancel) — cheap, shouldn't ever hang long.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// `POST /searches` blocks server-side for the whole search — must outlast
/// whatever `search_timeout_secs` we send it by a comfortable margin.
const SEARCH_HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Our own floor on the timeout callers pass in, seconds — slskd's
/// `searchTimeout` field is milliseconds (confirmed against a live
/// instance: passing a small integer as-is times the search out almost
/// instantly with zero responses), converted to ms right before we send it.
const MIN_SEARCH_TIMEOUT_SECS: u32 = 5;
/// Hard cap on results a search keeps, per TODO — Soulseek searches can
/// otherwise return an unbounded flood as slow peers keep trickling in
/// responses. Passed to slskd as `responseLimit`/`fileLimit` too, so it stops
/// collecting past this point rather than just having us truncate a bigger
/// pile after the fact.
pub const MAX_RESULTS: usize = 100;
/// How often to poll a search's own state while waiting for it to complete.
const SEARCH_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Slack added on top of the search's own `searchTimeout` before we give up
/// polling and just take whatever responses the last poll returned — slskd
/// needs a moment after the timeout elapses to actually flip `isComplete`.
const SEARCH_POLL_GRACE: Duration = Duration::from_secs(5);

pub type ClientResult<T> = Result<T, String>;

/// `reqwest::Error`'s own `Display` only prints its top-level message (e.g.
/// "error sending request for url (...)") and drops the actual cause (DNS
/// failure, connection refused, timeout, ...), which lives in `source()` —
/// walk the chain so error messages are actually actionable.
fn describe(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut cause = std::error::Error::source(e);
    while let Some(c) = cause {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cause = c.source();
    }
    out
}

#[derive(Clone)]
pub struct SlskdConfig {
    /// e.g. `http://localhost:5030`, no trailing slash.
    pub base_url: String,
    pub username: String,
    pub password: String,
    /// Takes precedence over username/password when set — sent as
    /// `X-API-Key` on every call, no login round-trip needed.
    pub api_key: Option<String>,
}

pub struct SlskdClient {
    http: reqwest::blocking::Client,
    cfg: SlskdConfig,
    token: Mutex<Option<String>>,
}

impl SlskdClient {
    pub fn new(cfg: SlskdConfig) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { http, cfg, token: Mutex::new(None) }
    }

    /// Pure connectivity: can we even open an HTTP connection to the
    /// configured host:port? No auth, no interpretation of the response —
    /// used for the plugin's background probe check, which only needs to
    /// answer "is anything listening there at all".
    pub fn reachable(&self) -> bool {
        self.http
            .get(&self.cfg.base_url)
            .timeout(Duration::from_secs(3))
            .send()
            .is_ok()
    }

    fn login(&self) -> ClientResult<String> {
        if let Some(t) = self.token.lock().unwrap().clone() {
            return Ok(t);
        }
        #[derive(Serialize)]
        struct Req<'a> {
            username: &'a str,
            password: &'a str,
        }
        #[derive(Deserialize)]
        struct Resp {
            token: String,
        }
        let url = format!("{}/api/v0/session", self.cfg.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&Req { username: &self.cfg.username, password: &self.cfg.password })
            .send()
            .map_err(|e| format!("login: {}", describe(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(format!("login failed ({status}): {body}"));
        }
        let body: Resp = resp.json().map_err(|e| format!("login: bad response: {}", describe(&e)))?;
        *self.token.lock().unwrap() = Some(body.token.clone());
        Ok(body.token)
    }

    fn auth_header(&self) -> ClientResult<(&'static str, String)> {
        if let Some(k) = &self.cfg.api_key {
            return Ok(("X-API-Key", k.clone()));
        }
        Ok(("Authorization", format!("Bearer {}", self.login()?)))
    }

    /// One authenticated call, retried once with a fresh login on a 401
    /// (stale cached token) — never retried when auth is a fixed API key,
    /// since that can't go stale the way a JWT does.
    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> ClientResult<(reqwest::StatusCode, String)> {
        let url = format!("{}{path}", self.cfg.base_url);
        let attempt = |c: &Self| -> ClientResult<(reqwest::StatusCode, String)> {
            let (h, v) = c.auth_header()?;
            let mut req = c.http.request(method.clone(), &url).timeout(timeout).header(h, v);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().map_err(|e| format!("{path}: {}", describe(&e)))?;
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            Ok((status, text))
        };
        let (status, text) = attempt(self)?;
        if status == reqwest::StatusCode::UNAUTHORIZED && self.cfg.api_key.is_none() {
            *self.token.lock().unwrap() = None;
            return attempt(self);
        }
        Ok((status, text))
    }

    /// Run a search and return its file responses, already capped at
    /// [`MAX_RESULTS`] total files across all peers.
    pub fn search(&self, text: &str, search_timeout_secs: u32) -> ClientResult<Vec<SearchResponse>> {
        let timeout_secs = search_timeout_secs.max(MIN_SEARCH_TIMEOUT_SECS);
        let id = uuid::Uuid::new_v4();
        let body = serde_json::json!({
            "id": id,
            "searchText": text,
            "responseLimit": MAX_RESULTS,
            "fileLimit": MAX_RESULTS,
            "searchTimeout": timeout_secs * 1000,
        });
        let (status, text_body) =
            self.call(reqwest::Method::POST, "/api/v0/searches", Some(&body), SEARCH_HTTP_TIMEOUT)?;
        if !status.is_success() {
            return Err(format!("search failed ({status}): {text_body}"));
        }

        // The POST only starts the search server-side and returns
        // immediately — slskd's own web UI finds out the search is done via
        // a SignalR push, then fetches responses separately. We have no
        // push channel, so poll the search's own state until it reports
        // `isComplete`, bounded by the timeout we asked it to run for.
        let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64) + SEARCH_POLL_GRACE;
        let responses = loop {
            let (status, text_body) = self.call(
                reqwest::Method::GET,
                &format!("/api/v0/searches/{id}?includeResponses=true"),
                None,
                DEFAULT_TIMEOUT,
            )?;
            if !status.is_success() {
                break Err(format!("search poll failed ({status}): {text_body}"));
            }
            let search: SearchResult =
                match serde_json::from_str(&text_body) {
                    Ok(s) => s,
                    Err(e) => break Err(format!("search: bad response: {e}")),
                };
            if search.is_complete || Instant::now() >= deadline {
                break Ok(search.responses);
            }
            std::thread::sleep(SEARCH_POLL_INTERVAL);
        };
        let _ = self.call(reqwest::Method::DELETE, &format!("/api/v0/searches/{id}"), None, DEFAULT_TIMEOUT);
        responses
    }

    /// Enqueue a single-file download into its own `medley/<batch id>`
    /// subdirectory (so the completed file is the sole entry there — no
    /// need to replicate slskd's own filename-sanitizing rules to guess its
    /// final name). Returns the batch id to poll.
    pub fn enqueue_download(&self, username: &str, filename: &str, size: u64) -> ClientResult<uuid::Uuid> {
        match self.try_enqueue(username, filename, size) {
            Err(e) if e.to_ascii_lowercase().contains("already in progress") => {
                // slskd skips a file with a live (non-completed) transfer for the same user; a stale one is ours to drop.
                self.cancel_active(username, filename)?;
                self.try_enqueue(username, filename, size)
            }
            r => r,
        }
    }

    fn try_enqueue(&self, username: &str, filename: &str, size: u64) -> ClientResult<uuid::Uuid> {
        let batch_id = uuid::Uuid::new_v4();
        let body = serde_json::json!({
            "id": batch_id,
            "username": username,
            "files": [{ "filename": filename, "size": size }],
            "options": { "destination": format!("medley/{batch_id}") },
        });
        let (status, text) =
            self.call(reqwest::Method::POST, "/api/v0/transfers/downloads/batches", Some(&body), DEFAULT_TIMEOUT)?;
        // 200/201/207 all mean "the batch was created" — only a non-2xx-family
        // failure to even create it is fatal here; per-file failures show up
        // once we poll (`transfers` will be empty and `failures` populated).
        if !status.is_success() {
            return Err(format!("enqueue download failed ({status}): {text}"));
        }
        let resp: BatchResponse = serde_json::from_str(&text).map_err(|e| format!("enqueue download: bad response: {e}"))?;
        if let Some(failure) = resp.failures.first() {
            return Err(format!("slskd rejected the download: {}", failure.message));
        }
        Ok(batch_id)
    }

    /// The one transfer in a batch enqueued by [`Self::enqueue_download`],
    /// if slskd has recorded it yet.
    pub fn batch_transfer(&self, batch_id: uuid::Uuid) -> ClientResult<Option<Transfer>> {
        let (status, text) =
            self.call(reqwest::Method::GET, &format!("/api/v0/transfers/downloads/batches/{batch_id}"), None, DEFAULT_TIMEOUT)?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("batch status failed ({status}): {text}"));
        }
        let batch: BatchBody = serde_json::from_str(&text).map_err(|e| format!("batch status: bad response: {e}"))?;
        Ok(batch.transfers.into_iter().next())
    }

    fn cancel_active(&self, username: &str, filename: &str) -> ClientResult<()> {
        let (status, text) =
            self.call(reqwest::Method::GET, &format!("/api/v0/transfers/downloads/{username}"), None, DEFAULT_TIMEOUT)?;
        if !status.is_success() {
            return Err(format!("listing downloads failed ({status}): {text}"));
        }
        let user: UserTransfers = serde_json::from_str(&text).map_err(|e| format!("listing downloads: bad response: {e}"))?;
        for t in user.directories.into_iter().flat_map(|d| d.files) {
            if t.filename == filename && !t.state.contains("Completed") {
                self.cancel_download(username, t.id);
            }
        }
        Ok(())
    }

    /// Best-effort cleanup of a download we gave up on (timed out waiting).
    pub fn cancel_download(&self, username: &str, id: uuid::Uuid) {
        let path = format!("/api/v0/transfers/downloads/{username}/{id}?remove=true");
        let _ = self.call(reqwest::Method::DELETE, &path, None, DEFAULT_TIMEOUT);
    }
}

// ---- API JSON (only the fields we use; slskd serializes camelCase) ----

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    #[serde(default)]
    is_complete: bool,
    #[serde(default)]
    responses: Vec<SearchResponse>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub username: String,
    #[serde(default)]
    pub files: Vec<SearchFile>,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub upload_speed: u64,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchFile {
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    pub extension: String,
    #[serde(default)]
    pub bit_rate: Option<u32>,
    #[serde(default)]
    pub bit_depth: Option<u32>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Track length in seconds.
    #[serde(default)]
    pub length: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchResponse {
    #[serde(default)]
    failures: Vec<BatchFailure>,
}

#[derive(Deserialize)]
struct UserTransfers {
    #[serde(default)]
    directories: Vec<UserDirectory>,
}

#[derive(Deserialize)]
struct UserDirectory {
    #[serde(default)]
    files: Vec<ListedTransfer>,
}

#[derive(Deserialize)]
struct ListedTransfer {
    id: uuid::Uuid,
    #[serde(default)]
    filename: String,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchBody {
    #[serde(default)]
    transfers: Vec<Transfer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchFailure {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: uuid::Uuid,
    /// Flags-enum, serialized as a comma-joined name list (e.g. "Completed,
    /// Succeeded") — checked with `contains`, never parsed into an enum.
    pub state: String,
    #[serde(default)]
    pub exception: Option<String>,
}

impl Transfer {
    pub fn succeeded(&self) -> bool {
        self.state.contains("Succeeded")
    }

    pub fn failed(&self) -> bool {
        ["Cancelled", "TimedOut", "Errored", "Rejected", "Aborted"]
            .iter()
            .any(|s| self.state.contains(s))
    }
}
