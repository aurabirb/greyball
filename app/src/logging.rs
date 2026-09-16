//! File logging + a terminal-restoring panic hook.
//!
//! A TUI can't log to stderr without corrupting the display, so everything goes
//! to a file: `$XDG_STATE_HOME/medley/medley.log` (fallback
//! `~/.local/state/medley/medley.log`), truncated on each start.
//!
//! Level resolution, first match wins:
//!   1. `--log-level <off|error|warn|info|debug|trace>` on the command line
//!   2. `RUST_LOG` (only the bare level word is honoured — not the full
//!      env_logger target syntax)
//!   3. default: `info`

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use medley_core::LogBuf;
use log::{Level, LevelFilter, Log, Metadata, Record};

fn state_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(x).join("medley");
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local")
        .join("state")
        .join("medley")
}

pub fn log_path() -> PathBuf {
    state_dir().join("medley.log")
}

fn parse_level(s: &str) -> Option<LevelFilter> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

/// `--log-level X` from argv, if present.
fn level_from_args() -> Option<LevelFilter> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--log-level" {
            return args.next().and_then(|v| parse_level(&v));
        }
        if let Some(v) = a.strip_prefix("--log-level=") {
            return parse_level(v);
        }
    }
    None
}

fn resolve_level() -> LevelFilter {
    level_from_args()
        .or_else(|| std::env::var("RUST_LOG").ok().and_then(|v| parse_level(&v)))
        .unwrap_or(LevelFilter::Info)
}

/// When a decoder resyncs past corrupted/undecrypted audio (see
/// `sources/spotify/tests/decrypt_key_unavailable.rs`), it can log dozens of
/// near-identical WARNs in a fraction of a second before it finally gives up
/// and errors out — the failure itself is already handled correctly
/// (`Unavailable`/`EndOfTrack` -> next track), only the logging is noisy.
/// Groups the target down to its crate name so a burst collapses to one
/// line instead of flooding the log/`:log` pane.
fn decode_noise_key(target: &str) -> Option<&'static str> {
    if target.starts_with("symphonia") {
        Some("symphonia")
    } else if target.starts_with("librespot_playback::decoder") {
        Some("librespot_playback::decoder")
    } else {
        None
    }
}

/// Warnings that librespot re-emits identically on every call (not just
/// within a burst) — e.g. `librespot_core::cache::Cache::credentials()` warns
/// about world-readable credential file permissions on *every* read, which
/// means every session reconnect re-triggers it. These are only useful once.
fn once_per_run_key(target: &str, message: &str) -> Option<&'static str> {
    if target.starts_with("librespot_core::cache") && message.contains("is currently world readable")
    {
        Some("librespot_core::cache: world-readable credentials warning")
    } else {
        None
    }
}

struct Burst {
    key: &'static str,
    /// How many *additional* matching WARNs came in after the first (which
    /// was already logged normally), i.e. how many were suppressed.
    suppressed: u32,
}

/// A `log::Log` that writes to the file logger and also tees a formatted line
/// into a [`LogBuf`], so `ui`'s Log pane (`:log`) can show a recent tail
/// without reading the file back.
struct TeeLogger {
    level: LevelFilter,
    file: Box<simplelog::WriteLogger<std::fs::File>>,
    buf: Arc<LogBuf>,
    burst: Mutex<Option<Burst>>,
    once_seen: Mutex<std::collections::HashSet<&'static str>>,
}

impl TeeLogger {
    /// Actually writes a record out, bypassing burst coalescing.
    fn emit(&self, record: &Record) {
        self.file.log(record);
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let time = chrono::Local::now().format("%H:%M:%S");
        self.buf.push(format!("{time} {level} {}", record.args()));
    }

    /// Emits the one-line summary for a finished burst, if it ever grew
    /// past the single (already-logged) first occurrence.
    fn flush_burst(&self, burst: Option<Burst>) {
        let Some(burst) = burst else { return };
        if burst.suppressed == 0 {
            return;
        }
        let msg = format!(
            "...{} more \"{}\" warning(s) suppressed",
            burst.suppressed, burst.key
        );
        self.emit(
            &Record::builder()
                .level(Level::Warn)
                .target("logging::burst")
                .args(format_args!("{msg}"))
                .build(),
        );
    }
}

impl Log for TeeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        if record.level() == Level::Warn {
            let message = record.args().to_string();
            if let Some(k) = once_per_run_key(record.target(), &message) {
                let mut seen = self.once_seen.lock().unwrap_or_else(|e| e.into_inner());
                if !seen.insert(k) {
                    return;
                }
            }
        }

        let key = (record.level() == Level::Warn)
            .then(|| decode_noise_key(record.target()))
            .flatten();

        let mut burst = self.burst.lock().unwrap_or_else(|e| e.into_inner());
        match (burst.as_mut(), key) {
            (Some(b), Some(k)) if b.key == k => {
                b.suppressed += 1;
                return;
            }
            _ => {
                let finished = burst.take();
                *burst = key.map(|k| Burst { key: k, suppressed: 0 });
                drop(burst);
                self.flush_burst(finished);
            }
        }
        self.emit(record);
    }

    fn flush(&self) {
        let finished = self.burst.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.flush_burst(finished);
        self.file.flush();
    }
}

/// Install the file + ring-buffer logger and the panic hook. Returns the log
/// path (for a one-line note to stderr before cursive takes the screen) plus
/// the shared [`LogBuf`] `ui` reads from — or `None` if the log file could not
/// be opened, in which case logging is a silent no-op and the app still runs
/// (the caller should still pass a fresh empty `LogBuf` to `ui` in that case).
pub fn init() -> Option<(PathBuf, Arc<LogBuf>)> {
    let level = resolve_level();
    let path = log_path();
    let _ = std::fs::create_dir_all(state_dir());

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .ok()?;

    let config = simplelog::ConfigBuilder::new()
        .set_time_format_rfc3339()
        .set_thread_level(LevelFilter::Error)
        .set_target_level(LevelFilter::Error)
        .build();

    let buf = Arc::new(LogBuf::default());
    let tee = TeeLogger {
        level,
        file: simplelog::WriteLogger::new(level, config, file),
        buf: buf.clone(),
        burst: Mutex::new(None),
        once_seen: Mutex::new(std::collections::HashSet::new()),
    };
    log::set_max_level(level);
    log::set_boxed_logger(Box::new(tee)).ok()?;

    install_panic_hook();

    log::info!(
        "medley {} ({}) starting — log level {level}",
        env!("CARGO_PKG_VERSION"),
        env!("MEDLEY_GIT_HASH"),
    );
    Some((path, buf))
}

/// Chain a hook that writes the panic + a backtrace to the log, then delegates
/// to the previous hook (cursive installs its own that restores the terminal).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        log::error!("PANIC on thread \"{name}\": {info}\n{bt}");
        // Best-effort: also drop a copy next to the log in case logging itself
        // is what broke.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_dir().join("medley.panic"))
        {
            let _ = writeln!(f, "PANIC on \"{name}\": {info}\n{bt}\n---");
        }
        previous(info);
    }));
}

