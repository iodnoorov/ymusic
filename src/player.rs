//! Audio playback through an mpv child process controlled over JSON IPC.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum PlayerEvent {
    TimePos(f64),
    Duration(f64),
    Pause(bool),
    Volume(f64),
    Mute(bool),
    Eof,
    Error(String),
    Died,
}

pub struct Player {
    child: Child,
    stream: UnixStream,
    sock: PathBuf,
}

impl Player {
    pub fn spawn<M: From<PlayerEvent> + Send + 'static>(tx: Sender<M>, volume: u8) -> Result<Player> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let sock = dir.join(format!("yamusic-cli-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let child = Command::new("mpv")
            .args([
                "--idle=yes",
                "--no-video",
                "--no-terminal",
                "--audio-display=no",
                "--ytdl=no",
                "--cache=yes",
                "--force-window=no",
                "--keep-open=no",
                // Preload: the next playlist entry starts downloading once the current one is fully read.
                "--prefetch-playlist=yes",
                // A whole track fits; the default (150 MiB) is far more than music needs.
                "--demuxer-max-bytes=24MiB",
                "--demuxer-max-back-bytes=8MiB",
            ])
            .arg(format!("--volume={volume}"))
            .arg(format!("--input-ipc-server={}", sock.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("не удалось запустить mpv — установите его (pacman -S mpv / nix: pkgs.mpv)")?;

        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(anyhow!("mpv не открыл IPC-сокет: {e}")),
            }
        };

        let reader = stream.try_clone()?;
        std::thread::spawn(move || {
            // time-pos changes many times per second; forward at most ~4 updates/s.
            let mut last_pos = f64::NEG_INFINITY;
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let ev = match v["event"].as_str() {
                    Some("property-change") => match (v["name"].as_str(), &v["data"]) {
                        (Some("time-pos"), Value::Number(n)) => {
                            let t = n.as_f64().unwrap_or(0.0);
                            if (t - last_pos).abs() < 0.25 {
                                continue;
                            }
                            last_pos = t;
                            PlayerEvent::TimePos(t)
                        }
                        (Some("duration"), Value::Number(n)) => PlayerEvent::Duration(n.as_f64().unwrap_or(0.0)),
                        (Some("pause"), Value::Bool(b)) => PlayerEvent::Pause(*b),
                        (Some("volume"), Value::Number(n)) => PlayerEvent::Volume(n.as_f64().unwrap_or(0.0)),
                        (Some("mute"), Value::Bool(b)) => PlayerEvent::Mute(*b),
                        _ => continue,
                    },
                    Some("end-file") => match v["reason"].as_str() {
                        Some("eof") => PlayerEvent::Eof,
                        Some("error") => {
                            PlayerEvent::Error(v["file_error"].as_str().unwrap_or("ошибка воспроизведения").to_string())
                        }
                        _ => continue,
                    },
                    _ => continue,
                };
                if tx.send(ev.into()).is_err() {
                    return;
                }
            }
            let _ = tx.send(PlayerEvent::Died.into());
        });

        let mut p = Player { child, stream, sock };
        for (i, prop) in ["time-pos", "duration", "pause", "volume", "mute"].iter().enumerate() {
            p.cmd(json!(["observe_property", i + 1, prop]));
        }
        Ok(p)
    }

    fn cmd(&mut self, args: Value) {
        let _ = writeln!(self.stream, "{}", json!({ "command": args }));
    }

    pub fn load(&mut self, url: &str) {
        self.cmd(json!(["loadfile", url, "replace"]));
        self.cmd(json!(["set_property", "pause", false]));
    }

    /// Appends a URL to mpv's playlist (it is prefetched and played after the current one).
    pub fn append(&mut self, url: &str) {
        self.cmd(json!(["loadfile", url, "append"]));
    }

    pub fn playlist_next(&mut self) {
        self.cmd(json!(["playlist-next", "force"]));
        self.cmd(json!(["set_property", "pause", false]));
    }

    pub fn toggle_pause(&mut self) {
        self.cmd(json!(["cycle", "pause"]));
    }

    pub fn toggle_mute(&mut self) {
        self.cmd(json!(["cycle", "mute"]));
    }

    pub fn seek(&mut self, secs: f64) {
        self.cmd(json!(["seek", secs, "relative"]));
    }

    pub fn set_volume(&mut self, v: f64) {
        self.cmd(json!(["set_property", "volume", v]));
    }

    pub fn stop(&mut self) {
        self.cmd(json!(["stop"]));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.cmd(json!(["quit"]));
        std::thread::sleep(Duration::from_millis(50));
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}
