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
    /// Equalizer part of the `af` chain.
    eq: String,
    /// Fade length in seconds (0 — off) and the current track's duration it is computed for.
    fade: f64,
    duration: f64,
}

/// Volume filter for fades: rises over `fade` seconds from the track start and falls over the
/// last `fade` seconds before `duration`. `t` is the position within the current track: mpv
/// recreates the filter for each track, which is also why the duration has to be set again.
fn fade_filter(fade: f64, duration: f64) -> String {
    if fade <= 0.0 {
        return String::new();
    }
    let expr = if duration > 0.0 { format!("min(1,min(t,{duration:.3}-t)/{fade})") } else { format!("min(1,t/{fade})") };
    format!("@fade:lavfi=[volume=volume='{expr}':eval=frame]")
}

impl Player {
    pub fn spawn<M: From<PlayerEvent> + Send + 'static>(tx: Sender<M>, volume: u8) -> Result<Player> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let sock = dir.join(format!("ymusic-cli-{}.sock", std::process::id()));
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
            .context("не удалось запустить mpv — установите его")?;

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

        let mut p = Player { child, stream, sock, eq: String::new(), fade: 0.0, duration: 0.0 };
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

    /// Sets the equalizer filter (empty string removes it).
    pub fn set_eq_filter(&mut self, af: &str) {
        self.eq = af.to_string();
        self.apply_filters();
    }

    /// Sets the fade length in seconds (0 — off).
    pub fn set_fade(&mut self, secs: f64) {
        self.fade = secs.max(0.0);
        self.apply_filters();
    }

    /// Duration of the track mpv has just started: the fade-out is placed before its end.
    pub fn set_track_duration(&mut self, secs: f64) {
        // Changing the chain re-creates the filters, so skip insignificant duration refinements.
        if (secs - self.duration).abs() > 0.5 {
            self.duration = secs;
            if self.fade > 0.0 {
                self.apply_filters();
            }
        }
    }

    fn apply_filters(&mut self) {
        let fade = fade_filter(self.fade, self.duration);
        let af = [fade.as_str(), self.eq.as_str()].into_iter().filter(|f| !f.is_empty()).collect::<Vec<_>>().join(",");
        self.cmd(json!(["set_property", "af", af]));
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

#[cfg(test)]
mod tests {
    use super::fade_filter;

    #[test]
    fn fade() {
        assert_eq!(fade_filter(0.0, 200.0), "");
        assert_eq!(fade_filter(5.0, 0.0), "@fade:lavfi=[volume=volume='min(1,t/5)':eval=frame]");
        assert_eq!(fade_filter(5.0, 200.5), "@fade:lavfi=[volume=volume='min(1,min(t,200.500-t)/5)':eval=frame]");
    }
}
