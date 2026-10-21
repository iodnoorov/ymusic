//! Sound output through the PulseAudio protocol (PulseAudio itself or PipeWire's pulse server),
//! fed from a ring buffer that the decoding thread fills. The client is our own: one thread owns
//! the connection and sleeps in `poll` until the server asks for data, the ring gets some it was
//! waiting for, or a command comes; the `pulseaudio` crate only encodes and decodes the messages.

use anyhow::{anyhow, Context, Result};
use pulseaudio::protocol::{self, Command, TagStructRead, TagStructReader};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Cursor, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// Output sample rate; every track the server sends has it, others are resampled.
pub const RATE: u32 = 44100;
/// Bytes per frame: stereo `f32`.
const FRAME: usize = 8;
/// The ring holds 100 ms, the server about as much again: a change of volume is heard that soon.
const RING_SAMPLES: usize = RATE as usize / 10 * 2;
const SERVER_BUFFER_MS: u32 = 200;

/// Where something happens in the stream of frames: a track starts, ends or fails.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// A track (`job`) starts at `at` seconds into it; `gapless` — right after the previous one.
    Start { job: u64, at: f64, duration: Option<f64>, gapless: bool },
    End,
    Error(String),
}

#[derive(Default)]
struct RingState {
    samples: VecDeque<f32>,
    /// The output thread found the ring empty and waits to be woken when it is not.
    starving: bool,
    wake: Option<Arc<Wake>>,
    /// Frames ever pushed and handed to the server (dropped ones count as handed).
    pushed: u64,
    handed: u64,
    /// Bumped by `clear`: a producer still pushing older audio is told to stop.
    gen: u64,
    marks: VecDeque<(u64, Mark)>,
    /// The job being decoded and a seek requested in it.
    producing: u64,
    seek: Option<f64>,
    /// Spectrum measurements by the frame they end at.
    spectra: VecDeque<(u64, Vec<f32>)>,
}

/// Measurements kept for the visualizer: about the sound in the ring and on the server.
const MAX_SPECTRA: usize = 32;

/// Interleaved stereo frames on their way to the server, with marks at track boundaries.
#[derive(Default)]
pub struct Ring {
    state: Mutex<RingState>,
    space: Condvar,
}

impl Ring {
    /// Appends frames, waiting while the ring is full. False if it was cleared since `gen`.
    pub fn push(&self, mut data: &[f32], gen: u64) -> bool {
        let mut s = self.state.lock().unwrap();
        while !data.is_empty() {
            if s.gen != gen {
                return false;
            }
            let room = (RING_SAMPLES - s.samples.len().min(RING_SAMPLES)).min(data.len()) & !1;
            if room == 0 {
                s = self.space.wait(s).unwrap();
                continue;
            }
            s.samples.extend(&data[..room]);
            s.pushed += room as u64 / 2;
            data = &data[room..];
            if s.starving {
                s.starving = false;
                if let Some(w) = &s.wake {
                    w.signal();
                }
            }
        }
        true
    }

    /// Puts a mark at the current end of the stream; returns the generation it belongs to.
    pub fn mark(&self, mark: Mark) -> u64 {
        let mut s = self.state.lock().unwrap();
        let at = s.pushed;
        s.marks.push_back((at, mark));
        s.gen
    }

    /// Drops the frames not yet handed to the server and all pending marks.
    pub fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        s.samples.clear();
        s.handed = s.pushed;
        s.marks.clear();
        s.spectra.clear();
        s.seek = None;
        s.gen += 1;
        self.space.notify_all();
    }

    /// Clears the ring and asks the producer to seek, if it is decoding `job`.
    pub fn request_seek(&self, job: u64, secs: f64) -> bool {
        if self.state.lock().unwrap().producing != job {
            return false;
        }
        self.clear();
        let mut s = self.state.lock().unwrap();
        s.seek = Some(secs);
        true
    }

    /// For the producer: starts decoding `job`; returns the current generation.
    pub fn begin(&self, job: u64) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.producing = job;
        s.seek = None;
        s.gen
    }

    /// For the producer: the current generation and a seek requested since the last call.
    pub fn poll_seek(&self) -> (u64, Option<f64>) {
        let mut s = self.state.lock().unwrap();
        let seek = s.seek.take();
        (s.gen, seek)
    }

    /// Stores band levels measured over the sound about to be pushed, ending `offset` frames into
    /// it; dropped if the ring was cleared since `gen`.
    pub fn push_spectrum(&self, gen: u64, offset: usize, levels: Vec<f32>) {
        let mut s = self.state.lock().unwrap();
        if s.gen != gen {
            return;
        }
        let at = s.pushed + offset as u64;
        if s.spectra.len() == MAX_SPECTRA {
            s.spectra.pop_front();
        }
        s.spectra.push_back((at, levels));
    }

    /// The latest band levels playback has reached by frame `played`.
    pub fn spectrum(&self, played: u64) -> Option<Vec<f32>> {
        let mut s = self.state.lock().unwrap();
        let mut last = None;
        while s.spectra.front().is_some_and(|(at, _)| *at <= played) {
            last = s.spectra.pop_front().map(|(_, l)| l);
        }
        last
    }

    /// For the output thread: moves up to `max` bytes of frames to `out` as `f32` little-endian;
    /// returns the bytes moved. When there are none, the thread is woken once some are pushed.
    fn take(&self, max: usize, out: &mut Vec<u8>) -> usize {
        let mut s = self.state.lock().unwrap();
        let frames = (max / FRAME).min(s.samples.len() / 2);
        if frames == 0 {
            s.starving = true;
            return 0;
        }
        out.extend(s.samples.drain(..frames * 2).flat_map(f32::to_le_bytes));
        s.handed += frames as u64;
        self.space.notify_all();
        frames * FRAME
    }

    fn set_wake(&self, wake: Arc<Wake>) {
        let mut s = self.state.lock().unwrap();
        s.wake = Some(wake);
        s.starving = false;
    }

    /// Frames handed to the server so far.
    fn handed(&self) -> u64 {
        self.state.lock().unwrap().handed
    }

    /// Takes the marks that playback has reached by frame `played`, with the frames they are at.
    pub fn reached(&self, played: u64) -> Vec<(u64, Mark)> {
        let mut s = self.state.lock().unwrap();
        let mut out = Vec::new();
        while s.marks.front().is_some_and(|(at, _)| *at <= played) {
            out.extend(s.marks.pop_front());
        }
        out
    }
}

/// An eventfd that wakes the output thread from its `poll`.
struct Wake(OwnedFd);

impl Wake {
    fn new() -> io::Result<Wake> {
        // SAFETY: plain syscall; the descriptor is owned from here on.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor nobody else owns.
        Ok(Wake(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn signal(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a live buffer to our own eventfd.
        unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    }

    fn reset(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: reads at most 8 bytes into a live buffer from our own eventfd.
        unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    }
}

/// The connection to the server: messages are read into `rbuf` and taken from there whole.
struct Conn {
    sock: UnixStream,
    rbuf: Vec<u8>,
    version: u16,
    /// Bytes of stream data the server has asked for and not got yet.
    requested: usize,
}

/// A message from the server: what it is and, for a reply, the rest of it.
struct Message {
    seq: u32,
    command: Command,
    payload: Cursor<Vec<u8>>,
}

impl Conn {
    /// Reads what has arrived (blocking or not, as the socket is); false when the server hung up.
    fn receive(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 16 * 1024];
        match self.sock.read(&mut buf) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.rbuf.extend_from_slice(&buf[..n]);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// The next whole message already received. Requests for data are counted here.
    fn message(&mut self) -> Result<Option<Message>> {
        loop {
            if self.rbuf.len() < protocol::DESCRIPTOR_SIZE {
                return Ok(None);
            }
            let desc = protocol::read_descriptor(&mut Cursor::new(&self.rbuf[..protocol::DESCRIPTOR_SIZE]))?;
            let len = protocol::DESCRIPTOR_SIZE + desc.length as usize;
            if self.rbuf.len() < len {
                return Ok(None);
            }
            let body: Vec<u8> = self.rbuf.drain(..len).skip(protocol::DESCRIPTOR_SIZE).collect();
            // Only control messages come to a client that does not record.
            if desc.channel != u32::MAX {
                continue;
            }
            let mut payload = Cursor::new(body);
            // A notification the crate does not know is of no interest here.
            let Ok((seq, command)) = Command::read_tag_prefixed(&mut payload, self.version) else { continue };
            if let Command::Request(r) = &command {
                self.requested += r.length as usize;
            }
            return Ok(Some(Message { seq, command, payload }));
        }
    }

    /// Sends a command and waits for its reply (while connecting, before the socket is
    /// non-blocking).
    fn call<T: TagStructRead>(&mut self, seq: u32, command: &Command) -> Result<T> {
        protocol::write_command_message(&mut self.sock, seq, command, self.version)?;
        loop {
            while let Some(mut m) = self.message()? {
                match m.command {
                    Command::Reply if m.seq == seq => return Ok(T::read(&mut TagStructReader::new(&mut m.payload, self.version), self.version)?),
                    Command::Error(e) if m.seq == seq => return Err(anyhow!("звуковой сервер ответил ошибкой: {e:?}")),
                    _ => {}
                }
            }
            if !self.receive()? {
                return Err(anyhow!("звуковой сервер закрыл соединение"));
            }
        }
    }
}

/// A command for the output thread and where its reply goes (the latency for a timing query).
type Request = (Command, Sender<Result<Option<protocol::PlaybackLatency>, String>>);

pub struct Output {
    channel: u32,
    requests: Option<Sender<Request>>,
    wake: Arc<Wake>,
    ring: Arc<Ring>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    pub fn open(ring: Arc<Ring>) -> Result<Output> {
        let none = || anyhow!("нет звукового сервера (PipeWire или PulseAudio)");
        let path = pulseaudio::socket_path_from_env().ok_or_else(none)?;
        let sock = UnixStream::connect(&path).with_context(|| format!("нет связи со звуковым сервером ({})", path.display()))?;
        let mut conn = Conn { sock, rbuf: Vec::new(), version: protocol::MAX_VERSION, requested: 0 };
        let cookie = pulseaudio::cookie_path_from_env().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        let auth = protocol::AuthParams { version: protocol::MAX_VERSION, supports_shm: false, supports_memfd: false, cookie };
        let reply: protocol::AuthReply = conn.call(0, &Command::Auth(auth))?;
        conn.version = conn.version.min(reply.version);
        let mut props = protocol::Props::new();
        props.set(protocol::Prop::ApplicationName, c"ymusic");
        let _: protocol::SetClientNameReply = conn.call(1, &Command::SetClientName(props))?;

        let buffer = RATE * SERVER_BUFFER_MS / 1000 * FRAME as u32;
        let mut props = protocol::Props::new();
        props.set(protocol::Prop::MediaName, c"ymusic");
        props.set(protocol::Prop::MediaRole, c"music");
        let params = protocol::PlaybackStreamParams {
            sample_spec: protocol::SampleSpec { format: protocol::SampleFormat::Float32Le, channels: 2, sample_rate: RATE },
            channel_map: protocol::ChannelMap::stereo(),
            cvolume: Some(protocol::ChannelVolume::norm(2)),
            // PULSE_SINK picks the device, as for any PulseAudio client.
            sink_name: Some(
                std::env::var("PULSE_SINK")
                    .ok()
                    .and_then(|s| std::ffi::CString::new(s).ok())
                    .unwrap_or_else(|| protocol::DEFAULT_SINK.to_owned()),
            ),
            buffer_attr: protocol::stream::BufferAttr {
                max_length: u32::MAX,
                target_length: buffer,
                pre_buffering: buffer / 2,
                minimum_request_length: buffer / 4,
                fragment_size: u32::MAX,
            },
            flags: protocol::stream::StreamFlags { adjust_latency: true, ..Default::default() },
            props,
            ..Default::default()
        };
        let stream: protocol::CreatePlaybackStreamReply =
            conn.call(2, &Command::CreatePlaybackStream(params)).context("звуковой сервер не открыл поток")?;
        conn.requested += stream.requested_bytes as usize;
        conn.sock.set_nonblocking(true)?;

        let wake = Arc::new(Wake::new()?);
        ring.set_wake(wake.clone());
        let (tx, rx) = mpsc::channel();
        let (r, w) = (ring.clone(), wake.clone());
        let thread = std::thread::Builder::new()
            .name("player-output".into())
            .spawn(move || serve(conn, stream.channel, &r, &rx, &w))?;
        Ok(Output { channel: stream.channel, requests: Some(tx), wake, ring, thread: Some(thread) })
    }

    fn call(&self, command: Command) -> Result<Option<protocol::PlaybackLatency>> {
        let lost = || anyhow!("нет связи со звуковым сервером");
        let (tx, rx) = mpsc::channel();
        self.requests.as_ref().ok_or_else(lost)?.send((command, tx)).map_err(|_| lost())?;
        self.wake.signal();
        rx.recv_timeout(Duration::from_secs(3)).map_err(|_| lost())?.map_err(|e| anyhow!("звуковой сервер: {e}"))
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.call(Command::CorkPlaybackStream(protocol::CorkStreamParams { channel: self.channel, cork: paused }))?;
        Ok(())
    }

    /// Drops what the server has buffered (after the ring was cleared).
    pub fn flush(&self) -> Result<()> {
        self.call(Command::FlushPlaybackStream(self.channel))?;
        Ok(())
    }

    /// Frames heard so far: handed to the server minus what it still holds and plays.
    pub fn played(&self) -> Result<u64> {
        let t = self
            .call(Command::GetPlaybackLatency(protocol::LatencyParams { channel: self.channel, now: SystemTime::now() }))?
            .ok_or_else(|| anyhow!("звуковой сервер не сообщил задержку"))?;
        let buffered = (t.write_offset - t.read_offset).max(0) as u64 / FRAME as u64;
        let latency = if buffered > 0 { t.sink_usec * u64::from(RATE) / 1_000_000 } else { 0 };
        Ok(self.ring.handed().saturating_sub(buffered + latency))
    }

    pub fn close(self) {
        let _ = self.call(Command::DeletePlaybackStream(self.channel));
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // The thread stops once its channel is closed.
        self.requests = None;
        self.wake.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The output thread: sends the ring's frames as the server asks for them and the commands of
/// `Output`, routes the replies. Ends when `Output` is gone or the connection breaks.
fn serve(mut conn: Conn, channel: u32, ring: &Ring, requests: &Receiver<Request>, wake: &Wake) {
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = Vec::new();
    let mut pending: HashMap<u32, Sender<Result<Option<protocol::PlaybackLatency>, String>>> = HashMap::new();
    let mut seq = 100u32;
    let result = (|| -> Result<()> {
        loop {
            loop {
                match requests.try_recv() {
                    Ok((command, reply)) => {
                        protocol::write_command_message(&mut out, seq, &command, conn.version)?;
                        pending.insert(seq, reply);
                        seq = seq.wrapping_add(1).max(100);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            while conn.requested >= FRAME && out.len() < 256 * 1024 {
                chunk.clear();
                let n = ring.take(conn.requested.min(64 * 1024), &mut chunk);
                if n == 0 {
                    break;
                }
                protocol::write_memblock(&mut out, channel, &chunk, 0)?;
                conn.requested -= n;
            }
            while !out.is_empty() {
                match conn.sock.write(&out) {
                    Ok(n) => drop(out.drain(..n)),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }

            let sock_events = libc::POLLIN | if out.is_empty() { 0 } else { libc::POLLOUT };
            let mut fds = [
                libc::pollfd { fd: conn.sock.as_raw_fd(), events: sock_events, revents: 0 },
                libc::pollfd { fd: wake.0.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: two valid pollfd entries of a live array.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if fds[1].revents != 0 {
                wake.reset();
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                if !conn.receive()? {
                    return Err(anyhow!("звуковой сервер закрыл соединение"));
                }
                while let Some(mut m) = conn.message()? {
                    match m.command {
                        Command::Reply => {
                            if let Some(reply) = pending.remove(&m.seq) {
                                // Only the timing query has a reply worth reading.
                                let latency = protocol::PlaybackLatency::read(&mut TagStructReader::new(&mut m.payload, conn.version), conn.version).ok();
                                let _ = reply.send(Ok(latency));
                            }
                        }
                        Command::Error(e) => {
                            if let Some(reply) = pending.remove(&m.seq) {
                                let _ = reply.send(Err(format!("{e:?}")));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    })();
    if let Err(e) = result {
        for (_, reply) in pending.drain() {
            let _ = reply.send(Err(format!("{e:#}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mark, Ring, RING_SAMPLES};
    use std::sync::Arc;

    #[test]
    fn marks_and_clearing() {
        let ring = Arc::new(Ring::default());
        let gen = ring.begin(1);
        assert_eq!(ring.mark(Mark::Start { job: 1, at: 0.0, duration: None, gapless: false }), gen);
        assert!(ring.push(&[0.5; 100], gen));
        ring.mark(Mark::End);
        assert_eq!(ring.reached(0).len(), 1);
        assert!(ring.reached(49).is_empty());
        assert_eq!(ring.reached(50), vec![(50, Mark::End)]);

        // A full ring blocks the producer until it is cleared; then its push reports that.
        let r = ring.clone();
        let producer = std::thread::spawn(move || r.push(&vec![0.0; RING_SAMPLES * 2], gen));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!producer.is_finished());
        assert!(!ring.request_seek(2, 10.0), "not the job being decoded");
        assert!(ring.request_seek(1, 10.0));
        assert!(!producer.join().unwrap());
        let (new_gen, seek) = ring.poll_seek();
        assert_eq!((new_gen, seek), (gen + 1, Some(10.0)));
        assert_eq!(ring.poll_seek().1, None);
        // Everything pushed counts as handed: playback position continues from there.
        assert_eq!(ring.handed(), ring.state.lock().unwrap().pushed);
    }
}
