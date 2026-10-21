//! Sound output through the PulseAudio protocol (PulseAudio itself or PipeWire's pulse server),
//! fed from a ring buffer that the decoding thread fills.

use anyhow::{anyhow, Context, Result};
use futures::executor::block_on;
use pulseaudio::{protocol, Client, PlaybackSource, PlaybackStream};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Poll, Waker};

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
    waker: Option<Waker>,
    /// Frames ever pushed and handed to the server (dropped ones count as handed).
    pushed: u64,
    handed: u64,
    /// Bumped by `clear`: a producer still pushing older audio is told to stop.
    gen: u64,
    marks: VecDeque<(u64, Mark)>,
    /// The job being decoded and a seek requested in it.
    producing: u64,
    seek: Option<f64>,
}

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
            if let Some(w) = s.waker.take() {
                w.wake();
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

/// The server pulls data from the ring. When the ring is empty the stream waits instead of
/// getting 0 bytes, which would end it for good.
struct Source(Arc<Ring>);

impl PlaybackSource for Source {
    fn poll_read(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut [u8]) -> Poll<usize> {
        let mut s = self.0.state.lock().unwrap();
        let frames = (buf.len() / FRAME).min(s.samples.len() / 2);
        if frames == 0 {
            s.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        for (out, sample) in buf.chunks_exact_mut(4).zip(s.samples.drain(..frames * 2)) {
            out.copy_from_slice(&sample.to_le_bytes());
        }
        s.handed += frames as u64;
        self.0.space.notify_all();
        Poll::Ready(frames * FRAME)
    }
}

pub struct Output {
    // The connection lives as long as the stream.
    _client: Client,
    stream: PlaybackStream,
    ring: Arc<Ring>,
}

impl Output {
    pub fn open(ring: Arc<Ring>) -> Result<Output> {
        let client = Client::from_env(c"ymusic").context("нет связи со звуковым сервером (PipeWire или PulseAudio)")?;
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
        let stream = block_on(client.create_playback_stream(params, Source(ring.clone())))
            .context("звуковой сервер не открыл поток")?;
        Ok(Output { _client: client, stream, ring })
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        if paused {
            block_on(self.stream.cork())?;
        } else {
            block_on(self.stream.uncork())?;
        }
        Ok(())
    }

    /// Drops what the server has buffered (after the ring was cleared).
    pub fn flush(&self) -> Result<()> {
        block_on(self.stream.flush()).map_err(|e| anyhow!("{e}"))
    }

    /// Frames heard so far: handed to the server minus what it still holds and plays.
    pub fn played(&self) -> Result<u64> {
        let t = block_on(self.stream.timing_info())?;
        let buffered = (t.write_offset - t.read_offset).max(0) as u64 / FRAME as u64;
        let latency = if buffered > 0 { t.sink_usec * u64::from(RATE) / 1_000_000 } else { 0 };
        Ok(self.ring.handed().saturating_sub(buffered + latency))
    }

    pub fn close(self) {
        let _ = block_on(self.stream.delete());
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
