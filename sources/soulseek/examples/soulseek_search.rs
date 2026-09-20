//! Ad-hoc smoke test against a real local `slskd`:
//! `cargo run -p sources-soulseek --example search -- "some track"`
//! Needs network + a reachable slskd (env: SLSKD_URL, SLSKD_USER, SLSKD_PASS,
//! defaults matching slskd's own out-of-the-box defaults). Not part of the
//! test suite.

use core::Source;
use sources_soulseek::{SlskdClient, SlskdConfig, SoulseekSource};

fn main() {
    let query = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let query = if query.is_empty() { "test".to_string() } else { query };

    let conn = SlskdConfig {
        base_url: std::env::var("SLSKD_URL").unwrap_or_else(|_| "http://localhost:5030".to_string()),
        username: std::env::var("SLSKD_USER").unwrap_or_else(|_| "slskd".to_string()),
        password: std::env::var("SLSKD_PASS").unwrap_or_else(|_| "slskd".to_string()),
        api_key: std::env::var("SLSKD_API_KEY").ok(),
    };
    let client = SlskdClient::new(conn);
    println!("reachable: {}", client.reachable());

    let src = SoulseekSource::new(client, None);
    let q = core::SearchQuery { text: query.clone(), kinds: vec![], limit: 20 };
    let mut hits = Vec::new();
    if let Err(e) = src.search(&q, &mut |h| hits.push(h)) {
        eprintln!("search failed: {e}");
        std::process::exit(1);
    }
    println!("{} hit(s) for {query:?}:", hits.len());
    for h in &hits {
        println!("  {} — {}  [{}]  {}ms  {:?}", h.artists.join(", "), h.title, h.rendition().uri, h.duration_ms, h.rendition().quality);
    }
}
