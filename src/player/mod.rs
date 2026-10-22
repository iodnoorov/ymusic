//! Audio playback: the own player (`engine`) — download, decoding, processing and output to the
//! sound server or an ALSA device on Linux, to Core Audio on macOS.

#[cfg(target_os = "linux")]
mod alsa;
#[cfg(target_os = "macos")]
mod coreaudio;
mod decode;
mod dsp;
mod engine;
mod fetch;
mod output;

pub use dsp::Biquad;
pub use engine::Player;

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

/// Playback commands the app sends; the player in the program, a recorder in tests.
pub trait Audio {
    fn load(&mut self, url: &str);
    /// Appends a URL to the playlist: it is prefetched and played after the current one.
    fn append(&mut self, url: &str);
    fn playlist_next(&mut self);
    /// Drops the appended entries: only the current track stays in the playlist.
    fn clear_next(&mut self);
    fn toggle_pause(&mut self);
    fn toggle_mute(&mut self);
    fn seek(&mut self, secs: f64);
    fn set_volume(&mut self, v: f64);
    fn set_eq(&mut self, preset: &'static crate::eq::Preset);
    /// Sets the fade length in seconds (0 — off).
    fn set_fade(&mut self, secs: f64);
    /// Turns the spectrum measurement on or off.
    fn set_spectrum(&mut self, on: bool);
    /// Asks for the current band levels: they come as `PlayerEvent::Spectrum`.
    fn request_spectrum(&mut self);
    fn stop(&mut self);
}

/// Records the commands instead of playing them (tests run without sound).
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
    fn clear_next(&mut self) {
        self.push("clear".into());
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
    fn set_eq(&mut self, preset: &'static crate::eq::Preset) {
        self.push(format!("eq {}", preset.id));
    }
    fn set_fade(&mut self, secs: f64) {
        self.push(format!("fade {secs}"));
    }
    fn set_spectrum(&mut self, on: bool) {
        self.push(format!("spectrum {on}"));
    }
    fn request_spectrum(&mut self) {
        self.push("spectrum?".into());
    }
    fn stop(&mut self) {
        self.push("stop".into());
    }
}
