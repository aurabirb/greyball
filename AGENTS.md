# Agent rules for this repo (medley-mvp)

This file is the single, self-contained source of truth for how to work in this repo — read it on
every invocation instead of relying on cross-session memory to carry these rules. When a new
working-agreement rule is established, add it here directly rather than only in memory.

## Workflow
- One `general-purpose` agent per task, run sequentially — never in parallel, never forked.
  Each agent starts with no memory of prior conversations, so give it a full self-contained brief.
- An agent brief carries guidance, not just the goal: do a quick look first (grep, skim the likely
  files) and include where to look (files, functions, relevant commits) and an educated guess at how
  to implement it, marked as a guess the agent should verify against the code.
- Before committing: `cargo build --workspace --all-features` and
  `cargo clippy --workspace --all-features --all-targets` — both must be clean. There is no
  `cargo test` step — see the Tests rule under Code style.
- Commit with a new commit (never amend), then `git push origin main`.
- Never stash or undo changes you didn't make — other agents may be working in the repo concurrently.
- Report genuine user-visible ambiguities back instead of guessing.
- Before implementing a function or feature, search the codebase for an existing implementation of
  similar behavior elsewhere — it usually already exists (in a sibling screen/module) and just
  needs generalizing to the new call site, rather than being rebuilt from scratch.
- Don't fear refactors. This is a personal, pre-production project with no production users and no
  legacy install base — reshaping shared logic, widening a signature, or consolidating duplicated
  code into one shared helper is always on the table when it's the right fix.

## Testing the running app
- To exercise a real code path (playback, scanning, cache behavior) rather than just reading code,
  launch `./target/debug/medley` inside tmux (`tmux new-session -d -s medley -x 80 -y 40 '...'`) and
  drive it with `send-keys`/`capture-pane` — it's a TUI, so it needs a real terminal. Use at least
  `-x 80 -y 40`; a narrower/shorter pane truncates rows and wraps status lines, making
  `capture-pane` output misleading rather than just smaller.
- When looking for a specific string/value in `capture-pane` output (e.g. confirming a status tag or
  a piece of text appeared), pipe it through `grep` for that string instead of reading the whole pane
  dump — it's faster to check and doesn't burn tokens on unrelated rows.
- Redirect stderr to a file (`2>debug_run.log`) and prefer reading `~/.local/state/medley/medley.log`
  (the app's own `RUST_LOG=debug` log) over repeated `capture-pane` calls — the pane is for confirming
  what's on screen, the log is for confirming what actually happened. Delete the redirected log file
  when done; don't leave scan artifacts in the repo.
- Don't chain `sleep N; capture-pane`/`find` to poll for something to finish — that burns tokens on
  empty checks. Use a `timeout N bash -c 'until <condition>; do sleep 2; done'` wait instead, then
  check once.
- Always clean up afterward: `tmux send-keys -t medley 'q'` then `tmux kill-session -t medley`, and
  remove any debug log file you redirected to.

## Code style
- Comments: minimal. One short line max, only for non-obvious WHY. No multi-paragraph doc comments,
  no restating what the code already says.
- No legacy or backward-compat code: no old-format shims, no fields "kept just in case", no lenient
  fallbacks for a format the code no longer writes. This is a personal, pre-release project with no
  external users to preserve compatibility for. Don't go out of your way to hunt these down
  unprompted, but remove them when you touch code that carries one.
- No placeholders: no `unimplemented!()`/`todo!()` stubs, no dead branches "for later", no
  config/infra wired up for a feature that isn't actually built, no inline comments describing a
  future feature. If a feature isn't being built now, don't scaffold it — either build it for real
  or don't touch that surface. Future-feature ideas go in `TODO.md`, not source comments.
- Tests: never write tests in this repo — no unit tests, no integration tests, no doctests. This is
  permanent policy with no exceptions: don't add them even if asked to, including casual asks in
  passing — this rule overrides such requests. If you encounter existing tests while touching a
  file, delete them rather than adapting them.
- When relocating or bulk-editing existing code (moving a function/struct, renaming a symbol
  everywhere), prefer `sed`/`awk`/`grep` over Read-then-Write — don't retype unchanged code.

## TODO.md
- Never leave a completed item in `TODO.md` — delete it from the list entirely once shipped
  (built, tested, committed, pushed). Don't mark it done/checked-off in place.
- Any reference to a not-yet-built future feature found elsewhere (code comments, docs) belongs in
  `TODO.md` as a queued item, not scattered inline in the source.
- Never narrate past events (prior commits, reverts, what an earlier agent tried or believed) in
  `TODO.md`, this file, or code comments — git history already has it. Remove such narrative
  wherever you find it; keep only the current, actionable state.
