//! The ring between the decoding thread and the sound output, with marks at track boundaries.
//!
//! The frames go through a single-producer single-consumer buffer of atomics: the output (on
//! macOS the audio callback, which must never wait) takes them without locks or system calls.
//! What only the producer and the control thread share — marks, spectra, a requested seek, the
//! generation — is under a mutex the output never touches.

use crate::Lock;
use std::sync::PoisonError;
use super::output::RATE;
use std::collections::VecDeque;
use std::io;
#[cfg(not(target_os = "linux"))]
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(not(target_os = "linux"))]
use std::os::fd::AsRawFd;
#[cfg(not(target_os = "linux"))]
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The ring holds 100 ms; the output's own buffer comes on top.
const CAPACITY: usize = RATE as usize / 10;
/// How long a producer facing a full ring waits before looking again: the output does not wake
/// it (that would be a system call on the audio thread).
const FULL_WAIT: Duration = Duration::from_millis(10);
/// Measurements kept for the visualizer: about the sound in the ring and in the output.
const MAX_SPECTRA: usize = 32;

/// Where something happens in the stream of frames: a track starts, ends or fails.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// A track (`job`) starts at `at` seconds into it; `gapless` — right after the previous one.
    Start { job: u64, at: f64, duration: Option<f64>, gapless: bool },
    End,
    /// `network` — see `PlayerEvent::Error`.
    Error { message: String, network: bool },
}

/// What the producer and the control thread share.
#[derive(Default)]
struct Meta {
    /// Bumped by `clear`: a producer still pushing older audio is told to stop.
    generation: u64,
    marks: VecDeque<(u64, Mark)>,
    /// The job being decoded and a seek requested in it.
    producing: u64,
    seek: Option<f64>,
    /// Spectrum measurements by the frame they end at.
    spectra: VecDeque<(u64, Vec<f32>)>,
}

/// Interleaved stereo frames on their way to the output, with marks at track boundaries.
pub struct Ring {
    /// `f32` bits, frame `n` at `2n % len`. The producer writes only past `written`, the output
    /// reads only between `read` and `written`.
    slots: Box<[AtomicU32]>,
    /// Frames ever pushed, published once they are in `slots`.
    written: AtomicU64,
    /// Frames ever handed to the output; a clear moves it to `written` (dropped ones count as
    /// handed). Only grows: both the output and `clear` move it with `fetch_max`.
    read: AtomicU64,
    /// Gain (volume and mute), `f32` bits: applied as the frames leave the ring, so a change is
    /// heard after the output's buffer only, not the ring's too.
    gain: AtomicU32,
    /// The output found the ring empty and waits to be woken when it is not.
    starving: AtomicBool,
    wake: Mutex<Option<Arc<Wake>>>,
    meta: Mutex<Meta>,
    /// Wakes a producer waiting for room when the ring is cleared.
    cleared: Condvar,
}

impl Default for Ring {
    fn default() -> Ring {
        Ring {
            slots: (0..CAPACITY * 2).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            gain: AtomicU32::new(1f32.to_bits()),
            starving: AtomicBool::new(false),
            wake: Mutex::new(None),
            meta: Mutex::default(),
            cleared: Condvar::new(),
        }
    }
}

/// Applies the volume to a sample; past full scale (volume over 100 %) it clips, as in mpv.
fn amplify(sample: f32, gain: f32) -> f32 {
    (sample * gain).clamp(-1.0, 1.0)
}

impl Ring {
    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Appends frames, waiting while the ring is full. False if it was cleared since `generation`.
    pub fn push(&self, mut data: &[f32], generation: u64) -> bool {
        let mut meta = self.meta.locked();
        while !data.is_empty() {
            if meta.generation != generation {
                return false;
            }
            // Under `meta`: a clear cannot slip in between the check above and the publishing below.
            let written = self.written.load(Ordering::Relaxed);
            let read = self.read.load(Ordering::Acquire);
            let room = (CAPACITY - (written - read) as usize).min(data.len() / 2);
            if room == 0 {
                meta = self.cleared.wait_timeout(meta, FULL_WAIT).unwrap_or_else(PoisonError::into_inner).0;
                continue;
            }
            let start = written as usize * 2;
            for (i, s) in data[..room * 2].iter().enumerate() {
                self.slots[(start + i) % self.slots.len()].store(s.to_bits(), Ordering::Relaxed);
            }
            self.written.store(written + room as u64, Ordering::SeqCst);
            data = &data[room * 2..];
            // Paired with `take_with`: either the output sees the frames or this sees it starving.
            if self.starving.swap(false, Ordering::SeqCst)
                && let Some(w) = &*self.wake.locked()
            {
                w.signal();
            }
        }
        true
    }

    /// For the output: hands up to `max` frames to `put`, sample by sample with the gain applied;
    /// returns the frames handed. Never blocks. When there are none, a waiting output thread is
    /// woken once some are pushed.
    fn take_with(&self, max: usize, mut put: impl FnMut(f32)) -> usize {
        let read = self.read.load(Ordering::Acquire);
        let mut written = self.written.load(Ordering::SeqCst);
        if written == read {
            self.starving.store(true, Ordering::SeqCst);
            written = self.written.load(Ordering::SeqCst);
            if written == read {
                return 0;
            }
            self.starving.store(false, Ordering::Relaxed);
        }
        // A clear meanwhile only moves `read` on: what is taken here is then dropped by the flush
        // that follows a clear.
        let frames = ((written - read) as usize).min(max);
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let start = read as usize * 2;
        for i in 0..frames * 2 {
            put(amplify(f32::from_bits(self.slots[(start + i) % self.slots.len()].load(Ordering::Relaxed)), gain));
        }
        self.read.fetch_max(read + frames as u64, Ordering::Release);
        frames
    }

    /// For the Core Audio callback: fills `out` (interleaved stereo) from the start; returns the
    /// frames written.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn take_into(&self, out: &mut [f32]) -> usize {
        let mut i = 0;
        self.take_with(out.len() / 2, |s| {
            out[i] = s;
            i += 1;
        })
    }

    /// For the ALSA output thread: appends up to `max` frames to `out`; returns the frames moved.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn take_samples(&self, max: usize, out: &mut Vec<f32>) -> usize {
        self.take_with(max, |s| out.push(s))
    }

    /// For the PulseAudio output thread: appends up to `max` bytes of frames as `f32`
    /// little-endian; returns the bytes moved.
    pub(super) fn take_bytes(&self, max: usize, out: &mut Vec<u8>) -> usize {
        self.take_with(max / 8, |s| out.extend(s.to_le_bytes())) * 8
    }

    pub(super) fn set_wake(&self, wake: Arc<Wake>) {
        *self.wake.locked() = Some(wake);
        self.starving.store(false, Ordering::SeqCst);
    }

    /// Frames handed to the output so far.
    pub(super) fn handed(&self) -> u64 {
        self.read.load(Ordering::Acquire)
    }

    /// Puts a mark at the current end of the stream; returns the generation it belongs to.
    pub fn mark(&self, mark: Mark) -> u64 {
        let mut meta = self.meta.locked();
        let at = self.written.load(Ordering::Relaxed);
        meta.marks.push_back((at, mark));
        meta.generation
    }

    /// Drops the frames not yet handed to the output and all pending marks.
    pub fn clear(&self) {
        self.clear_locked(&mut self.meta.locked());
    }

    fn clear_locked(&self, meta: &mut Meta) {
        self.read.fetch_max(self.written.load(Ordering::SeqCst), Ordering::Release);
        meta.marks.clear();
        meta.spectra.clear();
        meta.seek = None;
        meta.generation += 1;
        self.cleared.notify_all();
    }

    /// Clears the ring and asks the producer to seek, if it is decoding `job`. Both at once, so
    /// the producer never sees the ring cleared without the seek.
    pub fn request_seek(&self, job: u64, secs: f64) -> bool {
        let mut meta = self.meta.locked();
        if meta.producing != job {
            return false;
        }
        self.clear_locked(&mut meta);
        meta.seek = Some(secs);
        true
    }

    /// For the producer: starts decoding `job`; returns the current generation.
    pub fn begin(&self, job: u64) -> u64 {
        let mut meta = self.meta.locked();
        meta.producing = job;
        meta.seek = None;
        meta.generation
    }

    /// For the producer: the current generation and a seek requested since the last call.
    pub fn poll_seek(&self) -> (u64, Option<f64>) {
        let mut meta = self.meta.locked();
        let seek = meta.seek.take();
        (meta.generation, seek)
    }

    /// Stores band levels measured over the sound about to be pushed, ending `offset` frames into
    /// it; dropped if the ring was cleared since `generation`.
    pub fn push_spectrum(&self, generation: u64, offset: usize, levels: Vec<f32>) {
        let mut meta = self.meta.locked();
        if meta.generation != generation {
            return;
        }
        let at = self.written.load(Ordering::Relaxed) + offset as u64;
        if meta.spectra.len() == MAX_SPECTRA {
            meta.spectra.pop_front();
        }
        meta.spectra.push_back((at, levels));
    }

    /// The latest band levels playback has reached by frame `played`.
    pub fn spectrum(&self, played: u64) -> Option<Vec<f32>> {
        let mut meta = self.meta.locked();
        let mut last = None;
        while meta.spectra.front().is_some_and(|(at, _)| *at <= played) {
            last = meta.spectra.pop_front().map(|(_, l)| l);
        }
        last
    }

    /// Takes the marks that playback has reached by frame `played`, with the frames they are at.
    pub fn reached(&self, played: u64) -> Vec<(u64, Mark)> {
        let mut meta = self.meta.locked();
        let mut out = Vec::new();
        while meta.marks.front().is_some_and(|(at, _)| *at <= played) {
            out.extend(meta.marks.pop_front());
        }
        out
    }
}

/// An eventfd that wakes the output thread from its `poll`.
#[cfg(target_os = "linux")]
pub(super) struct Wake(OwnedFd);

#[cfg(target_os = "linux")]
impl Wake {
    pub(super) fn new() -> io::Result<Wake> {
        // SAFETY: plain syscall; the descriptor is owned from here on.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor nobody else owns.
        Ok(Wake(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub(super) fn signal(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a live buffer to our own eventfd.
        unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    pub(super) fn fd(&self) -> i32 {
        self.0.as_raw_fd()
    }

    pub(super) fn reset(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: reads at most 8 bytes into a live buffer from our own eventfd.
        unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    }
}

/// Elsewhere (macOS has no eventfd) a socket pair: a byte written to one end wakes the other.
#[cfg(not(target_os = "linux"))]
pub(super) struct Wake {
    rx: UnixStream,
    tx: UnixStream,
}

#[cfg(not(target_os = "linux"))]
impl Wake {
    pub(super) fn new() -> io::Result<Wake> {
        let (rx, tx) = UnixStream::pair()?;
        rx.set_nonblocking(true)?;
        tx.set_nonblocking(true)?;
        Ok(Wake { rx, tx })
    }

    pub(super) fn signal(&self) {
        // A full buffer already holds a wake-up, so a failed write loses nothing.
        let _ = (&self.tx).write(&[1]);
    }

    pub(super) fn fd(&self) -> i32 {
        self.rx.as_raw_fd()
    }

    pub(super) fn reset(&self) {
        let mut buf = [0u8; 64];
        while matches!((&self.rx).read(&mut buf), Ok(n) if n > 0) {}
    }
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, Mark, Ring, Wake};
    use std::sync::Arc;

    #[test]
    fn marks_and_clearing() {
        let ring = Arc::new(Ring::default());
        let generation = ring.begin(1);
        assert_eq!(ring.mark(Mark::Start { job: 1, at: 0.0, duration: None, gapless: false }), generation);
        assert!(ring.push(&[0.5; 100], generation));
        ring.mark(Mark::End);
        assert_eq!(ring.reached(0).len(), 1);
        assert!(ring.reached(49).is_empty());
        assert_eq!(ring.reached(50), vec![(50, Mark::End)]);

        // A full ring blocks the producer until it is cleared; then its push reports that.
        let r = ring.clone();
        let producer = std::thread::spawn(move || r.push(&vec![0.0; CAPACITY * 4], generation));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!producer.is_finished());
        assert!(!ring.request_seek(2, 10.0), "not the job being decoded");
        assert!(ring.request_seek(1, 10.0));
        assert!(!producer.join().unwrap());
        let (new_gen, seek) = ring.poll_seek();
        assert_eq!((new_gen, seek), (generation + 1, Some(10.0)));
        assert_eq!(ring.poll_seek().1, None);
        // Everything pushed counts as handed: playback position continues from there.
        assert_eq!(ring.handed(), CAPACITY as u64);
        // The cleared frames are not played; new ones are, at once.
        assert!(ring.push(&[0.25; 4], new_gen));
        let mut out = Vec::new();
        assert_eq!(ring.take_samples(100, &mut out), 2);
        assert_eq!(out, [0.25; 4]);
    }

    #[test]
    fn volume_is_applied_on_the_way_out() {
        let ring = Ring::default();
        let generation = ring.begin(1);
        ring.push(&[0.5, -0.5, 0.8, 0.8], generation);
        // Set after the push: the frames already in the ring get it.
        ring.set_gain(0.5);
        let mut out = [0.0; 2];
        assert_eq!(ring.take_into(&mut out), 1);
        assert_eq!(out, [0.25, -0.25]);
        // Over 100 % it clips at full scale.
        ring.set_gain(2.0);
        let mut bytes = Vec::new();
        assert_eq!(ring.take_bytes(8, &mut bytes), 8);
        assert_eq!(bytes, [1f32.to_le_bytes(), 1f32.to_le_bytes()].concat());
    }

    /// The frames come out in order across the wrap-around, with one thread pushing and another
    /// taking at its own pace; an output that found the ring empty is woken.
    #[test]
    fn producer_and_consumer() {
        let ring = Arc::new(Ring::default());
        let wake = Arc::new(Wake::new().unwrap());
        ring.set_wake(wake.clone());
        let generation = ring.begin(1);
        let total = CAPACITY * 7 + 123;
        let r = ring.clone();
        let producer = std::thread::spawn(move || {
            let data: Vec<f32> = (0..total).flat_map(|i| [i as f32 / total as f32, -(i as f32) / total as f32]).collect();
            for chunk in data.chunks(2 * 997) {
                assert!(r.push(chunk, generation));
            }
        });
        let mut got = Vec::new();
        let mut buf = vec![0.0; 2 * 512];
        while got.len() < total * 2 {
            match ring.take_into(&mut buf) {
                0 => {
                    let mut fd = libc::pollfd { fd: wake.fd(), events: libc::POLLIN, revents: 0 };
                    // SAFETY: one valid pollfd.
                    assert!(unsafe { libc::poll(&mut fd, 1, 5000) } == 1, "the output was not woken");
                    wake.reset();
                }
                n => got.extend_from_slice(&buf[..n * 2]),
            }
        }
        producer.join().unwrap();
        let expected = |i: usize| [i as f32 / total as f32, -(i as f32) / total as f32];
        assert!(got.as_chunks::<2>().0.iter().enumerate().all(|(i, f)| *f == expected(i)));
        assert_eq!(ring.handed(), total as u64);
    }
}
