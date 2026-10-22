//! Fallback sound output straight to an ALSA device (`/dev/snd/pcmC*D*p`) through the kernel's
//! ioctl interface, for systems without a PulseAudio or PipeWire server; no alsa-lib, so the
//! static binary needs nothing. The device is taken exclusively, as plain `hw:` in ALSA terms:
//! the format and the rate are whatever it supports, converted here.

use super::dsp::Resampler;
use super::output::{Sink, RATE};
use super::ring::{Ring, Wake};
use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// ioctl numbers of <sound/asound.h>: direction << 30 | size << 16 | 'A' << 8 | number.
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    dir << 30 | (size as u64) << 16 | (b'A' as u64) << 8 | nr
}
const WRITE: u64 = 1;
const READ: u64 = 2;
const HW_REFINE: u64 = ioc(READ | WRITE, 0x10, size_of::<HwParams>());
const HW_PARAMS: u64 = ioc(READ | WRITE, 0x11, size_of::<HwParams>());
const DELAY: u64 = ioc(READ, 0x21, size_of::<i64>());
const PREPARE: u64 = ioc(0, 0x40, 0);
const DROP: u64 = ioc(0, 0x43, 0);
const PAUSE: u64 = ioc(WRITE, 0x45, size_of::<i32>());
const WRITEI_FRAMES: u64 = ioc(WRITE, 0x50, size_of::<XferI>());

// Parameters: masks first, then intervals.
const ACCESS: usize = 0;
const FORMAT: usize = 1;
const SUBFORMAT: usize = 2;
const CHANNELS: usize = 10;
const RATE_PARAM: usize = 11;
const PERIOD_TIME: usize = 12;
const PERIOD_SIZE: usize = 13;
const BUFFER_TIME: usize = 16;
const FIRST_INTERVAL: usize = 8;
const ACCESS_RW_INTERLEAVED: u32 = 3;
const INTEGER: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Interval {
    min: u32,
    max: u32,
    /// Bit fields: open min, open max, integer, empty.
    flags: u32,
}

/// `struct snd_pcm_hw_params`.
#[repr(C)]
#[derive(Clone, Copy)]
struct HwParams {
    flags: u32,
    masks: [[u32; 8]; 8],
    intervals: [Interval; 21],
    rmask: u32,
    cmask: u32,
    info: u32,
    msbits: u32,
    rate_num: u32,
    rate_den: u32,
    fifo_size: u64,
    reserved: [u8; 64],
}

/// `struct snd_xferi`.
#[repr(C)]
struct XferI {
    result: i64,
    buf: *const u8,
    frames: u64,
}

// The structures above are those of 64-bit kernels; the targets are x86_64 and aarch64.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the ALSA output is written for 64-bit systems only");
const _: () = assert!(size_of::<HwParams>() == 608 && size_of::<XferI>() == 24);

impl HwParams {
    /// Everything allowed, as alsa-lib's `snd_pcm_hw_params_any`.
    fn any() -> HwParams {
        let mut p = HwParams {
            flags: 0,
            masks: [[0; 8]; 8],
            intervals: [Interval::default(); 21],
            rmask: !0,
            cmask: 0,
            info: !0,
            msbits: 0,
            rate_num: 0,
            rate_den: 0,
            fifo_size: 0,
            reserved: [0; 64],
        };
        for m in &mut p.masks[..3] {
            *m = [!0; 8];
        }
        for i in &mut p.intervals {
            i.max = u32::MAX;
        }
        p
    }

    fn set_mask(&mut self, param: usize, bit: u32) {
        self.masks[param] = [0; 8];
        self.masks[param][bit as usize / 32] |= 1 << (bit % 32);
    }

    fn interval(&mut self, param: usize) -> &mut Interval {
        &mut self.intervals[param - FIRST_INTERVAL]
    }

    fn set_exact(&mut self, param: usize, v: u32) {
        *self.interval(param) = Interval { min: v, max: v, flags: INTEGER };
    }

    fn set_range(&mut self, param: usize, min: u32, max: u32) {
        *self.interval(param) = Interval { min, max, flags: 0 };
    }
}

/// Sample formats tried, best first: (ALSA's number, bytes per sample).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Format {
    F32,
    S32,
    S24In3,
    S16,
}

impl Format {
    const ALL: [Format; 4] = [Format::F32, Format::S32, Format::S24In3, Format::S16];

    fn alsa(self) -> u32 {
        match self {
            Format::F32 => 14,
            Format::S32 => 10,
            Format::S24In3 => 32,
            Format::S16 => 2,
        }
    }

    fn bytes(self) -> usize {
        match self {
            Format::F32 | Format::S32 => 4,
            Format::S24In3 => 3,
            Format::S16 => 2,
        }
    }

    /// Appends samples in -1..1 in this format, little-endian.
    fn encode(self, samples: &[f32], out: &mut Vec<u8>) {
        let int = |s: f32, max: f32| (s.clamp(-1.0, 1.0) * max).round() as i32;
        for &s in samples {
            match self {
                Format::F32 => out.extend(s.to_le_bytes()),
                Format::S32 => out.extend(((f64::from(s.clamp(-1.0, 1.0)) * 2147483647.0).round() as i32).to_le_bytes()),
                Format::S24In3 => out.extend(&int(s, 8388607.0).to_le_bytes()[..3]),
                Format::S16 => out.extend((int(s, 32767.0) as i16).to_le_bytes()),
            }
        }
    }
}

fn ioctl<T>(f: &File, request: u64, arg: *mut T) -> io::Result<i32> {
    // SAFETY: `arg` points to the structure the request expects (or is unused by it), alive for
    // the call. The request type differs between libcs, hence the cast.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), request as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn ioctl_none(f: &File, request: u64) -> io::Result<i32> {
    ioctl(f, request, std::ptr::null_mut::<u8>())
}

/// What the device was set up with.
#[derive(Debug)]
struct Setup {
    format: Format,
    rate: u32,
    period: usize,
}

/// Picks a format and a rate (ours if it can, else 48 kHz or the nearest one), with a buffer of
/// about 200 ms in periods of 20–50 ms.
fn configure(dev: &File) -> Result<Setup> {
    for format in Format::ALL {
        let mut p = HwParams::any();
        p.set_mask(ACCESS, ACCESS_RW_INTERLEAVED);
        p.set_mask(FORMAT, format.alsa());
        p.set_mask(SUBFORMAT, 0);
        p.set_exact(CHANNELS, 2);
        if ioctl(dev, HW_REFINE, &mut p).is_err() {
            continue;
        }
        let rates = *p.interval(RATE_PARAM);
        let rate = [RATE, 48000].into_iter().find(|r| (rates.min..=rates.max).contains(r)).unwrap_or(rates.min.max(8000));
        p.set_exact(RATE_PARAM, rate);
        let mut timed = p;
        timed.set_range(PERIOD_TIME, 20_000, 50_000);
        timed.set_range(BUFFER_TIME, 100_000, 250_000);
        // Some devices cannot meet the times: then whatever the kernel picks.
        for mut q in [timed, p] {
            q.rmask = !0;
            if ioctl(dev, HW_PARAMS, &mut q).is_ok() {
                let period = q.interval(PERIOD_SIZE).min as usize;
                return Ok(Setup { format, rate: q.interval(RATE_PARAM).min, period: period.max(64) });
            }
        }
    }
    Err(anyhow!("устройство не поддерживает ни один из форматов"))
}

/// Playback devices in the order they are tried: `device` (`hw:1,0` or `1,0`) or all of them,
/// card by card.
fn devices(device: Option<&str>) -> Vec<String> {
    if let Some(d) = device {
        let d = d.trim_start_matches("hw:").replace(',', "D");
        return vec![format!("/dev/snd/pcmC{d}p")];
    }
    let mut found: Vec<(u32, u32, String)> = std::fs::read_dir("/dev/snd")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (card, dev) = name.strip_prefix("pcmC")?.strip_suffix('p')?.split_once('D')?;
            Some((card.parse().ok()?, dev.parse().ok()?, e.path().to_string_lossy().into_owned()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(.., p)| p).collect()
}

enum Request {
    Pause(bool),
    Flush,
    /// Frames still in the device (at our rate).
    Delay,
}

pub struct Alsa {
    requests: Option<Sender<(Request, Sender<io::Result<u64>>)>>,
    wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
}

impl Alsa {
    pub fn open(ring: Arc<Ring>, device: Option<&str>) -> Result<Alsa> {
        let mut errors = Vec::new();
        for path in devices(device) {
            // Opened non-blocking so that a busy device fails at once; writes block later.
            let dev = match OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path) {
                Ok(d) => d,
                Err(e) => {
                    errors.push(format!("{path}: {e}"));
                    continue;
                }
            };
            match configure(&dev) {
                Ok(setup) => {
                    // SAFETY: plain fcntl on our own descriptor.
                    unsafe { libc::fcntl(dev.as_raw_fd(), libc::F_SETFL, 0) };
                    ioctl_none(&dev, PREPARE).context("устройство не готово")?;
                    let wake = Arc::new(Wake::new()?);
                    ring.set_wake(wake.clone());
                    let (tx, rx) = mpsc::channel();
                    let w = wake.clone();
                    let thread = std::thread::Builder::new()
                        .name("player-output".into())
                        .spawn(move || serve(&dev, &setup, &ring, &rx, &w))?;
                    return Ok(Alsa { requests: Some(tx), wake, thread: Some(thread) });
                }
                Err(e) => errors.push(format!("{path}: {e:#}")),
            }
        }
        Err(anyhow!("нет подходящего устройства ALSA ({})", errors.join("; ")))
    }

    fn call(&self, req: Request) -> Result<u64> {
        let lost = || anyhow!("звуковое устройство не отвечает");
        let (tx, rx) = mpsc::channel();
        self.requests.as_ref().ok_or_else(lost)?.send((req, tx)).map_err(|_| lost())?;
        self.wake.signal();
        Ok(rx.recv_timeout(Duration::from_secs(3)).map_err(|_| lost())??)
    }
}

impl Sink for Alsa {
    fn set_paused(&self, paused: bool) -> Result<()> {
        self.call(Request::Pause(paused)).map(drop)
    }

    fn flush(&self) -> Result<()> {
        self.call(Request::Flush).map(drop)
    }

    /// Frames still to be heard (in the device's buffer), at our rate.
    fn delay(&self) -> Result<u64> {
        self.call(Request::Delay)
    }
}

impl Drop for Alsa {
    fn drop(&mut self) {
        self.requests = None;
        self.wake.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Writes all of `data` (frames of `bytes` each), recovering from underruns.
fn write_all(dev: &File, mut data: &[u8], frame_bytes: usize) -> io::Result<()> {
    while !data.is_empty() {
        let mut x = XferI { result: 0, buf: data.as_ptr(), frames: (data.len() / frame_bytes) as u64 };
        match ioctl(dev, WRITEI_FRAMES, &mut x) {
            Ok(_) => data = &data[x.result.max(0) as usize * frame_bytes..],
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) || e.raw_os_error() == Some(libc::ESTRPIPE) => {
                ioctl_none(dev, PREPARE)?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn wait(wake: &Wake) {
    let mut fd = libc::pollfd { fd: wake.fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut fd, 1, -1) };
    wake.reset();
}

fn serve(dev: &File, setup: &Setup, ring: &Ring, requests: &Receiver<(Request, Sender<io::Result<u64>>)>, wake: &Wake) {
    let frame_bytes = setup.format.bytes() * 2;
    let mut resampler = (setup.rate != RATE).then(|| Resampler::to(RATE, setup.rate));
    let (mut samples, mut resampled, mut out) = (Vec::new(), Vec::new(), Vec::new());
    let mut paused = false;
    // Pausing in hardware; without it the buffer is dropped and the stream restarted.
    let mut hw_pause = true;
    let to_ours = |frames: u64| frames * u64::from(RATE) / u64::from(setup.rate);
    loop {
        loop {
            let (req, reply) = match requests.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ioctl_none(dev, DROP);
                    return;
                }
            };
            let result = match req {
                Request::Pause(p) if p != paused => {
                    paused = p;
                    let mut arg = i32::from(p);
                    if hw_pause && ioctl(dev, PAUSE, &mut arg).is_ok() {
                        Ok(0)
                    } else {
                        hw_pause = false;
                        if p { ioctl_none(dev, DROP) } else { ioctl_none(dev, PREPARE) }.map(|_| 0)
                    }
                }
                Request::Pause(_) => Ok(0),
                Request::Flush => {
                    if let Some(r) = &mut resampler {
                        r.reset();
                    }
                    ioctl_none(dev, DROP).and_then(|_| ioctl_none(dev, PREPARE)).map(|_| 0)
                }
                Request::Delay => {
                    let mut d: i64 = 0;
                    // A stopped stream (underrun, not started) has nothing left to play.
                    Ok(ioctl(dev, DELAY, &mut d).map_or(0, |_| to_ours(d.max(0) as u64)))
                }
            };
            let _ = reply.send(result);
        }
        if paused {
            wait(wake);
            continue;
        }
        samples.clear();
        if ring.take_samples(setup.period, &mut samples) == 0 {
            wait(wake);
            continue;
        }
        let data = match &mut resampler {
            Some(r) => {
                resampled.clear();
                r.process(&samples, &mut resampled);
                &resampled
            }
            None => &samples,
        };
        out.clear();
        setup.format.encode(data, &mut out);
        if write_all(dev, &out, frame_bytes).is_err() {
            // The device is gone: the next request finds the channel closed.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Format;

    #[test]
    fn sample_formats() {
        let s = [0.0, 1.0, -1.0, 0.5, 2.0];
        let mut out = Vec::new();
        Format::S16.encode(&s, &mut out);
        let v: Vec<i16> = out.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        assert_eq!(v, [0, 32767, -32767, 16384, 32767]);
        out.clear();
        Format::S24In3.encode(&s[..4], &mut out);
        assert_eq!(out, [0, 0, 0, 0xFF, 0xFF, 0x7F, 0x01, 0x00, 0x80, 0x00, 0x00, 0x40]);
        out.clear();
        Format::S32.encode(&[1.0, -0.5], &mut out);
        assert_eq!(out, [0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x00, 0xC0]);
        out.clear();
        Format::F32.encode(&[0.25], &mut out);
        assert_eq!(out, 0.25f32.to_le_bytes());
    }
}
