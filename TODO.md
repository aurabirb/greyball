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

- [ ] Very rarely the player can play two tracks simultaneously — this should never happen. Playback commands/state should be routed through a single state machine tracking play state, so that a new play request always stops the previous track before starting the next.
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
     `939dc5ee9d833e1980f9495241219d9d4868a061`, which includes both fixes. Not yet confirmed
     whether this actually fixes the disconnect — it's an intermittent, network-dependent bug
     that needs real-world runtime to verify.
  2. Making drops actually inaudible (gapless reconnect) is a separate, much bigger feature, not a
     tweak: it needs a real ring/jitter buffer between decode and the output sink (decoupled from
     `Session` lifetime — today `TappedSink` forwards every decoded packet straight through with zero
     buffering beyond a small non-replayable visualizer window), and likely a pre-warmed standby
     session kept authenticated in parallel so the swap-over doesn't pay full AP-handshake +
     track-resolve latency in the critical path (untested whether Spotify's session semantics even
     allow an idle second authenticated session per account). Scope this as its own project if pursued,
     not a quick fix bundled with option 1.
- [ ] Media keys still don't work on macOS, and the OS "Now Playing" status/widget never gets updated. Investigate whether this needs some form of app registration/packaging (e.g. macOS media-remote/`MPNowPlayingInfoCenter`/`MPRemoteCommandCenter` integration typically requires a proper `.app` bundle with an `Info.plist`/bundle identifier, not a bare CLI binary) — figure out and document the actual OS requirements needed to make this work, then implement whatever's missing.
- [ ] On startup, Spotify sometimes shows a warning ("spotify: refreshing session — should clear on its own") even though the warnings menu shows Spotify's item already ticked/healthy — clicking the already-ticked item anyway clears the warning and playback starts working. The warning isn't actually clearing itself despite the log message claiming it should. Sample log:
  ```
  19:06:41 INFO spotify: refreshing session — should clear on its own
  19:06:41 INFO sources registered: http, soulseek, soundcloud
  19:06:41 DEBUG starting new connection: https://accounts.spotify.com/
  19:07:56 INFO spotify: using cached web-api token
  19:07:56 DEBUG new Session
  19:07:56 DEBUG new ApResolver
  19:07:56 DEBUG Requesting https://apresolve.spotify.com/?type=accesspoint&type=dealer&type=spclient
  19:07:56 DEBUG spotify: GET https://api.spotify.com/v1/me/playlists?limit=50
  ```
  Find where this warning is raised/cleared (likely `sources/spotify`) and fix the stale-warning state instead of requiring a manual click to force the clear.
- [ ] The `[bd]` BPM-scan status tag (`bpm_status_tag` in `ui/src/view.rs`) isn't visible on app
  startup even though the BPM plugin is disabled/paused at that point — investigate why the status
  line's tag doesn't show until some later redraw/state change and fix it to appear immediately.
### Features
- [ ] The Settings pane (`ui/src/view.rs`'s `settings_lines`/`Pane::Settings`) is currently just a
  read-only scrollable text dump — make it interactive, at least enough to toggle each plugin/source
  on/off from there (the same enable/disable state that plugin-specific config already tracks, e.g.
  Soulseek's `enabled` toggle) instead of only being editable via `config.toml`.
- [ ] Add a YouTube source/plugin (alongside the existing Spotify/SoundCloud/HTTP/local sources), wired into Search like the others.
- [ ] Ignore mouse events in the log panel so the user can select/copy text with the mouse instead of the panel capturing clicks/drags as input.

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
