//! Stamps `MEDLEY_GIT_HASH` (short commit id, `-dirty` if the working tree has
//! uncommitted changes) into the binary via `env!`, so a running process's log
//! and `:settings` pane can say exactly what code they're built from — the
//! recurring "did you actually rebuild after pulling?" question gets a
//! one-line answer instead of a guess. Hand-rolled rather than a crate (e.g.
//! `vergen`): `librespot-core`'s own `vergen` dependency already broke on a
//! version bump once, not worth a second copy to manage.
//!
//! No `git` on `PATH`, or not a git checkout at all (e.g. a source tarball):
//! falls back to `"unknown"` — never fails the build.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().map(|s| s.trim().to_string())
}

fn main() {
    let hash = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // Empty stdout from `status --porcelain` = clean tree.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=MEDLEY_GIT_HASH={hash}{suffix}");

    // Rerun on a new commit / checkout / merge, or a staged change. Misses an
    // unstaged edit to an already-tracked file — acceptable slack for a
    // dev-facing build stamp, not worth watching the whole tree for.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
}
