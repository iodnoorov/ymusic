//! Audio playback through an mpv child process controlled over JSON IPC.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
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

/// Playback commands the app sends; mpv in the program, a recorder in tests.
pub trait Audio {
    fn load(&mut self, url: &str);
    /// Appends a URL to the playlist: it is prefetched and played after the current one.
    fn append(&mut self, url: &str);
    fn playlist_next(&mut self);
    fn toggle_pause(&mut self);
    fn toggle_mute(&mut self);
    fn seek(&mut self, secs: f64);
    fn set_volume(&mut self, v: f64);
    /// Sets the equalizer filter (empty string removes it).
    fn set_eq_filter(&mut self, af: &str);
    /// Sets the fade length in seconds (0 — off).
    fn set_fade(&mut self, secs: f64);
    /// Duration of the track that has just started: the fade-out is placed before its end.
    fn set_track_duration(&mut self, secs: f64);
    fn stop(&mut self);
}

/// Extra mpv options from `YMUSIC_MPV_ARGS` (split on whitespace) and a log file from `YMUSIC_MPV_LOG`.
fn user_args() -> Vec<String> {
    let mut args: Vec<String> =
        crate::config::env_str("MPV_ARGS").unwrap_or_default().split_whitespace().map(String::from).collect();
    if let Some(log) = crate::config::env_str("MPV_LOG").filter(|l| !l.is_empty()) {
        args.push(format!("--log-file={log}"));
    }
    args
}

/// mpv gets SIGTERM when ymusic dies, even by SIGKILL: otherwise it keeps playing
/// with nothing left to stop it.
fn exit_with_parent(cmd: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls (prctl, getppid) run between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent died before prctl took effect.
            if libc::getppid() != parent {
                return Err(std::io::Error::other("ymusic уже завершился"));
            }
            Ok(())
        });
    }
}

/// Removes sockets left by earlier runs that were killed before `Drop` (their pid is gone).
fn remove_stale_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_prefix("ymusic-cli-")?.strip_suffix(".sock")?.parse::<u32>().ok());
        if pid.is_some_and(|pid| !std::path::Path::new(&format!("/proc/{pid}")).exists()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Player {
    pub fn spawn<M: From<PlayerEvent> + Send + 'static>(tx: Sender<M>, volume: u8) -> Result<Player> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        remove_stale_sockets(&dir);
        let sock = dir.join(format!("ymusic-cli-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let mut cmd = Command::new("mpv");
        exit_with_parent(&mut cmd);
        let child = cmd
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
                // A stalled connection is dropped (and reopened) after 10 s of silence instead of 60.
                "--network-timeout=10",
            ])
            .arg(format!("--volume={volume}"))
            .arg(format!("--input-ipc-server={}", sock.display()))
            .args(user_args())
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

    fn apply_filters(&mut self) {
        let fade = fade_filter(self.fade, self.duration);
        let af = [fade.as_str(), self.eq.as_str()].into_iter().filter(|f| !f.is_empty()).collect::<Vec<_>>().join(",");
        self.cmd(json!(["set_property", "af", af]));
    }
}

impl Audio for Player {
    fn load(&mut self, url: &str) {
        self.cmd(json!(["loadfile", url, "replace"]));
        self.cmd(json!(["set_property", "pause", false]));
    }

    fn append(&mut self, url: &str) {
        self.cmd(json!(["loadfile", url, "append"]));
    }

    fn playlist_next(&mut self) {
        self.cmd(json!(["playlist-next", "force"]));
        self.cmd(json!(["set_property", "pause", false]));
    }

    fn toggle_pause(&mut self) {
        self.cmd(json!(["cycle", "pause"]));
    }

    fn toggle_mute(&mut self) {
        self.cmd(json!(["cycle", "mute"]));
    }

    fn seek(&mut self, secs: f64) {
        self.cmd(json!(["seek", secs, "relative"]));
    }

    fn set_volume(&mut self, v: f64) {
        self.cmd(json!(["set_property", "volume", v]));
    }

    fn set_eq_filter(&mut self, af: &str) {
        self.eq = af.to_string();
        self.apply_filters();
    }

    fn set_fade(&mut self, secs: f64) {
        self.fade = secs.max(0.0);
        self.apply_filters();
    }

    fn set_track_duration(&mut self, secs: f64) {
        // Changing the chain re-creates the filters, so skip insignificant duration refinements.
        if (secs - self.duration).abs() > 0.5 {
            self.duration = secs;
            if self.fade > 0.0 {
                self.apply_filters();
            }
        }
    }

    fn stop(&mut self) {
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

/// Records the commands instead of playing them (tests run without mpv).
#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeAudio(pub std::rc::Rc<std::cell::RefCell<Vec<String>>>);

#[cfg(test)]
impl FakeAudio {
    fn push(&self, cmd: String) {
        self.0.borrow_mut().push(cmd);
    }

    /// Commands recorded so far; the log is cleared.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

#[cfg(test)]
impl Audio for FakeAudio {
    fn load(&mut self, url: &str) {
        self.push(format!("load {url}"));
    }
    fn append(&mut self, url: &str) {
        self.push(format!("append {url}"));
    }
    fn playlist_next(&mut self) {
        self.push("next".into());
    }
    fn toggle_pause(&mut self) {
        self.push("pause".into());
    }
    fn toggle_mute(&mut self) {
        self.push("mute".into());
    }
    fn seek(&mut self, secs: f64) {
        self.push(format!("seek {secs}"));
    }
    fn set_volume(&mut self, v: f64) {
        self.push(format!("volume {v}"));
    }
    fn set_eq_filter(&mut self, af: &str) {
        self.push(format!("eq {af}"));
    }
    fn set_fade(&mut self, secs: f64) {
        self.push(format!("fade {secs}"));
    }
    fn set_track_duration(&mut self, secs: f64) {
        self.push(format!("duration {secs}"));
    }
    fn stop(&mut self) {
        self.push("stop".into());
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

    /// mpv is stopped when the thread that started it is gone, even if `Drop` never ran
    /// (as after SIGKILL). Needs mpv: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn mpv_exits_with_parent() {
        use super::{Player, PlayerEvent};
        let pid = std::thread::spawn(|| {
            let (tx, _rx) = std::sync::mpsc::channel::<PlayerEvent>();
            let p = Player::spawn(tx, 0).expect("mpv");
            let pid = p.child.id();
            let sock = p.sock.clone();
            std::mem::forget(p);
            let _ = std::fs::remove_file(sock);
            pid
        })
        .join()
        .unwrap();
        // The child is never reaped here, so a finished mpv stays a zombie (state Z).
        let gone = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_or(true, |s| s.rsplit(')').next().is_some_and(|r| r.trim_start().starts_with('Z')))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !gone() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(gone(), "mpv {pid} is still running");
    }
}
