//! The system's media controls: media keys and the desktop's player widgets control the program
//! and show what plays — MPRIS on Linux, Now Playing on macOS. The app hands over its state as it
//! changes; the commands come back to it as messages.

use std::time::Instant;

pub use crate::app::Repeat;

#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "macos")]
mod nowplaying;

#[cfg(target_os = "linux")]
pub use mpris::Mpris as Media;
#[cfg(target_os = "macos")]
pub use nowplaying::NowPlaying as Media;

/// A position this far both from where playback should be by now and from where it was is a
/// seek (a stall — loading, a slow network — goes on from where it was).
const SEEK_JUMP: f64 = 1.5;

/// A command from the system.
#[derive(Debug, Clone, PartialEq)]
pub enum Remote {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    /// Seek by this many seconds.
    Seek(f64),
    /// Go to this position of the current track, seconds.
    SeekTo(f64),
    /// Volume, % (100 — normal).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Volume(f64),
    Repeat(Repeat),
    /// Play the list in random order or in its own.
    Shuffle(bool),
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Quit,
}

/// What the system is shown.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub track: Option<TrackInfo>,
    pub status: Status,
    /// Volume, % (100 — normal).
    pub volume: f64,
    pub repeat: Repeat,
    /// The list plays in random order.
    pub shuffle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    /// Seconds.
    pub length: f64,
    pub art_url: Option<String>,
    pub url: String,
}

/// Where playback was last told to be and when: tells a seek from playback going on.
pub(crate) struct Clock {
    pos: f64,
    pub(crate) at: Instant,
}

impl Default for Clock {
    fn default() -> Clock {
        Clock { pos: 0.0, at: Instant::now() }
    }
}

impl Clock {
    /// Takes the new position (seconds); true if it is a seek within the same track.
    pub(crate) fn seeked(&mut self, position: f64, was_playing: bool, same_track: bool) -> bool {
        if position == self.pos {
            return false;
        }
        let expected = if was_playing { self.pos + self.at.elapsed().as_secs_f64() } else { self.pos };
        let seek = same_track && (position - expected).abs() > SEEK_JUMP && (position - self.pos).abs() > SEEK_JUMP;
        self.pos = position;
        self.at = Instant::now();
        seek
    }
}
