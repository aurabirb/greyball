## Working agreement

- One `general-purpose` agent per task, run sequentially (never in
  parallel, never forked) — each gets a full self-contained brief since it
  starts with no memory of this conversation.
- Each task: read AGENTS.md, then read the relevant code fresh, implement, then `cargo build
  --workspace --all-features`, `cargo clippy --workspace --all-features --all-targets` — both
  clean — before committing (never amending) and `git push origin main`. No tests, ever — see
  AGENTS.md.
- Never stash or undo changes not made by you; multiple agents are running in parallel.
- Genuine user-visible ambiguities get reported back rather than guessed.

## TODOs:

### Bugs
- [ ] Spotify playback occasionally dies mid-song ("session invalid (dead access-point connection)")
  and reconnects, producing an audible ~1-2s gap while a whole new `Session`/mixer/player is rebuilt
  from scratch (`sources/spotify/src/player.rs`'s reconnect path is a full cold teardown-and-rebuild,
  not a resume-in-place — there's no ring/jitter buffer decoupling decode from the output sink, so
  nothing survives the teardown to keep audio flowing during the gap). Real repro log (macOS, session
  had been idle-ish at ~1 track for a couple minutes, not paused):
  ```
  14:46:13 ERROR Connection to server closed.
  14:46:13 WARN spotify: session invalid (dead access-point connection), reconnecting
  14:46:14 INFO Connecting to AP "ap-gew4.spotify.com:4070"
  14:46:14 DEBUG Connection to "ap-gew4.spotify.com:4070" failed: Connection refused (os error 61)
  14:46:14 DEBUG Retry access point...
  14:46:14 DEBUG Connection to AP established.
  14:46:14 INFO spotify: reconnected, resuming spotify:track:6pnma5HuIzznrKGATrRZiu (paused=false)
  14:46:15 DEBUG media_keys_macos: update_now_playing state=Playing ...
  ```
  Root cause is upstream librespot: it resolves a single AP/CDN endpoint with no fallback if that one
  goes bad mid-session — matches open librespot issues
  [#1151](https://github.com/librespot-org/librespot/issues/1151) (random mid-playback drops with
  connection resets, unresolved) and
  [#1486](https://github.com/librespot-org/librespot/issues/1486) (AP stops working after ~1-2h). We're
  pinned to `librespot-*` 0.8.0 (current stable — not behind on releases), but librespot's `dev` branch
  changelog already has two unreleased fixes for exactly this: "try all resolved addresses for the
  dealer connection instead of failing after the first" and "try the next CDN URL on a non-206 fetch
  instead of only retrying transport errors".
  1. Reduce how often it dies: pin `sources/spotify`'s `librespot-*` deps to a `dev` git rev to pick up
     those two fixes now (moderate cost — git-rev pinning is more fragile than crates.io and needs
     periodic re-vetting, but best effort/impact ratio available; no newer numbered release exists to
     just bump to). Cheaper/smaller: skip re-resolving track metadata in the reconnect path when
     duration is unchanged, to shave a network round-trip off the resume gap.
     Done: `sources/spotify`'s `librespot-*` deps are now pinned to `dev` git rev
     `939dc5ee9d833e1980f9495241219d9d4868a061`, which includes both fixes — confirmed present by
     inspecting `~/.cargo/git/checkouts/librespot-*/939dc5e`'s own history at that exact rev:
     `34f9cd2` ("fix: try all resolved socket addrs for connection") rewrites
     `core/src/socket.rs::connect` to call `TcpStream::connect((host, port))` (tokio tries every
     resolved address) instead of `.to_socket_addrs()?.next()` (only the first); `db1ef7a` ("audio:
     fall back to the next CDN URL when a fetch returns a non-206 status") moves the
     `StatusCode::PARTIAL_CONTENT` check inside `AudioFileStreaming::open()`'s per-URL loop in
     `audio/src/fetch/mod.rs` so a non-206 response (e.g. a CDN edge returning 500) falls through to
     the next resolved CDN URL instead of the loop breaking on the first response regardless of
     status. Workspace builds clean against the pin (`cargo build --workspace --all-features`), and
     live-tested normal Spotify playback in tmux against a real account: session connects/
     authenticates, a Spotify-only search hit loads and plays, position advances, and `:vis` shows a
     real, growing audio-level bar (not just wall-clock position ticking) — so the pin doesn't
     regress ordinary playback. Still not confirmed whether it actually reduces the reconnect-gap
     bug itself — that requires the AP connection to genuinely go bad mid-session (the
     `session.is_invalid()` / "dead access-point connection" path), which is intermittent and
     network-dependent and wasn't reproducible on demand in this session.
  2. Making drops actually inaudible (gapless reconnect) is a separate, much bigger feature, not a
     tweak: it needs a real ring/jitter buffer between decode and the output sink (decoupled from
     `Session` lifetime — today `TappedSink` forwards every decoded packet straight through with zero
     buffering beyond a small non-replayable visualizer window), and likely a pre-warmed standby
     session kept authenticated in parallel so the swap-over doesn't pay full AP-handshake +
     track-resolve latency in the critical path (untested whether Spotify's session semantics even
     allow an idle second authenticated session per account). Scope this as its own project if pursued,
     not a quick fix bundled with option 1.
- [ ] Media keys still don't work on macOS, and the OS "Now Playing" status/widget never gets updated. Investigate whether this needs some form of app registration/packaging (e.g. macOS media-remote/`MPNowPlayingInfoCenter`/`MPRemoteCommandCenter` integration typically requires a proper `.app` bundle with an `Info.plist`/bundle identifier, not a bare CLI binary) — figure out and document the actual OS requirements needed to make this work, then implement whatever's missing.
### Features
- [ ] Make soundcloud provide explore page playlist in the playlists view
- [ ] On soulseek setup page, it should ask the user if they want to set up slskd with docker if it is unavailable, and if the user types yes there should be a docker command with directory and everything set up so that medley can find it, the default folder should be ~/Documents/slskd. if the user skips or types something else we just ask the host, username and password for the slskd instance. the detected slskd status should show up in settings
- [ ] Ability to include spotify playlists in search results, maybe on the playlists tab initially
- [ ] Create playlist files (m3u8) when the playlist cache updates automatically, this basically creates playlist sync feature for the user. It should be in a Documents directory so the user doesnt have to adjust it (but it should be possible in settings).
- [ ] Add a YouTube source/plugin (alongside the existing Spotify/SoundCloud/HTTP/local sources), wired into Search like the others.
- [ ] Add a single-character spinner somewhere visible to indicate when a network request is in
  progress, for cases like opening a playlist that first tries a request, gets a 403, then tries a
  bunch of fallbacks before succeeding or failing — right now there's no visual indication anything
  is happening during that stretch.

### Audits / cleanup tasks
- [ ] Review how plugin/source failures are surfaced to the user and make the channel match the
  failure's nature, instead of whatever each call site currently happens to do:
  - A failure directly caused by user input (e.g. liking a track fails) should show an error modal.
  - A failure that just means the action is blocked/not applicable right now (e.g. today's "no
    liked-songs source for this track" case) should surface in the command status bar, same as the
    current like/unlike feedback.
  - A failure in background work (e.g. a scan/plugin probe failing on its own, not in response to a
    keypress) should be non-actionable and added to the warnings list instead, clearing on restart —
    not popped as a modal or shoved into the status bar.
  Audit existing call sites (`set_liked`/`Command::Like`/`Unlike` in `core/src/app.rs`, plugin
  `probe()`/`setup()` failures, the new plugin-command seam's `run_command` error path, scan/BPM plugin
  errors) against these three categories and fix whichever ones use the wrong channel.
- [ ] Check whether pausing the background scan with `B` (`ToggleScan`/`scan.set_paused`) actually
  inhibits *future and queued* track analysis/download, or only pauses whatever's in flight right now
  — i.e. does newly-added/queued work still get analyzed/downloaded while paused, or does it correctly
  stay queued until resumed?
- [ ] Find functionality that exists in the codebase but isn't currently bound to a key or command, and wire it up so it's reachable.
- [ ] Run an agent to collect and remove any placeholders of any kind. Write it in the memory to never write placeholders of any kind. Check the 
  todo for infra that is stubbed for unimplemented parts and remove it. Remove any reference for
  future features by moving them on the main todo list. never keep done items on the todo list.

- [ ] Run an agent to reduce code duplication and DRY violations, along with any
  violations of the user policies.
