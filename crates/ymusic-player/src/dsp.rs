//! Sound processing of the own player, modelled on the ffmpeg filters mpv used, so that it sounds
//! the same: biquad equalizer bands (RBJ cookbook, as ffmpeg's `equalizer`, `lowshelf`,
//! `highshelf`), a look-ahead limiter in place of `alimiter`, and the spectrum the visualizer shows,
//! measured as `bandpass` + `aspectralstats` did.

use std::collections::VecDeque;
use std::f64::consts::PI;

/// A second-order IIR filter for a stereo signal (transposed direct form II).
#[derive(Clone, Debug)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [[f64; 2]; 2],
}

/// `alpha` of a band `bw` octaves wide around `w0` (ffmpeg's `width_type=o`).
fn alpha_octaves(w0: f64, bw: f64) -> f64 {
    w0.sin() * ((2f64.ln() / 2.0) * bw * w0 / w0.sin()).sinh()
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Biquad {
        Biquad { b: [b[0] / a[0], b[1] / a[0], b[2] / a[0]], a: [a[1] / a[0], a[2] / a[0]], z: [[0.0; 2]; 2] }
    }

    fn peaking(w0: f64, alpha: f64, gain_db: f64) -> Biquad {
        let a = 10f64.powf(gain_db / 40.0);
        let c = w0.cos();
        Biquad::new([1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a])
    }

    /// Peaking band `bw` octaves wide (`equalizer=t=o`).
    pub fn peaking_octaves(f: f64, bw: f64, gain_db: f64, rate: f64) -> Biquad {
        let w0 = 2.0 * PI * f / rate;
        Biquad::peaking(w0, alpha_octaves(w0, bw), gain_db)
    }

    /// Peaking band with quality factor `q` (`equalizer=t=q`).
    pub fn peaking_q(f: f64, q: f64, gain_db: f64, rate: f64) -> Biquad {
        let w0 = 2.0 * PI * f / rate;
        Biquad::peaking(w0, w0.sin() / (2.0 * q), gain_db)
    }

    /// Shelf with slope `s` (`lowshelf=t=s` / `highshelf=t=s`).
    pub fn shelf(high: bool, f: f64, s: f64, gain_db: f64, rate: f64) -> Biquad {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f / rate;
        let (c, sa) = (w0.cos(), a.sqrt());
        let alpha = w0.sin() / 2.0 * ((a + 1.0 / a) * (1.0 / s - 1.0) + 2.0).sqrt();
        if high {
            Biquad::new(
                [
                    a * ((a + 1.0) + (a - 1.0) * c + 2.0 * sa * alpha),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                    a * ((a + 1.0) + (a - 1.0) * c - 2.0 * sa * alpha),
                ],
                [(a + 1.0) - (a - 1.0) * c + 2.0 * sa * alpha, 2.0 * ((a - 1.0) - (a + 1.0) * c), (a + 1.0) - (a - 1.0) * c - 2.0 * sa * alpha],
            )
        } else {
            Biquad::new(
                [
                    a * ((a + 1.0) - (a - 1.0) * c + 2.0 * sa * alpha),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                    a * ((a + 1.0) - (a - 1.0) * c - 2.0 * sa * alpha),
                ],
                [(a + 1.0) + (a - 1.0) * c + 2.0 * sa * alpha, -2.0 * ((a - 1.0) + (a + 1.0) * c), (a + 1.0) + (a - 1.0) * c - 2.0 * sa * alpha],
            )
        }
    }

    /// Band-pass `bw` octaves wide with 0 dB at the peak (`bandpass=width_type=o`).
    pub fn bandpass_octaves(f: f64, bw: f64, rate: f64) -> Biquad {
        let w0 = 2.0 * PI * f / rate;
        let alpha = alpha_octaves(w0, bw);
        Biquad::new([alpha, 0.0, -alpha], [1.0 + alpha, -2.0 * w0.cos(), 1.0 - alpha])
    }

    /// Gain at frequency `f`, as a factor.
    pub fn response(&self, f: f64, rate: f64) -> f64 {
        let w = 2.0 * PI * f / rate;
        // H(z) at z = e^{jw}: evaluate numerator and denominator as complex numbers.
        let eval = |c: [f64; 3]| {
            let re = c[0] + c[1] * w.cos() + c[2] * (2.0 * w).cos();
            let im = -c[1] * w.sin() - c[2] * (2.0 * w).sin();
            re.hypot(im)
        };
        eval(self.b) / eval([1.0, self.a[0], self.a[1]])
    }

    /// One sample of a mono signal (uses the first channel's state).
    fn process_mono(&mut self, x: f64) -> f64 {
        let z = &mut self.z[0];
        let y = self.b[0] * x + z[0];
        z[0] = self.b[1] * x - self.a[0] * y + z[1];
        z[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    fn reset(&mut self) {
        self.z = [[0.0; 2]; 2];
    }

    fn process(&mut self, frame: &mut [f32]) {
        for (x, z) in frame.iter_mut().zip(&mut self.z) {
            let v = f64::from(*x);
            let y = self.b[0] * v + z[0];
            z[0] = self.b[1] * v - self.a[0] * y + z[1];
            z[1] = self.b[2] * v - self.a[1] * y;
            *x = y as f32;
        }
    }
}

/// Look-ahead peak limiter in place of ffmpeg's `alimiter` with its defaults (5 ms attack, 50 ms
/// release, output levelled back up to full scale; a 20 ms time constant of the release matches
/// its gain on music within 0.1 dB most of the time). The gain needed by every sample is held over
/// the look-ahead and smoothed with a box filter of the same length, so it is already down when a
/// peak arrives and changes without steps.
pub struct Limiter {
    limit: f32,
    release: f32,
    hold: usize,
    /// Gain after the release smoothing, before the hold.
    gain: f32,
    /// Look-ahead window of gains for the running minimum: (index, gain), increasing gains.
    minimum: VecDeque<(u64, f32)>,
    /// The held gains being averaged and their sum.
    boxed: VecDeque<f32>,
    sum: f64,
    delay: VecDeque<[f32; 2]>,
    n: u64,
}

impl Limiter {
    pub fn new(limit: f32, rate: f64) -> Limiter {
        let hold = (rate * 0.005).round() as usize;
        let release = (-1.0 / (rate * 0.02)).exp() as f32;
        let mut l = Limiter {
            limit,
            release,
            hold,
            gain: 1.0,
            minimum: VecDeque::new(),
            boxed: VecDeque::new(),
            sum: 0.0,
            delay: VecDeque::new(),
            n: 0,
        };
        l.reset();
        l
    }

    /// Forgets the sound so far: the look-ahead is silence again.
    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.minimum.clear();
        self.boxed.clear();
        self.boxed.extend(std::iter::repeat_n(1.0, self.hold));
        self.sum = self.hold as f64;
        // A peak's gain is fully down `hold - 1` frames after it enters: the sound waits as long.
        self.delay.clear();
        self.delay.extend(std::iter::repeat_n([0.0; 2], self.hold - 1));
        self.n = 0;
    }

    /// Processes interleaved stereo; the output lags the input by the look-ahead.
    pub fn process(&mut self, samples: &mut [f32]) {
        for frame in samples.as_chunks_mut::<2>().0 {
            self.process_frame(frame);
        }
    }

    fn process_frame(&mut self, frame: &mut [f32]) {
        let peak = frame[0].abs().max(frame[1].abs());
        let target = if peak > self.limit { self.limit / peak } else { 1.0 };
        // Down at once, back up slowly.
        self.gain = if target < self.gain { target } else { target + (self.gain - target) * self.release };
        while self.minimum.back().is_some_and(|&(_, g)| g >= self.gain) {
            self.minimum.pop_back();
        }
        self.minimum.push_back((self.n, self.gain));
        while self.minimum.front().is_some_and(|&(i, _)| i + self.hold as u64 <= self.n) {
            self.minimum.pop_front();
        }
        let held = self.minimum.front().map_or(1.0, |&(_, g)| g);
        self.sum += f64::from(held) - f64::from(self.boxed.pop_front().unwrap_or(1.0));
        self.boxed.push_back(held);
        let gain = (self.sum / self.hold as f64) as f32 / self.limit;
        self.delay.push_back([frame[0], frame[1]]);
        let out = self.delay.pop_front().unwrap_or_default();
        frame[0] = out[0] * gain;
        frame[1] = out[1] * gain;
        self.n += 1;
    }
}

/// An equalizer: bands in a row, then the limiter that keeps boosted ones from clipping.
pub struct Equalizer {
    bands: Vec<Biquad>,
    limiter: Limiter,
}

impl Equalizer {
    pub fn new(bands: Vec<Biquad>, rate: f64) -> Equalizer {
        Equalizer { bands, limiter: Limiter::new(0.9, rate) }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for frame in samples.as_chunks_mut::<2>().0 {
            for b in &mut self.bands {
                b.process(frame);
            }
            self.limiter.process_frame(frame);
        }
    }

    /// Forgets the sound so far, as after a seek.
    pub fn reset(&mut self) {
        for b in &mut self.bands {
            b.reset();
        }
        self.limiter.reset();
    }
}

/// Gains of a crossfade `t` of the way through (0..1): the outgoing track's and the incoming
/// one's. Equal power — the squares add up to 1 — so two unrelated tracks do not dip in the middle.
pub fn crossfade_gains(t: f64) -> (f32, f32) {
    let a = t.clamp(0.0, 1.0) * std::f64::consts::FRAC_PI_2;
    (a.cos() as f32, a.sin() as f32)
}

/// Linear resampling, for the rare track (or device) with another rate than ours.
pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame in the input chunk; -1 is the previous chunk's last.
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    pub fn to(from: u32, to: u32) -> Resampler {
        Resampler { step: f64::from(from) / f64::from(to), pos: 0.0, prev: [0.0; 2] }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = [0.0; 2];
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.step == 1.0 {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / 2;
        if frames == 0 {
            return;
        }
        let at = |i: isize| if i < 0 { self.prev } else { [input[i as usize * 2], input[i as usize * 2 + 1]] };
        while self.pos < frames as f64 - 1.0 {
            let i = self.pos.floor();
            let f = (self.pos - i) as f32;
            let (a, b) = (at(i as isize), at(i as isize + 1));
            out.extend([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]);
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        self.prev = at(frames as isize - 1);
    }
}

/// Radix-2 FFT of a fixed length with its twiddle factors and bit-reversal order computed once.
struct Fft {
    cos: Vec<f64>,
    sin: Vec<f64>,
    reversed: Vec<usize>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        let bits = n.trailing_zeros();
        let angle = |k: usize| -2.0 * PI * k as f64 / n as f64;
        Fft {
            cos: (0..n / 2).map(|k| angle(k).cos()).collect(),
            sin: (0..n / 2).map(|k| angle(k).sin()).collect(),
            reversed: (0..n).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect(),
        }
    }

    /// In place, on `re` + i·`im`.
    fn run(&self, re: &mut [f64], im: &mut [f64]) {
        let n = re.len();
        for (i, &j) in self.reversed.iter().enumerate() {
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let stride = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = (self.cos[k * stride], self.sin[k * stride]);
                    let (a, b) = (start + k, start + k + len / 2);
                    let (tr, ti) = (re[b] * c - im[b] * s, re[b] * s + im[b] * c);
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }
}

/// Window of the spectrum measurement, frames (as `aspectralstats=win_size=1024`).
pub const WINDOW: usize = 1024;

/// The visualizer's measurement: for each band, the mean spectral magnitude of the mono signal
/// passed through the band's band-pass filter — what `bandpass` + `aspectralstats=measure=mean`
/// gave. Bands wider than a few FFT bins come from one FFT weighted by each filter's response;
/// the narrow low ones are filtered for real and get FFTs of their own, as ffmpeg did, since a
/// window of 1024 frames does not resolve them.
pub struct Spectrum {
    /// Filter response per FFT bin, for the wide bands.
    weights: Vec<Vec<f64>>,
    /// Filters of the narrow bands (the first ones) and their filtered windows.
    narrow: Vec<(Biquad, Vec<f64>)>,
    window: Vec<f64>,
    buf: Vec<f64>,
    fft: Fft,
}

/// Bands narrower than this many FFT bins are filtered for real (the four lowest, up to 130 Hz).
const NARROW_BINS: f64 = 1.5;

impl Spectrum {
    /// Bands at centre frequencies `freqs` (rising), each `bw` octaves wide.
    pub fn new(freqs: &[f64], bw: f64, rate: f64) -> Spectrum {
        let bins = WINDOW / 2 + 1;
        let bin_width = rate / WINDOW as f64;
        let width = |f: f64| f * (2f64.powf(bw / 2.0) - 2f64.powf(-bw / 2.0));
        let (narrow, wide): (Vec<f64>, Vec<f64>) = freqs.iter().partition(|&&f| width(f) < NARROW_BINS * bin_width);
        let weights = wide
            .iter()
            .map(|&f| {
                let b = Biquad::bandpass_octaves(f, bw, rate);
                (0..bins).map(|k| b.response(k as f64 * bin_width, rate)).collect()
            })
            .collect();
        let narrow = narrow.iter().map(|&f| (Biquad::bandpass_octaves(f, bw, rate), Vec::with_capacity(WINDOW))).collect();
        let window = (0..WINDOW).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / WINDOW as f64).cos()).collect();
        Spectrum { weights, narrow, window, buf: Vec::with_capacity(WINDOW), fft: Fft::new(WINDOW) }
    }

    /// Feeds stereo samples; for every window that fills up, calls `out` with the number of
    /// frames fed so far and the band levels (dB).
    pub fn feed(&mut self, samples: &[f32], mut out: impl FnMut(usize, Vec<f32>)) {
        for (i, frame) in samples.as_chunks::<2>().0.iter().enumerate() {
            let mono = (f64::from(frame[0]) + f64::from(frame[1])) / 2.0;
            self.buf.push(mono);
            for (b, buf) in &mut self.narrow {
                buf.push(b.process_mono(mono));
            }
            if self.buf.len() == WINDOW {
                out(i + 1, self.measure());
                self.buf.clear();
                for (_, buf) in &mut self.narrow {
                    buf.clear();
                }
            }
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        for (b, buf) in &mut self.narrow {
            b.reset();
            buf.clear();
        }
    }

    /// Magnitudes of two windowed real signals at once (one as the real part of a complex FFT,
    /// the other as the imaginary part), scaled as ffmpeg's (by the window length).
    fn magnitudes(&self, a: &[f64], b: Option<&[f64]>) -> [Vec<f64>; 2] {
        let mut re: Vec<f64> = a.iter().zip(&self.window).map(|(x, w)| x * w).collect();
        let mut im: Vec<f64> = match b {
            Some(b) => b.iter().zip(&self.window).map(|(x, w)| x * w).collect(),
            None => vec![0.0; WINDOW],
        };
        self.fft.run(&mut re, &mut im);
        // Z = A + iB: A[k] = (Z[k] + conj Z[N-k]) / 2, B[k] = (Z[k] - conj Z[N-k]) / 2i.
        let scale = 2.0 * WINDOW as f64;
        let mut out = [Vec::with_capacity(WINDOW / 2 + 1), Vec::with_capacity(WINDOW / 2 + 1)];
        for k in 0..=WINDOW / 2 {
            let m = (WINDOW - k) % WINDOW;
            out[0].push((re[k] + re[m]).hypot(im[k] - im[m]) / scale);
            out[1].push((im[k] + im[m]).hypot(re[m] - re[k]) / scale);
        }
        out
    }

    fn measure(&self) -> Vec<f32> {
        let db = |mean: f64| if mean > 0.0 { (20.0 * mean.log10()).max(-150.0) as f32 } else { -150.0 };
        let mean = |m: &[f64]| m.iter().sum::<f64>() / m.len() as f64;
        // The whole signal and the narrow bands' filtered ones, two per FFT.
        let signals: Vec<&[f64]> = std::iter::once(self.buf.as_slice()).chain(self.narrow.iter().map(|(_, b)| b.as_slice())).collect();
        let mut mags = Vec::with_capacity(signals.len());
        for pair in signals.chunks(2) {
            let [a, b] = self.magnitudes(pair[0], pair.get(1).copied());
            mags.push(a);
            if pair.len() == 2 {
                mags.push(b);
            }
        }
        let narrow = mags[1..].iter().map(|m| db(mean(m)));
        let all = &mags[0];
        let wide = self.weights.iter().map(|w| db(all.iter().zip(w).map(|(m, w)| m * w).sum::<f64>() / all.len() as f64));
        narrow.chain(wide).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 44100.0;

    fn sine(f: f64, amp: f32, secs: f64) -> Vec<f32> {
        (0..(RATE * secs) as usize)
            .flat_map(|n| {
                let v = amp * (2.0 * PI * f * n as f64 / RATE).sin() as f32;
                [v, v]
            })
            .collect()
    }

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn resampling() {
        // 22050 Hz → 44100: twice the frames, midpoints in between, continuous across chunks.
        let mut r = Resampler::to(22050, 44100);
        let mut out = Vec::new();
        r.process(&[0.0, 0.0, 1.0, -1.0], &mut out);
        r.process(&[2.0, -2.0, 3.0, -3.0], &mut out);
        assert_eq!(out, [0.0, 0.0, 0.5, -0.5, 1.0, -1.0, 1.5, -1.5, 2.0, -2.0, 2.5, -2.5]);
        let mut same = Resampler::to(44100, 44100);
        let mut out = Vec::new();
        same.process(&[0.1, 0.2], &mut out);
        assert_eq!(out, [0.1, 0.2]);
        // 44100 → 48000: about 48000 frames out of a second.
        let mut up = Resampler::to(44100, 48000);
        let mut out = Vec::new();
        for _ in 0..10 {
            up.process(&vec![0.0; 8820], &mut out);
        }
        assert!((out.len() / 2).abs_diff(48000) <= 1, "{}", out.len() / 2);
    }

    #[test]
    fn biquad_responses() {
        let db = |b: &Biquad, f: f64| 20.0 * b.response(f, RATE).log10();
        let p = Biquad::peaking_octaves(1000.0, 1.0, 6.0, RATE);
        assert!((db(&p, 1000.0) - 6.0).abs() < 0.01);
        assert!(db(&p, 50.0).abs() < 0.1 && db(&p, 15000.0).abs() < 0.2);
        let q = Biquad::peaking_q(3000.0, 1.0, -4.0, RATE);
        assert!((db(&q, 3000.0) + 4.0).abs() < 0.01);
        let low = Biquad::shelf(false, 100.0, 1.0, 5.0, RATE);
        assert!((db(&low, 10.0) - 5.0).abs() < 0.1 && db(&low, 5000.0).abs() < 0.1);
        assert!((db(&low, 100.0) - 2.5).abs() < 0.1, "half the gain at the corner");
        let high = Biquad::shelf(true, 10000.0, 1.0, 7.0, RATE);
        assert!((db(&high, 20000.0) - 7.0).abs() < 0.3 && db(&high, 200.0).abs() < 0.1);
        let bp = Biquad::bandpass_octaves(2000.0, 0.5, RATE);
        assert!(db(&bp, 2000.0).abs() < 0.01 && db(&bp, 200.0) < -15.0);

        // Filtering a sine changes its level by the response.
        let mut s = sine(1000.0, 0.25, 0.5);
        let mut b = Biquad::peaking_octaves(1000.0, 1.0, 6.0, RATE);
        for f in s.as_chunks_mut::<2>().0 {
            b.process(f);
        }
        let tail = peak(&s[s.len() / 2..]);
        assert!((tail / 0.25 - 10f32.powf(6.0 / 20.0)).abs() < 0.01, "{tail}");
    }

    #[test]
    fn limiter() {
        // Quiet input is only delayed and levelled up; a loud one is held at full scale.
        let mut l = Limiter::new(0.9, RATE);
        let mut quiet = sine(440.0, 0.5, 0.2);
        l.process(&mut quiet);
        assert!((peak(&quiet) - 0.5 / 0.9).abs() < 0.01, "{}", peak(&quiet));
        let mut loud = sine(440.0, 2.0, 0.5);
        loud.extend(sine(440.0, 0.3, 0.5));
        let mut l = Limiter::new(0.9, RATE);
        l.process(&mut loud);
        assert!(peak(&loud) <= 1.0 + 1e-6, "{}", peak(&loud));
        assert!(peak(&loud[..loud.len() / 2]) > 0.99);
        // After the release the quiet part comes back to its level.
        let end = &loud[loud.len() - 4410..];
        assert!((peak(end) - 0.3 / 0.9).abs() < 0.01, "{}", peak(end));
    }

    #[test]
    fn crossfade() {
        assert_eq!(crossfade_gains(0.0), (1.0, 0.0));
        let (out, inc) = crossfade_gains(1.0);
        assert!(out.abs() < 1e-6 && inc == 1.0);
        for t in [0.1, 0.5, 0.9] {
            let (out, inc) = crossfade_gains(t);
            assert!((out * out + inc * inc - 1.0).abs() < 1e-6, "equal power at {t}");
        }
        assert_eq!(crossfade_gains(-1.0), (1.0, 0.0));
        assert_eq!(crossfade_gains(2.0), crossfade_gains(1.0));
    }

    #[test]
    fn spectrum_bands() {
        let freqs: Vec<f64> = (0..16).map(|i| 40.0 * 2f64.powf(i as f64 * 0.576)).collect();
        let mut s = Spectrum::new(&freqs, 0.576, RATE);
        let mut levels = Vec::new();
        s.feed(&sine(freqs[9], 0.5, 0.1), |_, l| levels = l);
        let loudest = (0..16).max_by(|&a, &b| levels[a].total_cmp(&levels[b])).unwrap();
        assert_eq!(loudest, 9, "{levels:?}");
        assert!(levels[2] < levels[9] - 30.0);
        // Silence brings the bars down (the narrow filters ring out for a while).
        s.feed(&vec![0.0; WINDOW * 4], |_, l| levels = l);
        assert!(levels.iter().all(|&l| l < -110.0), "{levels:?}");
        assert!(levels[8..].iter().all(|&l| l == -150.0), "{levels:?}");
    }
}
