//! Demuxing (MP4, MP3) and decoding with symphonia. AAC-LC and MP3 use symphonia's own decoders;
//! AAC with SBR (HE-AAC: the server sends it for «Экономия трафика») goes through fdk-aac, since
//! symphonia's decoder would silently play only its 22 kHz core.

use anyhow::{anyhow, Context, Result};
use std::sync::OnceLock;
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

/// Sampling frequencies by the 4-bit index of an AudioSpecificConfig.
const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// So many undecodable packets in a row mean the stream is broken, not just a glitch.
const MAX_BAD_PACKETS: u32 = 50;

/// Big-endian bit reader over an AudioSpecificConfig.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = v << 1 | u32::from(byte >> (7 - self.pos % 8) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    fn object_type(&mut self) -> Option<u32> {
        match self.read(5)? {
            31 => Some(32 + self.read(6)?),
            t => Some(t),
        }
    }

    fn rate(&mut self) -> Option<u32> {
        match self.read(4)? {
            15 => self.read(24),
            i => AAC_RATES.get(i as usize).copied(),
        }
    }
}

/// Whether an AAC stream needs fdk-aac: anything but plain AAC-LC with a full-rate core. SBR is
/// signalled explicitly (object type 5 or 29), backward-compatibly (an LC config followed by sync
/// extension 0x2B7 with the SBR flag set — as Yandex sends it) or not at all (implicitly: only the
/// halved core rate gives it away).
fn needs_fdk(asc: &[u8]) -> bool {
    let mut b = Bits { data: asc, pos: 0 };
    let plain_lc = (|| {
        if b.object_type()? != 2 || b.rate()? < 32000 {
            return Some(false);
        }
        // Channel configuration 0 comes with a program config element; fdk reads it.
        if b.read(4)? == 0 {
            return Some(false);
        }
        // GASpecificConfig: frame length flag, depends on core coder (+ its delay), extension flag.
        b.read(1)?;
        if b.read(1)? == 1 {
            b.read(14)?;
        }
        b.read(1)?;
        if b.left() >= 16 && b.read(11)? == 0x2B7 && b.object_type()? == 5 {
            return Some(b.read(1)? == 0);
        }
        Some(true)
    })();
    plain_lc != Some(true)
}

fn fdk_codecs() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut r = CodecRegistry::new();
        r.register_audio_decoder::<symphonia_adapter_fdk_aac::AacDecoder>();
        r
    })
}

/// Latency of fdk-aac for this stream, frames: it starts the output (and every restart after a
/// seek) with that much of its own delay — 1685 frames for AAC-LC, 3671 for HE-AAC. The adapter
/// does not tell it, so one packet goes through a decoder of our own.
fn fdk_delay(asc: &[u8], packet: &[u8]) -> u64 {
    let mut d = fdk_aac::dec::Decoder::new(fdk_aac::dec::Transport::Raw);
    // Room for the longest frame (2048 with SBR) of up to 8 channels.
    let mut pcm = vec![0i16; 2048 * 8];
    if d.config_raw(asc).is_err() || d.fill(packet).is_err() || d.decode_frame(&mut pcm).is_err() {
        return 0;
    }
    u64::from(d.stream_info().outputDelay)
}

fn make_decoder(params: &AudioCodecParameters, fdk: bool) -> Result<Box<dyn AudioDecoder>> {
    let codecs = if fdk { fdk_codecs() } else { symphonia::default::get_codecs() };
    Ok(codecs.make_audio_decoder(params, &AudioDecoderOptions::default())?)
}

/// A track being decoded into interleaved stereo `f32`.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    params: AudioCodecParameters,
    track: u32,
    fdk: bool,
    /// Sample rate of the track as the container states it: what the decoder must produce.
    rate: u32,
    duration: Option<f64>,
    /// Frames still to drop: fdk's latency, and after an accurate seek the part of the packet
    /// before the target (the demuxer stops at a packet boundary).
    skip: u64,
    /// fdk's latency, once its first packet is decoded.
    delay: Option<u64>,
    samples: Vec<f32>,
    out: Vec<f32>,
}

impl Decoder {
    pub fn open(source: Box<dyn MediaSource>) -> Result<Decoder> {
        let mss = MediaSourceStream::new(source, Default::default());
        let format = symphonia::default::get_probe()
            .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
            .context("неизвестный формат файла")?;
        let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("в файле нет звука"))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| anyhow!("в файле нет звука"))?
            .clone();
        let rate = params.sample_rate.ok_or_else(|| anyhow!("неизвестная частота дискретизации"))?;
        let duration = track.num_frames.map(|n| n as f64 / f64::from(rate));
        let id = track.id;
        let fdk = params.codec == CODEC_ID_AAC && needs_fdk(params.extra_data.as_deref().unwrap_or(&[]));
        let decoder = make_decoder(&params, fdk)?;
        Ok(Decoder { format, decoder, params, track: id, fdk, rate, duration, skip: 0, delay: None, samples: Vec::new(), out: Vec::new() })
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn duration(&self) -> Option<f64> {
        self.duration
    }

    #[cfg(test)]
    fn uses_fdk(&self) -> bool {
        self.fdk
    }

    /// The next decoded chunk, interleaved stereo; `None` at the end of the track.
    pub fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let mut bad = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(Error::ResetRequired) => {
                    self.decoder = make_decoder(&self.params, self.fdk)?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id != self.track {
                continue;
            }
            let rate = match self.decoder.decode(&packet) {
                Ok(buf) => buf.spec().rate(),
                Err(Error::DecodeError(e)) => {
                    bad += 1;
                    if bad > MAX_BAD_PACKETS {
                        return Err(anyhow!("файл повреждён: {e}"));
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if rate < self.rate && !self.fdk && self.params.codec == CODEC_ID_AAC {
                // SBR the config did not reveal: decode with fdk from this packet on.
                self.decoder = make_decoder(&self.params, true)?;
                self.fdk = true;
                if self.decoder.decode(&packet).is_err() {
                    continue;
                }
            }
            if self.fdk && self.delay.is_none() {
                let delay = fdk_delay(self.params.extra_data.as_deref().unwrap_or(&[]), &packet.data);
                self.delay = Some(delay);
                self.skip += delay;
            }
            let buf = self.decoder.last_decoded();
            let channels = buf.spec().channels().count().max(1);
            self.samples.resize(buf.samples_interleaved(), 0.0);
            buf.copy_to_slice_interleaved(&mut self.samples);
            let frames = self.samples.len() / channels;
            let skip = self.skip.min(frames as u64) as usize;
            self.skip -= skip as u64;
            self.out.clear();
            for frame in self.samples.chunks_exact(channels).skip(skip) {
                let (l, r) = if channels == 1 { (frame[0], frame[0]) } else { (frame[0], frame[1]) };
                self.out.extend([l, r]);
            }
            if !self.out.is_empty() {
                return Ok(Some(&self.out));
            }
        }
    }

    /// Moves to `secs` from the start; returns the position actually reached, seconds.
    pub fn seek(&mut self, secs: f64) -> Result<f64> {
        let time = Time::try_from_secs_f64(secs.max(0.0)).ok_or_else(|| anyhow!("неверная позиция"))?;
        let to = self.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track) })?;
        self.decoder.reset();
        let (required, actual) = (to.required_ts.get().max(0) as u64, to.actual_ts.get().max(0) as u64);
        self.skip = required.saturating_sub(actual) + if self.fdk { self.delay.unwrap_or(0) } else { 0 };
        Ok(required as f64 / f64::from(self.rate))
    }
}

#[cfg(test)]
mod tests {
    use super::{needs_fdk, Decoder};
    use std::io::Cursor;

    fn open(data: &'static [u8]) -> Decoder {
        Decoder::open(Box::new(Cursor::new(data))).unwrap()
    }

    fn decode_all(d: &mut Decoder) -> Vec<f32> {
        let mut pcm = Vec::new();
        while let Some(chunk) = d.next_chunk().unwrap() {
            pcm.extend_from_slice(chunk);
        }
        pcm
    }

    /// Level of frequency `f` in the left channel (Goertzel), relative to a full-scale sine.
    fn tone(pcm: &[f32], rate: u32, f: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * f / f64::from(rate);
        let (mut s1, mut s2) = (0.0, 0.0);
        let n = pcm.len() / 2;
        for frame in pcm.chunks_exact(2) {
            let s = f64::from(frame[0]) + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt() * 2.0 / n as f64
    }

    /// RMS of the left channel above 8 kHz (two 2nd-order high-pass sections).
    fn high_band(pcm: &[f32], rate: u32) -> f64 {
        let w = 2.0 * std::f64::consts::PI * 8000.0 / f64::from(rate);
        let alpha = w.sin() / 2.0f64.sqrt();
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = ((1.0 + w.cos()) / 2.0 / a0, -(1.0 + w.cos()) / a0, (1.0 + w.cos()) / 2.0 / a0);
        let (a1, a2) = (-2.0 * w.cos() / a0, (1.0 - alpha) / a0);
        let mut x: Vec<f64> = pcm.chunks_exact(2).map(|f| f64::from(f[0])).collect();
        for _ in 0..2 {
            let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
            for v in &mut x {
                let y = b0 * *v + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
                (x2, x1, y2, y1) = (x1, *v, y1, y);
                *v = y;
            }
        }
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn audio_specific_config() {
        // AAC-LC as the server sends it.
        assert!(!needs_fdk(&[0x12, 0x10]));
        // HE-AAC as the server sends it: an LC config at 22050 Hz + sync extension with SBR.
        assert!(needs_fdk(&[0x13, 0x90, 0x56, 0xE5, 0xA0]));
        // LC with the sync extension saying «no SBR» (written by ffmpeg).
        assert!(!needs_fdk(&[0x12, 0x10, 0x56, 0xE5, 0x00]));
        // Implicit SBR: only the core rate (22050 Hz) gives it away.
        assert!(needs_fdk(&[0x13, 0x90]));
        // Explicit HE-AAC (object type 5).
        assert!(needs_fdk(&[0x2B, 0x94, 0x10]));
        // Truncated or missing config: let fdk deal with it.
        assert!(needs_fdk(&[0x12]));
        assert!(needs_fdk(&[]));
    }

    /// The test files hold 2 s of 440 Hz (0.3), 3 kHz (0.2) and a 9–14 kHz band (0.1 RMS); see
    /// `tests/data/README.md`.
    #[test]
    fn decodes_all_formats() {
        for (name, data, fdk) in [
            ("aac-lc", &include_bytes!("../tests/data/aac-lc.m4a")[..], false),
            ("he-aac", &include_bytes!("../tests/data/he-aac.m4a")[..], true),
            ("mp3", &include_bytes!("../tests/data/mp3.mp3")[..], false),
        ] {
            let mut d = open(data);
            assert_eq!(d.uses_fdk(), fdk, "{name}");
            assert_eq!(d.sample_rate(), 44100, "{name}");
            let pcm = decode_all(&mut d);
            let secs = pcm.len() as f64 / 2.0 / 44100.0;
            assert!((2.0..2.3).contains(&secs), "{name}: {secs} s");
            let duration = d.duration().unwrap();
            assert!((duration - secs).abs() < 0.1, "{name}: {duration} s, decoded {secs} s");
            let mid = &pcm[pcm.len() / 4..pcm.len() / 4 * 3];
            let (a, b) = (tone(mid, 44100, 440.0), tone(mid, 44100, 3000.0));
            assert!((a - 0.3).abs() < 0.03 && (b - 0.2).abs() < 0.03, "{name}: {a} {b}");
            // SBR recreates the high band only roughly (−5 dB here), but it must be there.
            let high = high_band(mid, 44100);
            assert!(high > 0.04, "{name}: {high}");
        }
    }

    #[test]
    fn seek_is_sample_accurate() {
        let data = include_bytes!("../tests/data/aac-lc.m4a");
        let full = decode_all(&mut open(data));
        let mut d = open(data);
        let at = d.seek(1.0).unwrap();
        assert!((at - 1.0).abs() < 0.001, "{at}");
        let after: Vec<f32> = std::iter::from_fn(|| d.next_chunk().unwrap().map(<[f32]>::to_vec)).flatten().collect();
        let start = (at * 44100.0).round() as usize * 2;
        assert_eq!(after.len(), full.len() - start);
        // A decoder restarted mid-stream needs a frame to settle. After that it matches, up to the
        // noise substitution (PNS) that ffmpeg's encoder uses: its generator restarts too.
        let settled = 4096;
        let diff = |lag: usize| {
            let full = &full[start + settled + lag * 2..];
            after[settled..].iter().zip(full).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max)
        };
        assert!(diff(0) < 0.005, "{}", diff(0));
        // One frame off is already far worse: the position is exact.
        assert!(diff(1) > 10.0 * diff(0), "{} {}", diff(0), diff(1));
    }

    /// With fdk (HE-AAC) its latency is dropped after a seek too: the sound lines up with the
    /// full decode (not sample for sample: SBR adds noise of its own).
    #[test]
    fn seek_with_fdk_keeps_alignment() {
        let data = include_bytes!("../tests/data/he-aac.m4a");
        let full = decode_all(&mut open(data));
        let mut d = open(data);
        // Play a little first: the latency is known by the time of the seek.
        for _ in 0..5 {
            d.next_chunk().unwrap();
        }
        let at = d.seek(1.0).unwrap();
        let after: Vec<f32> = std::iter::from_fn(|| d.next_chunk().unwrap().map(<[f32]>::to_vec)).flatten().collect();
        let start = (at * 44100.0).round() as i64;
        // Correlate 0.25 s from a settled point against the full decode around the same position.
        let (from, n) = (8192usize, 11025usize);
        let corr = |lag: i64| -> f64 {
            let base = ((start + from as i64 + lag) * 2) as usize;
            (0..n).map(|i| f64::from(after[(from + i) * 2]) * f64::from(full[base + i * 2])).sum()
        };
        let best = (-400i64..=400).max_by(|a, b| corr(*a).total_cmp(&corr(*b))).unwrap();
        assert_eq!(best, 0);
    }
}
