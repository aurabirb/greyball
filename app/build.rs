//! Stamps `MEDLEY_GIT_HASH` (`git describe`: tag-commits-ghash[-dirty]) into the binary; "unknown" without git.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().map(|s| s.trim().to_string())
}

fn main() {
    let version = git(&["describe", "--tags", "--always", "--dirty", "--abbrev=12"])
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MEDLEY_GIT_HASH={version}");

    // Misses an unstaged edit to a tracked file; fine for a dev build stamp.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    println!("cargo:rerun-if-changed=../.git/refs/tags");
}
