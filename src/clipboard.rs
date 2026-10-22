//! Copying text to the system clipboard and reading it back.
//!
//! Text is sent both through the terminal (OSC 52 — works over SSH and without extra tools in
//! kitty, foot, alacritty, wezterm, tmux with `set-clipboard on`…) and through the first available
//! clipboard utility (wl-copy, xclip, xsel on Linux; pbcopy on macOS).

use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Sends the text to the terminal's clipboard (OSC 52). Whether the terminal supports it is unknown.
pub fn osc52(text: &str) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

/// Clipboard utilities to try: (program, arguments) to copy and to paste.
type Tool = (&'static str, &'static [&'static str]);

#[cfg(target_os = "linux")]
fn tools() -> (Vec<Tool>, Vec<Tool>) {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let copy: [(bool, Tool); 3] = [
        (wayland, ("wl-copy", &[])),
        (x11, ("xclip", &["-selection", "clipboard"])),
        (x11, ("xsel", &["--clipboard", "--input"])),
    ];
    let paste: [(bool, Tool); 3] = [
        (wayland, ("wl-paste", &["--no-newline"])),
        (x11, ("xclip", &["-selection", "clipboard", "-o"])),
        (x11, ("xsel", &["--clipboard", "--output"])),
    ];
    let available = |tools: [(bool, Tool); 3]| tools.into_iter().filter(|(ok, _)| *ok).map(|(_, t)| t).collect();
    (available(copy), available(paste))
}

#[cfg(target_os = "macos")]
fn tools() -> (Vec<Tool>, Vec<Tool>) {
    (vec![("pbcopy", &[])], vec![("pbpaste", &[])])
}

/// Copies with the first clipboard utility that works; false if there is none.
/// May block (e.g. a misbehaving X server): call it off the UI thread.
pub fn copy_with_tool(text: &str) -> bool {
    tools().0.iter().any(|(bin, args)| run(bin, args, text))
}

/// Text from the clipboard via the first utility that works (None if none of them works within
/// a short timeout, so a hanging tool does not freeze the UI).
pub fn paste() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let text = tools().1.iter().find_map(|(bin, args)| {
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
        assert_eq!(super::paste().as_deref(), Some(url));
    }
}
