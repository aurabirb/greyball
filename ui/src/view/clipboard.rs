use std::io::Write;
use std::sync::{Mutex, OnceLock};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

static SYSTEM: OnceLock<Option<Mutex<arboard::Clipboard>>> = OnceLock::new();

// Linux clipboard contents vanish when the owning handle drops, so it lives for the process.
fn system_copy(text: &str) -> bool {
    let cb = SYSTEM.get_or_init(|| arboard::Clipboard::new().ok().map(Mutex::new));
    let Some(cb) = cb else { return false };
    let Ok(mut cb) = cb.lock() else { return false };
    cb.set_text(text).is_ok()
}

/// Copies via the system clipboard, else the terminal (OSC 52, works over SSH); returns the method.
pub(super) fn copy(text: &str) -> String {
    if system_copy(text) {
        return "system clipboard".to_string();
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
    "OSC 52".to_string()
}
