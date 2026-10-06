//! `app` — front-end wiring for medley.
//!
//! The only public seam is [`build_session`], the named constructor the M1c
//! integration test and the future `main.rs` both call.

#[cfg(target_os = "linux")]
pub mod mpris;
#[cfg(target_os = "macos")]
pub mod media_keys_macos;
mod media_keys_common;
pub mod session_builder;
pub mod web;

pub use session_builder::build_session;
