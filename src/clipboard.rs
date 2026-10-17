//! Copying text to the system clipboard and reading it back.
//!
//! Text is sent both through the terminal (OSC 52 — works over SSH and without extra tools in
//! kitty, foot, alacritty, wezterm, tmux with `set-clipboard on`…) and through the first available
//! clipboard utility (wl-copy, xclip, xsel).

use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn copy(text: &str) {
    osc52(text);
    let text = text.to_string();
    // Utilities may block (e.g. a misbehaving X server): keep the UI responsive.
    std::thread::spawn(move || {
        let _ = copy_with_tool(&text);
    });
}

fn osc52(text: &str) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

fn copy_with_tool(text: &str) -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let tools: [(bool, &str, &[&str]); 3] = [
        (wayland, "wl-copy", &[]),
        (x11, "xclip", &["-selection", "clipboard"]),
        (x11, "xsel", &["--clipboard", "--input"]),
    ];
    tools.iter().filter(|(ok, ..)| *ok).any(|(_, bin, args)| run(bin, args, text))
}

/// Text from the clipboard via wl-paste, xclip or xsel (None if none of them works within
/// a short timeout, so a hanging tool does not freeze the UI).
pub fn paste() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        let tools: [(bool, &str, &[&str]); 3] = [
            (wayland, "wl-paste", &["--no-newline"]),
            (x11, "xclip", &["-selection", "clipboard", "-o"]),
            (x11, "xsel", &["--clipboard", "--output"]),
        ];
        let text = tools.iter().filter(|(ok, ..)| *ok).find_map(|(_, bin, args)| {
            let out = Command::new(bin).args(*args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        });
        let _ = tx.send(text);
    });
    rx.recv_timeout(Duration::from_millis(300)).ok().flatten()
}

fn run(bin: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child.stdin.take().is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && written
}

#[cfg(test)]
mod tests {
    /// Uses the real clipboard (overwrites it). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn tool_copy() {
        let url = "https://music.yandex.ru/album/9/track/123";
        assert!(super::copy_with_tool(url));
        let out = std::process::Command::new("wl-paste").arg("--no-newline").output().expect("wl-paste");
        assert_eq!(String::from_utf8_lossy(&out.stdout), url);
    }
}
