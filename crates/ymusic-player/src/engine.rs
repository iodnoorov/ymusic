//! The own player. A control thread takes commands, talks to the sound server and reports
//! events; a producer thread downloads and decodes tracks and pushes the sound into the ring
//! between them. Track boundaries travel through the ring as marks, so events follow what is
//! heard rather than what is decoded. With a crossfade the producer decodes two tracks at once
//! for a while and mixes them.

use crate::Lock;
use super::decode::Decoder;
use super::dsp::{crossfade_gains, Equalizer, Limiter, Resampler, Spectrum};
use super::fetch::Download;
use super::output::{Output, RATE};
use super::ring::{Mark, Ring};
use super::{spectrum_freq, spectrum_step, Options, PlayerCmd, PlayerEvent, TrackFile, SPECTRUM_BANDS};
use crate::eq::Preset;
use anyhow::Result;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the position is reported and the marks are checked.
const TICK: Duration = Duration::from_millis(250);
/// Frames the producer handles at a time (≈ 93 ms).
const CHUNK: usize = 4096;

/// One track: its download and whether it has been abandoned. The download stops once the last
/// handle to the job is gone.
struct Job {
    id: u64,
    download: Download,
    cancelled: AtomicBool,
    /// See `TrackFile`.
    length: Option<f64>,
    start: f64,
}

impl Job {
    fn new(file: &TrackFile, tls: &Arc<rustls::ClientConfig>) -> Arc<Job> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Arc::new(Job {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            download: Download::start(&file.url, tls),
            cancelled: AtomicBool::new(false),
            length: file.length,
            start: file.start,
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.download.cancel();
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.download.cancel();
    }
}

/// State of both threads.
struct Shared {
    ring: Arc<Ring>,
    /// The appended track: the producer moves on to it when the current one ends.
    next: Mutex<Option<Arc<Job>>>,
    /// The appended track while it is being crossfaded into: already heard, but `Next` still
    /// moves on to it at once.
    incoming: Mutex<Option<Arc<Job>>>,
    /// Bumped whenever what plays is dropped (a new track, `Next`, stop): the producer abandons
    /// the tracks it is decoding.
    epoch: AtomicU64,
    settings: Mutex<Settings>,
}

/// What the producer does to the sound besides the volume.
#[derive(Clone, Copy, Default)]
struct Settings {
    eq: Option<&'static Preset>,
    /// Crossfade length, seconds (0 — off).
    fade: f64,
    spectrum: bool,
    /// Bumped on every change: the producer rebuilds its filters.
    version: u64,
}

impl Shared {
    fn change(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = self.settings.locked();
        f(&mut s);
        s.version += 1;
    }

    fn set_gain(&self, volume: f64, muted: bool) {
        // The same cubic curve as mpv's volume, so that the saved volume sounds the same.
        self.ring.set_gain(if muted { 0.0 } else { (volume / 100.0).powi(3) as f32 });
    }
}

/// Messages to the producer.
enum Work {
    /// Play this track now.
    Play(Arc<Job>),
    /// A track was appended (matters when the producer has already run out of sound).
    Appended,
}

enum Cmd {
    Load(TrackFile),
    Append(TrackFile),
    Next,
    ClearNext,
    TogglePause,
    ToggleMute,
    Seek(f64),
    Volume(f64),
    RequestSpectrum,
    Output(Option<String>),
    Stop,
}

pub struct Player {
    shared: Arc<Shared>,
    cmd: Option<Sender<Cmd>>,
    control: Option<JoinHandle<()>>,
}

impl Player {
    pub fn spawn<M: From<PlayerEvent> + Send + 'static>(tx: Sender<M>, volume: u8, options: Options) -> Result<Player> {
        let shared = Arc::new(Shared {
            ring: Arc::new(Ring::default()),
            next: Mutex::new(None),
            incoming: Mutex::new(None),
            epoch: AtomicU64::new(0),
            settings: Mutex::new(Settings { version: 1, ..Settings::default() }),
        });
        shared.set_gain(f64::from(volume), false);
        let output = Output::open(shared.ring.clone(), &options)?;
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (work_tx, work_rx) = mpsc::channel();
        let s = shared.clone();
        std::thread::Builder::new().name("player-decode".into()).spawn(move || produce(&s, &work_rx))?;
        let s = shared.clone();
        let control = std::thread::Builder::new().name("player-control".into()).spawn(move || {
            let mut c = Control {
                shared: s,
                options,
                output: Some(output),
                work: work_tx,
                live: Vec::new(),
                cur: None,
                seek_to: None,
                last_pos: f64::NEG_INFINITY,
                paused: false,
                muted: false,
                volume: f64::from(volume),
            };
            c.run(&cmd_rx, &tx);
        })?;
        Ok(Player { shared, cmd: Some(cmd_tx), control: Some(control) })
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.cmd {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The control thread stops (and stops the producer) once its channel is closed.
        self.cmd = None;
        if let Some(c) = self.control.take() {
            let _ = c.join();
        }
    }
}

impl Player {
    pub fn apply(&mut self, cmd: PlayerCmd) {
        match cmd {
            PlayerCmd::Load(file) => self.send(Cmd::Load(file)),
            PlayerCmd::Append(file) => self.send(Cmd::Append(file)),
            PlayerCmd::Next => self.send(Cmd::Next),
            PlayerCmd::ClearNext => self.send(Cmd::ClearNext),
            PlayerCmd::TogglePause => self.send(Cmd::TogglePause),
            PlayerCmd::ToggleMute => self.send(Cmd::ToggleMute),
            PlayerCmd::Seek(secs) => self.send(Cmd::Seek(secs)),
            PlayerCmd::Volume(v) => self.send(Cmd::Volume(v)),
            PlayerCmd::Eq(preset) => self.shared.change(|s| s.eq = (!preset.is_flat()).then_some(preset)),
            PlayerCmd::Fade(secs) => self.shared.change(|s| s.fade = secs.max(0.0)),
            PlayerCmd::Spectrum(on) => self.shared.change(|s| s.spectrum = on),
            PlayerCmd::RequestSpectrum => self.send(Cmd::RequestSpectrum),
            PlayerCmd::Output(output) => self.send(Cmd::Output(output)),
            PlayerCmd::Stop => self.send(Cmd::Stop),
        }
    }
}

/// The track being heard: `at` seconds into it at frame `frame` of the output stream.
struct Current {
    job: u64,
    at: f64,
    frame: u64,
}

struct Control {
    shared: Arc<Shared>,
    options: Options,
    output: Option<Output>,
    work: Sender<Work>,
    /// Tracks that may still be playing or be played: cancelled when the playlist is replaced.
    live: Vec<Arc<Job>>,
    cur: Option<Current>,
    /// Target of a seek the producer has not reached yet: the position to report meanwhile.
    seek_to: Option<f64>,
    last_pos: f64,
    paused: bool,
    muted: bool,
    volume: f64,
}

impl Control {
    fn run<M: From<PlayerEvent>>(&mut self, cmds: &Receiver<Cmd>, tx: &Sender<M>) {
        let mut last_tick = Instant::now();
        loop {
            let mut events = Vec::new();
            let result = match cmds.recv_timeout(TICK.saturating_sub(last_tick.elapsed())) {
                Ok(cmd) => self.command(cmd, &mut events),
                Err(RecvTimeoutError::Timeout) => Ok(()),
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let result = result.and_then(|()| {
                if last_tick.elapsed() >= TICK {
                    last_tick = Instant::now();
                    self.tick(&mut events)
                } else {
                    Ok(())
                }
            });
            if let Err(e) = result {
                // The sound server went away (or restarted): connect again once.
                // Dropped: closed.
                self.output = None;
                match self.open_output() {
                    Ok(o) => self.output = Some(o),
                    Err(e2) => {
                        let msg = format!("звук пропал: {e:#}; переподключиться не удалось: {e2:#}");
                        let _ = tx.send(PlayerEvent::Died(msg).into());
                        break;
                    }
                }
            }
            for ev in events {
                if tx.send(ev.into()).is_err() {
                    break;
                }
            }
        }
        self.reset_quietly();
        self.output = None;
    }

    fn output(&self) -> Result<&Output> {
        self.output.as_ref().ok_or_else(|| anyhow::anyhow!("нет звукового вывода"))
    }

    fn command(&mut self, cmd: Cmd, events: &mut Vec<PlayerEvent>) -> Result<()> {
        match cmd {
            Cmd::Load(file) => {
                self.reset()?;
                self.start(Job::new(&file, &self.options.tls));
                self.set_paused(false, events)?;
            }
            Cmd::Append(file) => {
                let job = Job::new(&file, &self.options.tls);
                if let Some(old) = self.shared.next.locked().replace(job.clone()) {
                    old.cancel();
                }
                self.live.push(job);
                let _ = self.work.send(Work::Appended);
            }
            Cmd::Next => {
                // During a crossfade the incoming track starts over on its own.
                let next = self.shared.next.locked().take().or_else(|| self.shared.incoming.locked().take());
                if let Some(job) = next {
                    self.live.retain(|j| j.id != job.id);
                    self.reset()?;
                    self.start(job);
                    self.set_paused(false, events)?;
                }
            }
            Cmd::ClearNext => {
                if let Some(job) = self.shared.next.locked().take() {
                    job.cancel();
                    self.live.retain(|j| j.id != job.id);
                }
            }
            Cmd::TogglePause => self.set_paused(!self.paused, events)?,
            Cmd::ToggleMute => {
                self.muted = !self.muted;
                self.shared.set_gain(self.volume, self.muted);
                events.push(PlayerEvent::Mute(self.muted));
            }
            Cmd::Volume(v) => {
                self.volume = v;
                self.shared.set_gain(self.volume, self.muted);
                events.push(PlayerEvent::Volume(v));
            }
            Cmd::Seek(d) => {
                if let Some(c) = &self.cur {
                    let pos = match self.seek_to {
                        Some(t) => t,
                        None => self.position(c, self.output()?.played(&self.shared.ring)?),
                    };
                    let target = (pos + d).max(0.0);
                    if self.shared.ring.request_seek(c.job, target) {
                        self.output()?.flush()?;
                        self.seek_to = Some(target);
                        self.last_pos = f64::NEG_INFINITY;
                    }
                }
            }
            Cmd::RequestSpectrum => {
                if let Some(levels) = self.shared.ring.spectrum(self.output()?.played(&self.shared.ring)?) {
                    events.push(PlayerEvent::Spectrum(levels));
                }
            }
            Cmd::Output(output) if output != self.options.output => {
                let kept = std::mem::replace(&mut self.options.output, output);
                // The old one is closed first: it may hold the device the new one needs (ALSA).
                self.output = None;
                match self.open_output() {
                    Ok(o) => self.output = Some(o),
                    Err(e) => {
                        events.push(PlayerEvent::OutputFailed { kept: kept.clone(), error: format!("{e:#}") });
                        self.options.output = kept;
                        self.output = Some(self.open_output()?);
                    }
                }
            }
            Cmd::Output(_) => {}
            Cmd::Stop => self.reset()?,
        }
        Ok(())
    }

    /// Opens the output of `options`, paused or not as the player is.
    fn open_output(&self) -> Result<Output> {
        let output = Output::open(self.shared.ring.clone(), &self.options)?;
        output.set_paused(self.paused)?;
        Ok(output)
    }

    fn set_paused(&mut self, paused: bool, events: &mut Vec<PlayerEvent>) -> Result<()> {
        self.output()?.set_paused(paused)?;
        self.paused = paused;
        events.push(PlayerEvent::Pause(paused));
        Ok(())
    }

    fn start(&mut self, job: Arc<Job>) {
        self.live.push(job.clone());
        let _ = self.work.send(Work::Play(job));
    }

    /// Cancels every track and drops the sound on its way.
    fn reset(&mut self) -> Result<()> {
        self.reset_quietly();
        self.output()?.flush()
    }

    fn reset_quietly(&mut self) {
        self.shared.epoch.fetch_add(1, Ordering::Relaxed);
        self.shared.incoming.locked().take();
        for job in self.live.drain(..) {
            job.cancel();
        }
        if let Some(job) = self.shared.next.locked().take() {
            job.cancel();
        }
        self.shared.ring.clear();
        self.cur = None;
        self.seek_to = None;
    }

    fn position(&self, c: &Current, played: u64) -> f64 {
        c.at + played.saturating_sub(c.frame) as f64 / f64::from(RATE)
    }

    /// Reports what playback has reached: track starts and ends, errors, the position.
    fn tick(&mut self, events: &mut Vec<PlayerEvent>) -> Result<()> {
        let played = self.output()?.played(&self.shared.ring)?;
        for (frame, mark) in self.shared.ring.reached(played) {
            match mark {
                Mark::Start { job, at, duration, gapless } => {
                    if gapless {
                        events.push(PlayerEvent::Eof);
                    }
                    // Earlier tracks are over: free their downloads.
                    self.live.retain(|j| j.id >= job);
                    self.cur = Some(Current { job, at, frame });
                    self.seek_to = None;
                    self.last_pos = f64::NEG_INFINITY;
                    // Also tells the app that the track has started (0 — the length is unknown).
                    events.push(PlayerEvent::Duration(duration.unwrap_or(0.0)));
                }
                Mark::End | Mark::Error(_) => {
                    if let Some(c) = self.cur.take() {
                        self.live.retain(|j| j.id > c.job);
                    }
                    events.push(match mark {
                        Mark::Error(e) => PlayerEvent::Error(e),
                        _ => PlayerEvent::Eof,
                    });
                }
            }
        }
        if let Some(c) = &self.cur {
            let t = self.seek_to.unwrap_or_else(|| self.position(c, played));
            if (t - self.last_pos).abs() >= 0.25 {
                self.last_pos = t;
                events.push(PlayerEvent::TimePos(t));
            }
        }
        Ok(())
    }
}

fn produce(shared: &Shared, work: &Receiver<Work>) {
    let mut pending: Option<(Arc<Job>, bool)> = None;
    // Kept from track to track: a seamless one goes on with the sound still in the look-ahead.
    let mut effects = Effects::new();
    loop {
        let (job, gapless) = match pending.take() {
            Some(p) => p,
            None => match work.recv() {
                Ok(Work::Play(job)) => (job, false),
                Ok(Work::Appended) => match shared.next.locked().take() {
                    // The previous track has already ended: this one is not seamless.
                    Some(job) => (job, false),
                    None => continue,
                },
                Err(_) => return,
            },
        };
        // A track loaded meanwhile replaces this one.
        let newest = work.try_iter().filter_map(|w| if let Work::Play(j) = w { Some(j) } else { None }).last();
        let (job, gapless) = newest.map_or((job, gapless), |j| (j, false));
        if !job.cancelled() {
            pending = play(shared, job, gapless, &mut effects);
        }
    }
}

/// A track on its way into the ring: decoded and resampled to our rate.
struct Track {
    /// Keeps the download going while the track is decoded.
    _job: Option<Arc<Job>>,
    dec: Decoder,
    resampler: Resampler,
    duration: Option<f64>,
    /// Seconds into the track of the next frame `read` gives out.
    pos: f64,
    /// Resampled frames not given out yet.
    pending: VecDeque<f32>,
    ended: bool,
    resampled: Vec<f32>,
}

impl Track {
    fn open(job: &Arc<Job>) -> Result<Track> {
        let mut track = Track::from_decoder(Decoder::open(Box::new(job.download.reader()))?)?;
        track._job = Some(job.clone());
        track.duration = track.duration.or(job.length);
        Ok(track)
    }

    fn from_decoder(dec: Decoder) -> Result<Track> {
        Ok(Track {
            _job: None,
            resampler: Resampler::to(dec.sample_rate(), RATE),
            duration: dec.duration(),
            dec,
            pos: 0.0,
            pending: VecDeque::new(),
            ended: false,
            resampled: Vec::new(),
        })
    }

    /// Appends up to `frames` frames to `out`; fewer only at the end of the track.
    fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize> {
        while self.pending.len() < frames * 2 && !self.ended {
            match self.dec.next_chunk()? {
                Some(chunk) => {
                    self.resampled.clear();
                    self.resampler.process(chunk, &mut self.resampled);
                    self.pending.extend(&self.resampled);
                }
                None => self.ended = true,
            }
        }
        let n = frames.min(self.pending.len() / 2);
        out.extend(self.pending.drain(..n * 2));
        self.pos += n as f64 / f64::from(RATE);
        Ok(n)
    }

    /// Moves to `secs`; returns where it landed.
    fn seek(&mut self, secs: f64) -> Result<f64> {
        let at = self.dec.seek(secs)?;
        self.resampler.reset();
        self.pending.clear();
        self.ended = false;
        self.pos = at;
        Ok(at)
    }

    /// Seconds left, if the length is known.
    fn left(&self) -> Option<f64> {
        self.duration.map(|d| d - self.pos)
    }
}

/// The track being faded out while the next one comes in.
struct Crossfade {
    from: Track,
    /// Length and how far it has gone, seconds.
    len: f64,
    done: f64,
}

impl Crossfade {
    /// Mixes the outgoing track into `buf` (the incoming one's frames), both with their gains.
    /// Returns false once it is over.
    fn mix(&mut self, buf: &mut [f32], scratch: &mut Vec<f32>) -> bool {
        let frames = buf.len() / 2;
        scratch.clear();
        // A failing outgoing track just ends early.
        let got = self.from.read(frames, scratch).unwrap_or(0);
        for (i, frame) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let (out, inc) = crossfade_gains((self.done + i as f64 / f64::from(RATE)) / self.len);
            let old = scratch.get(i * 2..i * 2 + 2).unwrap_or(&[0.0, 0.0]);
            frame[0] = frame[0] * inc + old[0] * out;
            frame[1] = frame[1] * inc + old[1] * out;
        }
        self.done += frames as f64 / f64::from(RATE);
        self.done < self.len && got == frames
    }
}

/// Decodes `job` into the ring, crossfading into the appended track when the setting is on.
/// Returns the appended track to go on with without a crossfade.
fn play(shared: &Shared, job: Arc<Job>, gapless: bool, effects: &mut Effects) -> Option<(Arc<Job>, bool)> {
    let ring = &shared.ring;
    let epoch = shared.epoch.load(Ordering::Relaxed);
    ring.begin(job.id);
    let fail = |job: &Job, e: anyhow::Error| {
        if !job.cancelled() {
            ring.mark(Mark::Error(format!("{e:#}")));
        }
        None
    };
    let mut cur = match Track::open(&job) {
        Ok(t) => t,
        Err(e) => return fail(&job, e),
    };
    let mut job = job;
    // Resumed after a failure: where it stopped (from the beginning if that cannot be reached).
    let at = if job.start > 0.0 { cur.seek(job.start).unwrap_or(0.0) } else { 0.0 };
    ring.mark(Mark::Start { job: job.id, at, duration: cur.duration, gapless });
    if !gapless {
        effects.reset();
    }
    let mut fade: Option<Crossfade> = None;
    let (mut buf, mut scratch) = (Vec::new(), Vec::new());
    loop {
        if job.cancelled() || shared.epoch.load(Ordering::Relaxed) != epoch {
            return None;
        }
        let (mut generation, seek) = ring.poll_seek();
        if let Some(t) = seek {
            // A seek ends a crossfade: only the incoming track goes on.
            fade = None;
            shared.incoming.locked().take();
            // Past the end the demuxer fails: the track is over.
            let Ok(at) = cur.seek(t) else { break };
            effects.reset();
            generation = ring.mark(Mark::Start { job: job.id, at, duration: cur.duration, gapless: false });
        }
        let settings = *shared.settings.locked();
        effects.sync(settings);
        if fade.is_none() && settings.fade > 0.0 && cur.left().is_some_and(|l| l <= settings.fade)
            && let Some((next, track)) = open_next(shared)
        {
            // As long as is left (the next track may have come late), at most half of either track.
            let half = |t: &Track| t.duration.map_or(f64::INFINITY, |d| d / 2.0);
            let len = cur.left().unwrap_or(0.0).min(half(&cur)).min(half(&track));
            if len > 0.0 {
                generation = ring.begin(next.id);
                ring.mark(Mark::Start { job: next.id, at: 0.0, duration: track.duration, gapless: true });
                *shared.incoming.locked() = Some(next.clone());
                let from = std::mem::replace(&mut cur, track);
                fade = Some(Crossfade { from, len, done: 0.0 });
                job = next;
            } else {
                // Nothing left to overlap: the track follows right after.
                *shared.next.locked() = Some(next);
            }
        }
        buf.clear();
        let frames = match cur.read(CHUNK, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return fail(&job, e),
        };
        if let Some(f) = &mut fade
            && !f.mix(&mut buf[..frames * 2], &mut scratch)
        {
            fade = None;
            shared.incoming.locked().take();
        }
        effects.process(&mut buf, |offset, levels| ring.push_spectrum(generation, offset, levels));
        // A refused push means a seek or a new track: the loop above sorts it out.
        ring.push(&buf, generation);
    }
    shared.incoming.locked().take();
    if let Some(next) = shared.next.locked().take() {
        return Some((next, true));
    }
    ring.mark(Mark::End);
    None
}

/// Takes the appended track for a crossfade and opens it; one that fails to open is left to be
/// played (and to report its error) the usual way.
fn open_next(shared: &Shared) -> Option<(Arc<Job>, Track)> {
    let next = shared.next.locked().take()?;
    match Track::open(&next) {
        Ok(t) => Some((next, t)),
        Err(_) => {
            let mut slot = shared.next.locked();
            // Another track may have been appended meanwhile.
            if slot.is_none() {
                *slot = Some(next);
            }
            None
        }
    }
}

/// The processing of the sound as mpv's filter chain did it: equalizer with its limiter, then
/// the spectrum measurement (before the volume, so the bars do not depend on it). Between them a
/// limiter at full scale: two loud tracks crossfaded add up to more than it, and so may a track
/// itself; the volume is applied later, on output.
struct Effects {
    version: u64,
    eq: Option<(&'static str, Equalizer)>,
    peak: Limiter,
    spectrum: Option<Spectrum>,
}

impl Effects {
    fn new() -> Effects {
        Effects { version: 0, eq: None, peak: Limiter::new(1.0, f64::from(RATE)), spectrum: None }
    }

    /// Follows changed settings; an unchanged equalizer keeps its state.
    fn sync(&mut self, s: Settings) {
        if s.version == self.version {
            return;
        }
        self.version = s.version;
        if s.eq.map(|p| p.id) != self.eq.as_ref().map(|(id, _)| *id) {
            self.eq = s.eq.map(|p| (p.id, Equalizer::new(p.biquads(f64::from(RATE)), f64::from(RATE))));
        }
        if s.spectrum != self.spectrum.is_some() {
            self.spectrum = s.spectrum.then(|| {
                let freqs: Vec<f64> = (0..SPECTRUM_BANDS).map(spectrum_freq).collect();
                Spectrum::new(&freqs, spectrum_step(), f64::from(RATE))
            });
        }
    }

    /// Forgets the sound so far (a seek, a track that does not follow the previous one).
    fn reset(&mut self) {
        if let Some((_, eq)) = &mut self.eq {
            eq.reset();
        }
        self.peak.reset();
        if let Some(s) = &mut self.spectrum {
            s.reset();
        }
    }

    /// Processes a chunk; spectrum measurements go to `measured` with the frame they end at
    /// within the chunk.
    fn process(&mut self, buf: &mut [f32], measured: impl FnMut(usize, Vec<f32>)) {
        if let Some((_, eq)) = &mut self.eq {
            eq.process(buf);
        }
        self.peak.process(buf);
        if let Some(s) = &mut self.spectrum {
            s.feed(buf, measured);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const LC: &[u8] = include_bytes!("../tests/data/aac-lc.m4a");
    const MP3: &[u8] = include_bytes!("../tests/data/mp3.mp3");

    fn track(data: &'static [u8]) -> Track {
        Track::from_decoder(Decoder::open(Box::new(Cursor::new(data))).unwrap()).unwrap()
    }

    #[test]
    fn track_reads_by_frames() {
        let mut t = track(LC);
        let duration = t.duration.unwrap();
        assert!((duration - 2.0).abs() < 0.1, "{duration}");
        let mut out = Vec::new();
        assert_eq!(t.read(1000, &mut out).unwrap(), 1000);
        assert_eq!(out.len(), 2000);
        assert!((t.pos - 1000.0 / 44100.0).abs() < 1e-9);
        // A seek drops what was decoded ahead and goes on from there.
        assert!((t.seek(1.5).unwrap() - 1.5).abs() < 0.001);
        assert!((t.pos - 1.5).abs() < 0.001);
        assert_eq!(t.seek(0.0).unwrap(), 0.0);
        let mut total = 0;
        loop {
            match t.read(CHUNK, &mut out).unwrap() {
                0 => break,
                n => total += n,
            }
        }
        // The encoder pads the end a little.
        assert!((total as f64 / 44100.0 - duration).abs() < 0.05, "{total}");
        assert!(t.left().unwrap().abs() < 0.05);
    }

    /// A crossfade starts by the length, so every format the server gives must tell it.
    #[test]
    fn lengths_are_known() {
        for (name, data) in [("lc", LC), ("mp3", MP3), ("he", &include_bytes!("../tests/data/he-aac.m4a")[..])] {
            let d = track(data).duration;
            // HE-AAC counts its encoder delay and padding too.
            assert!(d.is_some_and(|d| (d - 2.0).abs() < 0.25), "{name}: {d:?}");
        }
    }

    #[test]
    fn crossfade_mixes_both_tracks() {
        // The last second of one track over the first second of another.
        let (mut from, mut incoming) = (track(MP3), track(LC));
        from.seek(1.0).unwrap();
        let (mut want_out, mut want_in) = (track(MP3), track(LC));
        want_out.seek(1.0).unwrap();
        let mut f = Crossfade { from, len: 1.0, done: 0.0 };
        let (mut mixed, mut old, mut new, mut scratch): (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) = Default::default();
        let mut more = true;
        while more {
            let mut buf = Vec::new();
            let n = incoming.read(CHUNK, &mut buf).unwrap();
            more = f.mix(&mut buf, &mut scratch);
            mixed.extend(&buf[..n * 2]);
            want_out.read(n, &mut old).unwrap();
            want_in.read(n, &mut new).unwrap();
        }
        // Over after a second (or when the outgoing track ran out).
        assert!(mixed.len() / 2 >= 44100 - CHUNK && mixed.len() / 2 <= 44100 + CHUNK, "{}", mixed.len());
        for (i, ((m, o), n)) in mixed.iter().zip(&old).zip(&new).enumerate() {
            let (go, gi) = crossfade_gains((i / 2) as f64 / 44100.0);
            assert!((m - (o * go + n * gi)).abs() < 1e-5, "frame {}: {m} vs {o}·{go} + {n}·{gi}", i / 2);
        }
        // The start is all the outgoing track, the end all the incoming one.
        assert!((mixed[0] - old[0]).abs() < 1e-6);
    }

    /// Two loud tracks in the middle of a crossfade add up to +3 dB: the limiter keeps the peaks
    /// at full scale without cutting them flat.
    #[test]
    fn loud_crossfade_does_not_clip() {
        let (out, inc) = crossfade_gains(0.5);
        let mut buf: Vec<f32> = (0..RATE as usize)
            .flat_map(|i| {
                let t = i as f32 / RATE as f32;
                let s = (t * 440.0 * std::f32::consts::TAU).sin() * out + (t * 660.0 * std::f32::consts::TAU).sin() * inc;
                [s, s]
            })
            .collect();
        assert!(buf.iter().any(|s| s.abs() > 1.3));
        let mut effects = Effects::new();
        effects.process(&mut buf, |_, _| {});
        let peak = buf.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.0 + 1e-6, "{peak}");
        // Clipping would leave runs of samples at full scale.
        let flat = buf.windows(4).filter(|w| w.iter().all(|s| s.abs() >= 0.9999)).count();
        assert_eq!(flat, 0);
    }

    #[test]
    fn dropped_job_stops_its_download() {
        use std::io::Read;
        let file = TrackFile { url: "http://127.0.0.1:9/".into(), length: None, start: 0.0 };
        let job = Job::new(&file, &crate::fetch::tests::tls());
        let download = job.download.clone();
        drop(job);
        let err = download.reader().read(&mut [0; 16]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionAborted);
    }
}
