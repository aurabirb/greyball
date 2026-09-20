use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

type Tool = (&'static str, &'static [&'static str]);

fn tools() -> Vec<Tool> {
    let mut t: Vec<Tool> = Vec::new();
    if cfg!(target_os = "macos") {
        t.push(("pbcopy", &[]));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        t.push(("wl-copy", &[]));
    }
    if std::env::var_os("DISPLAY").is_some() {
        t.push(("xclip", &["-selection", "clipboard"]));
        t.push(("xsel", &["--clipboard", "--input"]));
    }
    t.push(("clip.exe", &[]));
    t
}

fn run(cmd: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Copies via a system clipboard tool, else the terminal (OSC 52, works over SSH); returns the method.
pub(super) fn copy(text: &str) -> String {
    for (cmd, args) in tools() {
        if run(cmd, args, text) {
            return cmd.to_string();
        }
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
    "OSC 52".to_string()
}
