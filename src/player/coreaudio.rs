//! Sound output on macOS through Core Audio: the default output unit takes frames from the ring
//! in its render callback and converts them to the device's format and rate itself. It follows
//! the system's choice of device (headphones plugged in, AirPods connected).

use super::output::{Ring, RATE};
use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};

type OSStatus = i32;
type AudioUnit = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    sub_type: u32,
    manufacturer: u32,
    flags: u32,
    flags_mask: u32,
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct SmpteTime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    kind: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SmpteTime,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type RenderCallback =
    extern "C" fn(*mut c_void, *mut u32, *const AudioTimeStamp, u32, u32, *mut AudioBufferList) -> OSStatus;

#[repr(C)]
struct AURenderCallbackStruct {
    proc_: RenderCallback,
    ref_con: *mut c_void,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
const OUTPUT: u32 = fourcc(b"auou");
const DEFAULT_OUTPUT: u32 = fourcc(b"def ");
const APPLE: u32 = fourcc(b"appl");
const LINEAR_PCM: u32 = fourcc(b"lpcm");
const FLOAT: u32 = 1;
const PACKED: u32 = 1 << 3;
const STREAM_FORMAT: u32 = 8;
const SET_RENDER_CALLBACK: u32 = 23;
const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const HOST_TIME_VALID: u32 = 1 << 1;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(component: *mut c_void, desc: *const AudioComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, unit: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitReset(unit: AudioUnit, scope: u32, element: u32) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioGetCurrentHostTime() -> u64;
    fn AudioConvertHostTimeToNanos(host_time: u64) -> u64;
}

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(anyhow!("Core Audio: {what}: ошибка {status}"))
    }
}

/// What the render callback shares with the rest.
struct Shared {
    ring: Arc<Ring>,
    /// The callback's own buffer, kept to not allocate on the audio thread.
    samples: Mutex<Vec<f32>>,
    /// Frames the last callback took from the ring and the host time the first of them is heard.
    heard: Mutex<(u64, u64)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic on the audio thread aborts, so a poisoned lock holds nothing half-done.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

extern "C" fn render(
    ctx: *mut c_void,
    _flags: *mut u32,
    time: *const AudioTimeStamp,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: `ctx` is the `Shared` that `CoreAudio` keeps until the unit is disposed; Core Audio
    // passes a valid time stamp and an interleaved buffer of the format we set.
    let (shared, time, list) = unsafe { (&*ctx.cast::<Shared>(), &*time, &mut *data) };
    if list.count == 0 {
        return 0;
    }
    let buffer = &mut list.buffers[0];
    let len = (frames as usize * 2).min(buffer.byte_size as usize / 4);
    // SAFETY: the buffer holds `byte_size` bytes of `f32`.
    let out = unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<f32>(), len) };
    let mut samples = lock(&shared.samples);
    samples.clear();
    let taken = shared.ring.take_samples(len / 2, &mut samples);
    out[..taken * 2].copy_from_slice(&samples);
    out[taken * 2..].fill(0.0);
    // SAFETY: plain calls.
    let at = if time.flags & HOST_TIME_VALID != 0 { time.host_time } else { unsafe { AudioGetCurrentHostTime() } };
    *lock(&shared.heard) = (taken as u64, at);
    0
}

pub struct CoreAudio {
    unit: AudioUnit,
    shared: Box<Shared>,
    paused: Mutex<bool>,
}

// SAFETY: the unit's functions may be called from any thread.
unsafe impl Send for CoreAudio {}
unsafe impl Sync for CoreAudio {}

impl CoreAudio {
    pub fn open(ring: Arc<Ring>) -> Result<CoreAudio> {
        let desc = AudioComponentDescription {
            component_type: OUTPUT,
            sub_type: DEFAULT_OUTPUT,
            manufacturer: APPLE,
            flags: 0,
            flags_mask: 0,
        };
        // SAFETY: a valid description; `unit` is written on success.
        let unit = unsafe {
            let component = AudioComponentFindNext(std::ptr::null_mut(), &desc);
            if component.is_null() {
                return Err(anyhow!("Core Audio: нет устройства вывода"));
            }
            let mut unit = std::ptr::null_mut();
            check(AudioComponentInstanceNew(component, &mut unit), "AudioComponentInstanceNew")?;
            unit
        };
        let shared = Box::new(Shared {
            ring,
            samples: Mutex::new(Vec::with_capacity(16384)),
            heard: Mutex::new((0, 0)),
        });
        // From here on `Drop` disposes of the unit.
        let out = CoreAudio { unit, shared, paused: Mutex::new(true) };
        let format = AudioStreamBasicDescription {
            sample_rate: f64::from(RATE),
            format_id: LINEAR_PCM,
            format_flags: FLOAT | PACKED,
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels_per_frame: 2,
            bits_per_channel: 32,
            reserved: 0,
        };
        let callback = AURenderCallbackStruct { proc_: render, ref_con: (&*out.shared as *const Shared).cast_mut().cast() };
        // SAFETY: properties of the types they are set with; `shared` outlives the unit.
        unsafe {
            let set = |id, data: *const c_void, size: usize| AudioUnitSetProperty(unit, id, SCOPE_INPUT, 0, data, size as u32);
            check(set(STREAM_FORMAT, (&format as *const AudioStreamBasicDescription).cast(), size_of_val(&format)), "формат")?;
            check(
                set(SET_RENDER_CALLBACK, (&callback as *const AURenderCallbackStruct).cast(), size_of_val(&callback)),
                "обработчик",
            )?;
            check(AudioUnitInitialize(unit), "AudioUnitInitialize")?;
        }
        out.set_paused(false)?;
        Ok(out)
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        let mut p = lock(&self.paused);
        if *p == paused {
            return Ok(());
        }
        // SAFETY: an initialized unit; `stop` returns once the callback is done.
        if paused {
            check(unsafe { AudioOutputUnitStop(self.unit) }, "остановка")?;
            // What was in the device is dropped.
            *lock(&self.shared.heard) = (0, 0);
        } else {
            check(unsafe { AudioOutputUnitStart(self.unit) }, "запуск")?;
        }
        *p = paused;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        // SAFETY: an initialized unit.
        check(unsafe { AudioUnitReset(self.unit, SCOPE_GLOBAL, 0) }, "сброс")?;
        *lock(&self.shared.heard) = (0, 0);
        Ok(())
    }

    /// Frames still to be heard: of those the last callback took, the ones not played yet.
    pub fn delay(&self) -> Result<u64> {
        let (frames, at) = *lock(&self.shared.heard);
        if frames == 0 {
            return Ok(0);
        }
        // SAFETY: plain calls.
        let (now, at) = unsafe { (AudioConvertHostTimeToNanos(AudioGetCurrentHostTime()), AudioConvertHostTimeToNanos(at)) };
        let ahead = frames as i128 + (i128::from(at) - i128::from(now)) * i128::from(RATE) / 1_000_000_000;
        Ok(ahead.max(0) as u64)
    }
}

impl Drop for CoreAudio {
    fn drop(&mut self) {
        // SAFETY: the unit is ours; once stopped and disposed the callback no longer runs.
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}
