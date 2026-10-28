//! The terminal window's (tab's) title: what plays now. The terminal's own title comes back on
//! exit, where it keeps a stack of titles (xterm, kitty, VTE, Konsole…).
//!
//! Konsole shows a program's title only if the tab title format has `%w` (the default is
//! `%d : %n`, the directory and the program). It lets a program change that format (OSC 30), so
//! while the program runs the format is `%w`, and the one it was is put back on exit; it is asked
//! for over D-Bus, the only way to know it.

use std::io::Write;

pub struct Title {
    shown: String,
    /// Konsole's tab title format to put back on exit.
    konsole_format: Option<String>,
}

fn send(s: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(s.as_bytes()).and_then(|()| out.flush());
}

impl Title {
    pub fn start() -> Title {
        // Saves the title on the terminal's stack.
        send("\x1b[22;0t");
        let konsole_format = konsole_format().filter(|f| !f.contains("%w"));
        if konsole_format.is_some() {
            send("\x1b]30;%w\x07");
        }
        Title { shown: String::new(), konsole_format }
    }

    pub fn set(&mut self, title: &str) {
        if title != self.shown {
            // Control characters would end the sequence early.
            let clean: String = title.chars().filter(|c| !c.is_control()).collect();
            send(&format!("\x1b]0;{clean}\x07"));
            self.shown = title.to_string();
        }
    }

    pub fn restore(self) {
        if let Some(f) = &self.konsole_format {
            send(&format!("\x1b]30;{f}\x07"));
        }
        send("\x1b[23;0t");
    }
}

/// The tab title format of the Konsole session the program runs in (`None` — not Konsole, or it
/// did not answer in time).
#[cfg(target_os = "linux")]
fn konsole_format() -> Option<String> {
    let service = std::env::var("KONSOLE_DBUS_SERVICE").ok()?;
    let session = std::env::var("KONSOLE_DBUS_SESSION").ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    // A D-Bus that does not answer must not hold up the start.
    std::thread::Builder::new()
        .name("konsole".into())
        .spawn(move || {
            let format = zbus::block_on(async {
                let conn = zbus::Connection::session().await?;
                let reply = conn
                    .call_method(Some(service.as_str()), session.as_str(), Some("org.kde.konsole.Session"), "tabTitleFormat", &(0i32,))
                    .await?;
                reply.body().deserialize::<String>()
            });
            let _ = tx.send(format.ok());
        })
        .ok()?;
    rx.recv_timeout(std::time::Duration::from_millis(500)).ok().flatten()
}

#[cfg(not(target_os = "linux"))]
fn konsole_format() -> Option<String> {
    None
}
