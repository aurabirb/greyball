//! Ad-hoc smoke test: `cargo run -p sources-soundcloud --example search -- "daft punk"`
//! Scrapes a client_id, runs a track search, prints the hits, then resolves the
//! first one's stream URL. Needs network. Not part of the test suite.

use core::{Bus, Media, MediaProvider, SearchQuery, Source};
use sources_soundcloud::SoundcloudSource;

fn main() {
    let query = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let query = if query.is_empty() { "daft punk".to_string() } else { query };

    let src = SoundcloudSource::new(
        std::env::var("SC_CLIENT_ID").ok(),
        std::env::var("SC_OAUTH_TOKEN").ok(),
        Bus::new(),
        true,
    );
    let q = SearchQuery { text: query.clone(), kinds: vec![], limit: 5 };

    let mut hits = Vec::new();
    if let Err(e) = src.search(&q, &mut |h| hits.push(h)) {
        eprintln!("search failed: {e}");
        std::process::exit(1);
    }
    println!("{} hit(s) for {query:?}:", hits.len());
    for h in &hits {
        println!("  {} — {}  [{}]  {}ms", h.artists.join(", "), h.title, h.rendition().uri, h.duration_ms);
    }

    if let Some(first) = hits.first() {
        match src.open(first.rendition(), &|| true) {
            Ok(Media::Url(u)) => println!("\nstream for {}: {u}", first.rendition().uri),
            Ok(_) => println!("\nstream: unexpected non-URL Media variant"),
            Err(e) => eprintln!("\nopen failed: {e}"),
        }
    }
}
