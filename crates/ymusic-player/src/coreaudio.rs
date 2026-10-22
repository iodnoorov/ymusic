//! Sound output on macOS through Core Audio: the default output unit takes frames from the ring
//! in its render callback and converts them to the device's format and rate itself. It follows
//! the system's choice of device (headphones plugged in, AirPods connected).
//!
//! The callback runs on a real-time thread: it takes no locks, allocates nothing and makes no
//! system calls — the ring is lock-free, and what it tells the rest goes through atomics.

//!
//! The bindings come from Apple's SDK headers (objc2-audio-toolbox and friends), so the structures
//! are laid out as Core Audio expects, not as written out by hand.

use super::output::{Sink, RATE};
use super::ring::Ring;
use crate::Lock;
use anyhow::{anyhow, Result};
use objc2_audio_toolbox::{
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat,
    kAudioUnitScope_Global, kAudioUnitScope_Input, kAudioUnitSubType_DefaultOutput, kAudioUnitType_Output,
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext, AudioComponentInstanceDispose,
    AudioComponentInstanceNew, AudioOutputUnitStart, AudioOutputUnitStop, AudioUnit, AudioUnitInitialize,
    AudioUnitRenderActionFlags, AudioUnitReset, AudioUnitSetProperty, AudioUnitUninitialize,
};
use objc2_core_audio::{AudioConvertHostTimeToNanos, AudioGetCurrentHostTime};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, AudioBufferList,
    AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type OSStatus = i32;

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
    heard: Heard,
}

/// Frames the last callback took from the ring and the host time the first of them is heard: a
/// pair written by the callback and read by the control thread, kept consistent without a lock
/// (a sequence lock: odd while being written). Only the callback writes it; the control thread
/// marks it `dropped` when the device's sound is thrown away, until the next callback.
#[derive(Default)]
struct Heard {
    seq: AtomicU64,
    frames: AtomicU64,
    at: AtomicU64,
    dropped: AtomicBool,
}

impl Heard {
    /// For the callback.
    fn set(&self, frames: u64, at: u64) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq + 1, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.frames.store(frames, Ordering::Relaxed);
        self.at.store(at, Ordering::Relaxed);
        self.seq.store(seq + 2, Ordering::Release);
        self.dropped.store(false, Ordering::Release);
    }

    /// Nothing is in the device any more (stopped or reset).
    fn drop_all(&self) {
        self.dropped.store(true, Ordering::Release);
    }

    fn get(&self) -> (u64, u64) {
        if self.dropped.load(Ordering::Acquire) {
            return (0, 0);
        }
        loop {
            let seq = self.seq.load(Ordering::Acquire);
            let (frames, at) = (self.frames.load(Ordering::Relaxed), self.at.load(Ordering::Relaxed));
            std::sync::atomic::fence(Ordering::Acquire);
            if seq.is_multiple_of(2) && self.seq.load(Ordering::Relaxed) == seq {
                return (frames, at);
            }
            std::hint::spin_loop();
        }
    }
}

/// Fills the device's buffer from the ring. A panic is not let out into Core Audio.
unsafe extern "C-unwind" fn render(
    ctx: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    time: NonNull<AudioTimeStamp>,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `ctx` is the `Shared` that `CoreAudio` keeps until the unit is disposed; Core
        // Audio passes a valid time stamp and an interleaved buffer of the format we set.
        let (shared, time) = unsafe { (ctx.cast::<Shared>().as_ref(), time.as_ref()) };
        // SAFETY: as above; null only in notifications, which this callback is not set for.
        let Some(list) = (unsafe { data.as_mut() }) else { return 0 };
        if list.mNumberBuffers == 0 {
            return 0;
        }
        let buffer = &mut list.mBuffers[0];
        let len = (frames as usize * 2).min(buffer.mDataByteSize as usize / 4);
        // SAFETY: the buffer holds `mDataByteSize` bytes of `f32`.
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), len) };
        let taken = shared.ring.take_into(out);
        out[taken * 2..].fill(0.0);
        let at = if time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
            time.mHostTime
        } else {
            // SAFETY: a plain call.
            unsafe { AudioGetCurrentHostTime() }
        };
        shared.heard.set(taken as u64, at);
        0
    }))
    .unwrap_or(-1)
}

pub struct CoreAudio {
    unit: AudioUnit,
    shared: Box<Shared>,
    paused: Mutex<bool>,
}

// SAFETY: the unit's functions may be called from any thread.
unsafe impl Send for CoreAudio {}

impl CoreAudio {
    pub fn open(ring: Arc<Ring>) -> Result<CoreAudio> {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_DefaultOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        // SAFETY: a valid description; `unit` is written on success.
        let unit = unsafe {
            let component = AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&desc));
            if component.is_null() {
                return Err(anyhow!("Core Audio: нет устройства вывода"));
            }
            let mut unit: AudioUnit = std::ptr::null_mut();
            check(AudioComponentInstanceNew(component, NonNull::from(&mut unit)), "AudioComponentInstanceNew")?;
            unit
        };
        let shared = Box::new(Shared { ring, heard: Heard::default() });
        // From here on `Drop` disposes of the unit.
        let out = CoreAudio { unit, shared, paused: Mutex::new(true) };
        let format = AudioStreamBasicDescription {
            mSampleRate: f64::from(RATE),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let callback = AURenderCallbackStruct {
            inputProc: Some(render),
            inputProcRefCon: (&*out.shared as *const Shared).cast_mut().cast(),
        };
        // SAFETY: properties of the types they are set with; `shared` outlives the unit.
        unsafe {
            let set = |id, data: *const c_void, size: usize| {
                AudioUnitSetProperty(unit, id, kAudioUnitScope_Input, 0, data, size as u32)
            };
            let format_ptr: *const AudioStreamBasicDescription = &format;
            check(set(kAudioUnitProperty_StreamFormat, format_ptr.cast(), size_of_val(&format)), "формат")?;
            let callback_ptr: *const AURenderCallbackStruct = &callback;
            check(set(kAudioUnitProperty_SetRenderCallback, callback_ptr.cast(), size_of_val(&callback)), "обработчик")?;
            check(AudioUnitInitialize(unit), "AudioUnitInitialize")?;
        }
        out.set_paused(false)?;
        Ok(out)
    }
}

impl Sink for CoreAudio {
    fn set_paused(&self, paused: bool) -> Result<()> {
        let mut p = self.paused.locked();
        if *p == paused {
            return Ok(());
        }
        if paused {
            // SAFETY: an initialized unit; `stop` returns once the callback is done.
            check(unsafe { AudioOutputUnitStop(self.unit) }, "остановка")?;
            // What was in the device is dropped.
            self.shared.heard.drop_all();
        } else {
            // SAFETY: an initialized unit.
            check(unsafe { AudioOutputUnitStart(self.unit) }, "запуск")?;
        }
        *p = paused;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        // SAFETY: an initialized unit.
        check(unsafe { AudioUnitReset(self.unit, kAudioUnitScope_Global, 0) }, "сброс")?;
        self.shared.heard.drop_all();
        Ok(())
    }

    /// Frames still to be heard: of those the last callback took, the ones not played yet.
    fn delay(&self) -> Result<u64> {
        let (frames, at) = self.shared.heard.get();
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
