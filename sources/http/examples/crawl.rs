//! Manual smoke: crawl a directory-listing URL and print the hits.
//!
//! ```text
//! cargo run -p sources-http --example crawl -- http://localhost:8000/
//! ```

use core::{Source, SearchQuery};
use sources_http::HttpDirSource;

fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: crawl <directory-url> [query]");
        std::process::exit(2);
    });
    let query = std::env::args().nth(2).unwrap_or_default();

    let src = HttpDirSource::new(std::slice::from_ref(&url), 3);
    let mut q = SearchQuery::text(query);
    q.limit = 0; // unlimited

    let mut n = 0;
    let res = src.search(&q, &mut |hit| {
        n += 1;
        println!(
            "{:>2}. {} — {}  [{}]  {}",
            n,
            if hit.artists.is_empty() {
                "(no artist)".to_string()
            } else {
                hit.artists.join(", ")
            },
            hit.title,
            match hit.rendition().quality {
                core::Quality::Lossless { .. } => "lossless",
                core::Quality::Lossy { .. } => "lossy",
                core::Quality::Unknown => "unknown",
                core::Quality::Preview => "preview",
            },
            hit.rendition().uri,
        );
    });

    match res {
        Ok(()) => println!("\n{n} hit(s) from {url}"),
        Err(e) => {
            eprintln!("\nsearch error: {e}");
            std::process::exit(1);
        }
    }
}
