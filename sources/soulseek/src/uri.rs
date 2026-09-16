//! `soulseek:<user>:<size>:<filename>` rendition URIs.
//!
//! Soulseek has no canonical per-file URL to paste/resolve like SoundCloud's
//! permalinks — a "track" only exists as (peer username, remote filename,
//! size) from a live search response. Self-describing URIs (rather than a
//! plugin-local id -> details table) mean a track saved to a playlist/queue
//! still resolves after a restart, with no session state to lose.

const ESCAPED: [(char, &str); 2] = [('%', "%25"), (':', "%3A")];

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    'next: for c in s.chars() {
        for (raw, esc) in ESCAPED {
            if c == raw {
                out.push_str(esc);
                continue 'next;
            }
        }
        out.push(c);
    }
    out
}

fn decode(s: &str) -> String {
    s.replace("%3A", ":").replace("%25", "%")
}

pub struct TrackRef {
    pub username: String,
    pub filename: String,
    pub size: u64,
}

impl TrackRef {
    pub fn to_uri(&self) -> String {
        format!("soulseek:{}:{}:{}", encode(&self.username), self.size, encode(&self.filename))
    }

    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("soulseek:")?;
        let mut parts = rest.splitn(3, ':');
        let username = decode(parts.next()?);
        let size: u64 = parts.next()?.parse().ok()?;
        let filename = decode(parts.next()?);
        if username.is_empty() || filename.is_empty() {
            return None;
        }
        Some(Self { username, filename, size })
    }
}
