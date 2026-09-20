//! Shared HTTP: whole-body GETs for small things (the updater) and `RangeReader`, the `Read + Seek`
//! over a URL that `Media::Url` playback and any source with its own HTTP needs are built on.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_RANGE, RANGE};

const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// A cancel takes effect within one request, so keep them small.
const CHUNK: u64 = 256 * 1024;

fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder().user_agent("medley").timeout(FETCH_TIMEOUT).build().expect("reqwest client with just a timeout set should never fail to build")
    })
}

/// A whole-body download has no total deadline, only a connect one.
fn body_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent("medley")
            .connect_timeout(FETCH_TIMEOUT)
            .timeout(None)
            .build()
            .expect("reqwest client with just a connect timeout set should never fail to build")
    })
}

/// GETs `url` and streams the whole body into `w`.
pub fn fetch_url_to(url: &str, w: &mut impl Write) -> io::Result<()> {
    let mut resp = client().get(url).send().and_then(|r| r.error_for_status()).map_err(io::Error::other)?;
    resp.copy_to(w).map_err(io::Error::other)?;
    Ok(())
}

/// GETs `url` and returns the whole body.
pub fn fetch_url_bytes(url: &str) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    fetch_url_to(url, &mut buf)?;
    Ok(buf)
}

#[derive(Clone, Default)]
pub struct HttpOptions {
    /// Replaces the shared client (proxies, cookies, a source's own timeouts).
    pub client: Option<Client>,
    pub headers: Vec<(String, String)>,
    /// Returns a fresh URL when the server answers 401/403 (a signed link that expired).
    pub refresh: Option<Arc<dyn Fn() -> io::Result<String> + Send + Sync>>,
}

enum Mode {
    Ranged { chunk: Vec<u8>, chunk_start: u64 },
    /// The server ignored `Range`: one body, read in order.
    Sequential { body: Box<dyn Read + Send>, at: u64 },
}

pub struct RangeReader {
    client: Client,
    url: String,
    opts: HttpOptions,
    pos: u64,
    len: Option<u64>,
    mode: Mode,
}

impl RangeReader {
    pub fn open(url: &str, opts: HttpOptions) -> io::Result<Self> {
        let client = opts.client.clone().unwrap_or_else(|| client().clone());
        let mut this = Self { client, url: url.to_string(), opts, pos: 0, len: None, mode: Mode::Ranged { chunk: Vec::new(), chunk_start: 0 } };
        let client = this.client.clone();
        let resp = this.send(&client, Some(0))?;
        if resp.status().as_u16() == 206 {
            this.len = total_from_content_range(&resp);
            let bytes = resp.bytes().map_err(io::Error::other)?.to_vec();
            this.mode = Mode::Ranged { chunk: bytes, chunk_start: 0 };
        } else {
            // The ranged client's total timeout would cut a long unranged body short.
            drop(resp);
            let resp = this.send(body_client(), None)?;
            this.len = resp.content_length();
            this.mode = Mode::Sequential { body: Box::new(resp), at: 0 };
        }
        Ok(this)
    }

    fn send(&mut self, client: &Client, range_start: Option<u64>) -> io::Result<Response> {
        for attempt in 0..2 {
            let mut req = client.get(&self.url);
            if let Some(start) = range_start {
                req = req.header(RANGE, format!("bytes={start}-{}", start + CHUNK - 1));
            }
            for (k, v) in &self.opts.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = req.send().map_err(io::Error::other)?;
            let status = resp.status().as_u16();
            if matches!(status, 401 | 403)
                && attempt == 0
                && let Some(refresh) = &self.opts.refresh
            {
                self.url = refresh()?;
                continue;
            }
            if status == 416 {
                return Ok(resp);
            }
            return resp.error_for_status().map_err(io::Error::other);
        }
        Err(io::Error::other("url refresh did not help"))
    }

    fn fill_chunk(&mut self) -> io::Result<()> {
        let pos = self.pos;
        let client = self.client.clone();
        let resp = self.send(&client, Some(pos))?;
        match resp.status().as_u16() {
            206 => {
                let bytes = resp.bytes().map_err(io::Error::other)?.to_vec();
                self.mode = Mode::Ranged { chunk: bytes, chunk_start: pos };
            }
            416 => {
                self.len = Some(pos);
                self.mode = Mode::Ranged { chunk: Vec::new(), chunk_start: pos };
            }
            s => return Err(io::Error::other(format!("range request answered {s}"))),
        }
        Ok(())
    }
}

fn total_from_content_range(resp: &Response) -> Option<u64> {
    resp.headers().get(CONTENT_RANGE)?.to_str().ok()?.rsplit('/').next()?.parse().ok()
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.len.is_some_and(|l| self.pos >= l) {
            return Ok(0);
        }
        if let Mode::Sequential { body, at } = &mut self.mode {
            if self.pos != *at {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "server does not support ranges"));
            }
            let n = body.read(buf)?;
            *at += n as u64;
            self.pos += n as u64;
            return Ok(n);
        }
        let in_chunk = |this: &Self| match &this.mode {
            Mode::Ranged { chunk, chunk_start } => this.pos >= *chunk_start && this.pos < chunk_start + chunk.len() as u64,
            Mode::Sequential { .. } => false,
        };
        if !in_chunk(self) {
            self.fill_chunk()?;
            if !in_chunk(self) {
                return Ok(0);
            }
        }
        let Mode::Ranged { chunk, chunk_start } = &self.mode else { unreachable!("checked above") };
        let from = (self.pos - chunk_start) as usize;
        let n = buf.len().min(chunk.len() - from);
        buf[..n].copy_from_slice(&chunk[from..from + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => match (&self.mode, self.len) {
                (Mode::Ranged { .. }, Some(len)) => len.checked_add_signed(d),
                _ => return Err(io::Error::new(io::ErrorKind::Unsupported, "length unknown or not seekable")),
            },
        };
        let target = target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        if let Mode::Sequential { at, .. } = &self.mode
            && target != *at
        {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "server does not support ranges"));
        }
        self.pos = target;
        Ok(target)
    }
}
