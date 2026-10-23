//! Audio playback: the own player (`engine`) — download, decoding, processing and output to the
//! sound server or an ALSA device on Linux, to Core Audio on macOS.

#[cfg(target_os = "linux")]
mod alsa;
#[cfg(target_os = "macos")]
mod coreaudio;
mod decode;
mod dsp;
mod engine;
pub mod eq;
mod fetch;
mod output;
mod ring;

pub use dsp::Biquad;
pub use engine::Player;
use std::sync::Arc;

/// Locking that goes on after a panic in another thread holding the lock: the state under these
/// locks is updated in small steps, and one failed thread should not bring the rest down with a
/// second panic.
pub(crate) trait Lock<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> Lock<T> for std::sync::Mutex<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How the player is set up: where the sound goes and how downloads connect.
#[derive(Clone)]
pub struct Options {
    /// `pulse` or `alsa` (Linux), `pulse` (macOS): that output only; otherwise the usual one and,
    /// on Linux, ALSA when there is no sound server.
    pub output: Option<String>,
    /// The ALSA device (`hw:1,0` or `1,0`); otherwise every one, card by card.
    pub alsa_device: Option<String>,
    /// TLS for the downloads.
    pub tls: Arc<rustls::ClientConfig>,
}

#[derive(Debug)]
pub enum PlayerEvent {
    TimePos(f64),
    /// A track has started (or was seeked in): its length, 0 if unknown.
    Duration(f64),
    Pause(bool),
    Volume(f64),
    Mute(bool),
    Eof,
    Error(String),
    /// The player is gone for good; the program exits with this message.
    Died(String),
    /// Levels of the spectrum bands (`SPECTRUM_BANDS`), dB, low to high.
    Spectrum(Vec<f32>),
    /// The output asked for by `PlayerCmd::Output` did not open; `kept` is the one still in use.
    OutputFailed { kept: Option<String>, error: String },
}

/// Bands of the spectrum visualizer: log-spaced from `SPECTRUM_LOW` to `SPECTRUM_HIGH` Hz.
pub const SPECTRUM_BANDS: usize = 16;
const SPECTRUM_LOW: f64 = 40.0;
const SPECTRUM_HIGH: f64 = 16000.0;

/// Width of a band in octaves (bands touch each other).
pub fn spectrum_step() -> f64 {
    (SPECTRUM_HIGH / SPECTRUM_LOW).log2() / (SPECTRUM_BANDS - 1) as f64
}

/// Center frequency of band `i`, Hz.
pub fn spectrum_freq(i: usize) -> f64 {
    SPECTRUM_LOW * 2f64.powf(spectrum_step() * i as f64)
}

/// A track to play: where its file is and what the app knows of it.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackFile {
    pub url: String,
    /// The length the catalogue tells, seconds: used when the file does not tell its own (an MP3
    /// without a Xing header), since a crossfade starts by it.
    pub length: Option<f64>,
    /// Where to start, seconds (0 — from the beginning).
    pub start: f64,
}

/// What the app asks the player to do.
#[derive(Clone)]
pub enum PlayerCmd {
    Load(TrackFile),
    /// Appends a URL to the playlist: it is prefetched and played after the current one.
    Append(TrackFile),
    Next,
    /// Drops the appended entries: only the current track stays in the playlist.
    ClearNext,
    TogglePause,
    ToggleMute,
    Seek(f64),
    Volume(f64),
    Eq(&'static eq::Preset),
    /// The crossfade length in seconds (0 — off).
    Fade(f64),
    /// Turns the spectrum measurement on or off.
    Spectrum(bool),
    /// Asks for the current band levels: they come as `PlayerEvent::Spectrum`.
    RequestSpectrum,
    /// Moves the sound to another output (as `Options::output`).
    Output(Option<String>),
    Stop,
}

/// As logs and tests show the commands.
impl std::fmt::Display for PlayerCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            PlayerCmd::Load(t) if t.start > 0.0 => write!(f, "load {} from {}", t.url, t.start),
            PlayerCmd::Load(t) => write!(f, "load {}", t.url),
            PlayerCmd::Append(t) => write!(f, "append {}", t.url),
            PlayerCmd::Next => write!(f, "next"),
            PlayerCmd::ClearNext => write!(f, "clear"),
            PlayerCmd::TogglePause => write!(f, "pause"),
            PlayerCmd::ToggleMute => write!(f, "mute"),
            PlayerCmd::Seek(secs) => write!(f, "seek {secs}"),
            PlayerCmd::Volume(v) => write!(f, "volume {v}"),
            PlayerCmd::Eq(preset) => write!(f, "eq {}", preset.id),
            PlayerCmd::Fade(secs) => write!(f, "fade {secs}"),
            PlayerCmd::Spectrum(on) => write!(f, "spectrum {on}"),
            PlayerCmd::RequestSpectrum => write!(f, "spectrum?"),
            PlayerCmd::Output(o) => write!(f, "output {}", o.as_deref().unwrap_or("auto")),
            PlayerCmd::Stop => write!(f, "stop"),
        }
    }
}
