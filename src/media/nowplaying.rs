//! Now Playing on macOS: the media keys, Control Center and the menu bar's player control the
//! program and show what plays. Commands are delivered on the main thread's run loop, which
//! `sync` turns on every pass of the UI loop; covers are downloaded in a thread of their own.

use super::{Clock, Remote, Repeat, State, Status};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AllocAnyThread;
use objc2_app_kit::NSImage;
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop, CGSize};
use objc2_foundation::{NSArray, NSData, NSMutableDictionary, NSNumber, NSString, NSURL};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPChangeRepeatModeCommandEvent, MPChangeShuffleModeCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState,
    MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus, MPRepeatType,
    MPShuffleType,
};
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// Seconds the skip commands move by, as the arrow keys do.
const SEEK_STEP: f64 = 10.0;

type Send = Arc<dyn Fn(Remote) + std::marker::Send + Sync>;

pub struct NowPlaying {
    last: State,
    clock: Clock,
    /// Cover of the current track and its URL.
    art: Option<(String, Retained<MPMediaItemArtwork>)>,
    /// Downloaded covers: (URL, image file).
    art_tx: Sender<(String, Vec<u8>)>,
    art_rx: Receiver<(String, Vec<u8>)>,
}

impl NowPlaying {
    /// Takes over the system's media commands; they come back through `send`.
    pub fn start(send: impl Fn(Remote) + std::marker::Send + Sync + 'static) -> NowPlaying {
        let send: Send = Arc::new(send);
        // SAFETY: the shared command center lives as long as the process; its commands keep the
        // handlers they are given.
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();
            let on = |command: &MPRemoteCommand, remote: Remote| {
                let send = send.clone();
                command.addTargetWithHandler(&RcBlock::new(move |_: NonNull<MPRemoteCommandEvent>| {
                    send(remote.clone());
                    MPRemoteCommandHandlerStatus::Success
                }));
            };
            on(&center.playCommand(), Remote::Play);
            on(&center.pauseCommand(), Remote::Pause);
            on(&center.togglePlayPauseCommand(), Remote::PlayPause);
            on(&center.stopCommand(), Remote::Pause);
            on(&center.nextTrackCommand(), Remote::Next);
            on(&center.previousTrackCommand(), Remote::Previous);
            // By the step of ← and →.
            let step = NSArray::from_retained_slice(&[NSNumber::new_f64(SEEK_STEP)]);
            for (command, by) in [(center.skipForwardCommand(), SEEK_STEP), (center.skipBackwardCommand(), -SEEK_STEP)] {
                command.setPreferredIntervals(&step);
                on(&command, Remote::Seek(by));
            }
            let seek = send.clone();
            center.changePlaybackPositionCommand().addTargetWithHandler(&RcBlock::new(
                move |event: NonNull<MPRemoteCommandEvent>| {
                    // SAFETY: this command's events are position changes.
                    let event = event.cast::<MPChangePlaybackPositionCommandEvent>();
                    seek(Remote::SeekTo(event.as_ref().positionTime()));
                    MPRemoteCommandHandlerStatus::Success
                },
            ));
            let shuffle = send.clone();
            center.changeRepeatModeCommand().addTargetWithHandler(&RcBlock::new(
                move |event: NonNull<MPRemoteCommandEvent>| {
                    // SAFETY: this command's events are repeat mode changes.
                    let event = event.cast::<MPChangeRepeatModeCommandEvent>();
                    let repeat = match event.as_ref().repeatType() {
                        MPRepeatType::All => Repeat::All,
                        MPRepeatType::One => Repeat::One,
                        _ => Repeat::Off,
                    };
                    send(Remote::Repeat(repeat));
                    MPRemoteCommandHandlerStatus::Success
                },
            ));
            center.changeShuffleModeCommand().addTargetWithHandler(&RcBlock::new(
                move |event: NonNull<MPRemoteCommandEvent>| {
                    // SAFETY: this command's events are shuffle mode changes.
                    let event = event.cast::<MPChangeShuffleModeCommandEvent>();
                    shuffle(Remote::Shuffle(event.as_ref().shuffleType() != MPShuffleType::Off));
                    MPRemoteCommandHandlerStatus::Success
                },
            ));
        }
        let (art_tx, art_rx) = mpsc::channel();
        NowPlaying { last: State::default(), clock: Clock::default(), art: None, art_tx, art_rx }
    }

    /// Takes the app's state and position (seconds), tells the system what changed. Runs on the
    /// main thread: that is where the commands are delivered.
    pub fn sync(&mut self, state: State, position: f64) {
        // SAFETY: reads a constant of CoreFoundation.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.0, false);

        let same_track = state.track.as_ref().map(|t| &t.id) == self.last.track.as_ref().map(|t| &t.id);
        let seeked = self.clock.seeked(position, self.last.status == Status::Playing, same_track);
        let art_url = state.track.as_ref().and_then(|t| t.art_url.clone());
        let mut new_art = false;
        while let Ok((url, bytes)) = self.art_rx.try_recv() {
            if art_url.as_ref() == Some(&url)
                && let Some(art) = artwork(&bytes)
            {
                self.art = Some((url, art));
                new_art = true;
            }
        }
        let last_art = self.last.track.as_ref().and_then(|t| t.art_url.clone());
        if let Some(url) = art_url.filter(|u| Some(u) != last_art.as_ref() && self.art.as_ref().is_none_or(|(a, _)| a != u)) {
            fetch_art(url, self.art_tx.clone());
        }
        if state != self.last || seeked || new_art {
            self.publish(&state, position);
            self.last = state;
        }
    }

    fn publish(&self, state: &State, position: f64) {
        // SAFETY: the keys are constants of MediaPlayer, the values of the types they take.
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            let Some(t) = &state.track else {
                center.setNowPlayingInfo(None);
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                return;
            };
            let info = NSMutableDictionary::<NSString, AnyObject>::new();
            let put = |key: &NSString, value: &AnyObject| info.insert(key, value);
            put(MPMediaItemPropertyTitle, &NSString::from_str(&t.title));
            put(MPMediaItemPropertyArtist, &NSString::from_str(&t.artists));
            put(MPMediaItemPropertyAlbumTitle, &NSString::from_str(&t.album));
            put(MPMediaItemPropertyPlaybackDuration, &NSNumber::new_f64(t.length));
            // The system moves the position on by itself at this rate.
            put(MPNowPlayingInfoPropertyElapsedPlaybackTime, &NSNumber::new_f64(position));
            let playing = state.status == Status::Playing;
            put(MPNowPlayingInfoPropertyPlaybackRate, &NSNumber::new_f64(if playing { 1.0 } else { 0.0 }));
            if let Some((_, art)) = self.art.as_ref().filter(|(url, _)| Some(url) == t.art_url.as_ref()) {
                put(MPMediaItemPropertyArtwork, art);
            }
            center.setNowPlayingInfo(Some(&info));
            let commands = MPRemoteCommandCenter::sharedCommandCenter();
            commands.changeRepeatModeCommand().setCurrentRepeatType(match state.repeat {
                Repeat::Off => MPRepeatType::Off,
                Repeat::All => MPRepeatType::All,
                Repeat::One => MPRepeatType::One,
            });
            let shuffle = if state.shuffle { MPShuffleType::Items } else { MPShuffleType::Off };
            commands.changeShuffleModeCommand().setCurrentShuffleType(shuffle);
            center.setPlaybackState(match state.status {
                Status::Playing => MPNowPlayingPlaybackState::Playing,
                Status::Paused => MPNowPlayingPlaybackState::Paused,
                Status::Stopped => MPNowPlayingPlaybackState::Stopped,
            });
        }
    }
}

impl Drop for NowPlaying {
    fn drop(&mut self) {
        // SAFETY: plain setters of the default center.
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            center.setNowPlayingInfo(None);
            center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }
}

/// Downloads a cover in the background; it arrives through `tx`.
fn fetch_art(url: String, tx: Sender<(String, Vec<u8>)>) {
    let _ = std::thread::Builder::new().name("cover".into()).spawn(move || {
        let data = NSURL::URLWithString(&NSString::from_str(&url)).and_then(|u| NSData::dataWithContentsOfURL(&u));
        if let Some(data) = data {
            let _ = tx.send((url, data.to_vec()));
        }
    });
}

fn artwork(bytes: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes))?;
    let size = image.size();
    let handler = RcBlock::new(move |_: CGSize| NonNull::from(&*image));
    // SAFETY: the handler returns the image it holds, alive as long as the handler is.
    Some(unsafe { MPMediaItemArtwork::initWithBoundsSize_requestHandler(MPMediaItemArtwork::alloc(), size, &handler) })
}
