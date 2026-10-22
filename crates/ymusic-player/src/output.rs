//! Sound output through the PulseAudio protocol (PulseAudio itself or PipeWire's pulse server),
//! fed from a ring buffer that the decoding thread fills. The client is our own: one thread owns
//! the connection and sleeps in `poll` until the server asks for data, the ring gets some it was
//! waiting for, or a command comes; the `pulseaudio` crate only encodes and decodes the messages.

use crate::Lock;
use anyhow::{anyhow, Context, Result};
use pulseaudio::protocol::{self, Command, TagStructRead, TagStructReader};
use super::ring::{Ring, Wake};
use super::Options;
use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// Output sample rate; every track the server sends has it, others are resampled.
pub const RATE: u32 = 44100;
/// Bytes per frame: stereo `f32`.
const FRAME: usize = 8;
/// The server's buffer: the sound ahead of what is heard, besides the ring's 100 ms.
const SERVER_BUFFER_MS: u32 = 200;
/// How long the server's answer about its delay is trusted: in between, playback is assumed to
/// go on at our rate.
const DELAY_REFRESH: Duration = Duration::from_secs(1);

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

/// Output to a PulseAudio (or PipeWire) server.
pub struct Pulse {
    channel: u32,
    requests: Option<Sender<Request>>,
    wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
    ring: Arc<Ring>,
    clock: Mutex<Clock>,
}

/// What the server last said about playback, to tell the position in between without asking it
/// (a round trip to the server each time the position is wanted, several times a second).
#[derive(Default)]
struct Clock {
    /// Frames heard by then, and when it was asked.
    heard: Option<(u64, Instant)>,
    paused: bool,
}

impl Pulse {
    fn open(ring: Arc<Ring>) -> Result<Pulse> {
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
        Ok(Pulse { channel: stream.channel, requests: Some(tx), wake, thread: Some(thread), ring, clock: Mutex::default() })
    }

    fn call(&self, command: Command) -> Result<Option<protocol::PlaybackLatency>> {
        let lost = || anyhow!("нет связи со звуковым сервером");
        let (tx, rx) = mpsc::channel();
        self.requests.as_ref().ok_or_else(lost)?.send((command, tx)).map_err(|_| lost())?;
        self.wake.signal();
        rx.recv_timeout(Duration::from_secs(3)).map_err(|_| lost())?.map_err(|e| anyhow!("звуковой сервер: {e}"))
    }

    /// Frames handed to the server that are not heard yet, as the server tells.
    fn server_delay(&self) -> Result<u64> {
        let t = self
            .call(Command::GetPlaybackLatency(protocol::LatencyParams { channel: self.channel, now: SystemTime::now() }))?
            .ok_or_else(|| anyhow!("звуковой сервер не сообщил задержку"))?;
        let buffered = (t.write_offset - t.read_offset).max(0) as u64 / FRAME as u64;
        let latency = if buffered > 0 { t.sink_usec * u64::from(RATE) / 1_000_000 } else { 0 };
        Ok(buffered + latency)
    }
}

impl Sink for Pulse {
    fn set_paused(&self, paused: bool) -> Result<()> {
        self.call(Command::CorkPlaybackStream(protocol::CorkStreamParams { channel: self.channel, cork: paused }))?;
        let mut clock = self.clock.locked();
        *clock = Clock { heard: None, paused };
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        self.call(Command::FlushPlaybackStream(self.channel))?;
        self.clock.locked().heard = None;
        Ok(())
    }

    /// Between the server's answers playback goes on at our rate (unless paused), though never past
    /// what the server got: it stops there when the sound runs out.
    fn delay(&self) -> Result<u64> {
        let handed = self.ring.handed();
        let mut clock = self.clock.locked();
        let heard = match clock.heard {
            Some((heard, at)) if at.elapsed() < DELAY_REFRESH => {
                let gone = if clock.paused { 0 } else { (at.elapsed().as_secs_f64() * f64::from(RATE)) as u64 };
                (heard + gone).min(handed)
            }
            _ => {
                let heard = handed.saturating_sub(self.server_delay()?);
                clock.heard = Some((heard, Instant::now()));
                heard
            }
        };
        Ok(handed - heard)
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        let _ = self.call(Command::DeletePlaybackStream(self.channel));
        // The thread stops once its channel is closed.
        self.requests = None;
        self.wake.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A sound output: takes the frames from the ring at its own pace.
pub trait Sink: Send {
    fn set_paused(&self, paused: bool) -> Result<()>;
    /// Drops the sound on its way (after the ring was cleared).
    fn flush(&self) -> Result<()>;
    /// Frames handed on that are not heard yet.
    fn delay(&self) -> Result<u64>;
}

/// Where the sound goes. On Linux a PulseAudio (PipeWire) server, or an ALSA device when there is
/// none; `YMUSIC_OUTPUT=pulse` or `alsa` forces one of them. On macOS Core Audio, or a PulseAudio
/// server with `YMUSIC_OUTPUT=pulse`. Closed when dropped.
pub struct Output(Box<dyn Sink>);

impl Output {
    pub fn open(ring: Arc<Ring>, options: &Options) -> Result<Output> {
        let pulse = |ring| Pulse::open(ring).map(|p| Output(Box::new(p)));
        #[cfg(target_os = "linux")]
        {
            let alsa = |ring| super::alsa::Alsa::open(ring, options.alsa_device.as_deref()).map(|a| Output(Box::new(a)));
            match options.output.as_deref() {
                Some("alsa") => alsa(ring),
                Some("pulse") => pulse(ring),
                _ => pulse(ring.clone()).or_else(|e| alsa(ring).map_err(|e2| anyhow!("{e:#}; {e2:#}"))),
            }
        }
        #[cfg(target_os = "macos")]
        match options.output.as_deref() {
            Some("pulse") => pulse(ring),
            _ => super::coreaudio::CoreAudio::open(ring).map(|c| Output(Box::new(c))),
        }
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.0.set_paused(paused)
    }

    pub fn flush(&self) -> Result<()> {
        self.0.flush()
    }

    /// Frames heard so far: handed on minus what is still buffered on the way to the speakers.
    pub fn played(&self, ring: &Ring) -> Result<u64> {
        Ok(ring.handed().saturating_sub(self.0.delay()?))
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
                let n = ring.take_bytes(conn.requested.min(64 * 1024), &mut chunk);
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
                libc::pollfd { fd: wake.fd(), events: libc::POLLIN, revents: 0 },
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
