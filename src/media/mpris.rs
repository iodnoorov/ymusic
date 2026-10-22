//! MPRIS (`org.mpris.MediaPlayer2`) on the session D-Bus: media keys and the player widgets of
//! desktop environments control the program and show what plays. The connection runs in a
//! thread of its own; the app hands over its state as it changes, and the commands come back
//! to it as messages.

use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use zbus::object_server::SignalEmitter;
use zbus::{ObjectPath, OwnedVariant, Variant};

use super::{Clock, Remote, State, Status};

const PATH: &str = "/org/mpris/MediaPlayer2";
const NAME: &str = "org.mpris.MediaPlayer2.ymusic";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

/// D-Bus object path of a track (only letters, digits and `_` are allowed in its elements).
fn track_path(id: &str) -> String {
    let id: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("/org/ymusic/track/t{id}")
}

/// State as the interfaces read it.
#[derive(Default)]
struct Current {
    state: State,
    /// Seconds into the track.
    position: f64,
}

type Send = Arc<dyn Fn(Remote) + std::marker::Send + Sync>;

/// Changes on their way to the D-Bus thread, whose future sleeps until there are some.
#[derive(Default)]
struct Inbox(Mutex<(VecDeque<Update>, Option<Waker>, bool)>);

enum Update {
    Changed { track: bool, status: bool, volume: bool },
    Seeked(f64),
}

impl Inbox {
    /// Queues an update; dropped once the D-Bus thread is gone (no session bus, say).
    fn push(&self, u: Update) {
        let mut q = self.0.lock().unwrap();
        if q.2 {
            return;
        }
        q.0.push_back(u);
        if let Some(w) = q.1.take() {
            w.wake();
        }
    }

    fn close(&self) {
        let mut q = self.0.lock().unwrap();
        q.2 = true;
        if let Some(w) = q.1.take() {
            w.wake();
        }
    }

    /// The next update; `None` once closed.
    async fn next(&self) -> Option<Update> {
        poll_fn(|cx| {
            let mut q = self.0.lock().unwrap();
            if let Some(u) = q.0.pop_front() {
                return Poll::Ready(Some(u));
            }
            if q.2 {
                return Poll::Ready(None);
            }
            q.1 = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

pub struct Mpris {
    inbox: Arc<Inbox>,
    current: Arc<Mutex<Current>>,
    last: State,
    clock: Clock,
}

impl Mpris {
    /// Starts serving on the session bus; without one the desktop just does not see the player.
    pub fn start(send: impl Fn(Remote) + std::marker::Send + Sync + 'static) -> Mpris {
        let inbox = Arc::new(Inbox::default());
        let current = Arc::new(Mutex::new(Current::default()));
        let (i, c, s): (_, _, Send) = (inbox.clone(), current.clone(), Arc::new(send));
        let _ = std::thread::Builder::new().name("mpris".into()).spawn(move || {
            let _ = zbus::block_on(serve(i.clone(), c, s));
            i.close();
        });
        Mpris { inbox, current, last: State::default(), clock: Clock::default() }
    }

    /// Takes the app's state and position (seconds), tells the desktop what changed.
    pub fn sync(&mut self, state: State, position: f64) {
        let same_track = state.track.as_ref().map(|t| &t.id) == self.last.track.as_ref().map(|t| &t.id);
        if self.clock.seeked(position, self.last.status == Status::Playing, same_track) {
            self.inbox.push(Update::Seeked(position));
        }
        let mut cur = self.current.lock().unwrap();
        cur.position = position;
        if state != self.last {
            let changed = Update::Changed {
                track: state.track != self.last.track,
                status: state.status != self.last.status,
                volume: state.volume != self.last.volume,
            };
            cur.state = state.clone();
            self.last = state;
            self.inbox.push(changed);
        }
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        self.inbox.close();
    }
}

async fn serve(inbox: Arc<Inbox>, current: Arc<Mutex<Current>>, send: Send) -> zbus::Result<()> {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    let conn = zbus::connection::Builder::session()
        .serve_at(PATH, Root { send: send.clone() })
        .serve_at(PATH, Player { current, send })
        .build()
        .await?;
    // A second copy of the program takes a name of its own, as the specification suggests.
    let owned = |r: zbus::Result<RequestNameReply>| matches!(r, Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner));
    if !owned(conn.request_name_with_flags(NAME, RequestNameFlags::DO_NOT_QUEUE).await) {
        let name = format!("{NAME}.instance{}", std::process::id());
        conn.request_name_with_flags(name.as_str(), RequestNameFlags::DO_NOT_QUEUE).await?;
    }
    let player = conn.object_server().interface::<_, Player>(PATH).await?;
    while let Some(update) = inbox.next().await {
        let emitter = player.signal_emitter();
        match update {
            Update::Changed { track, status, volume } => {
                let p = player.get().await;
                if track {
                    p.metadata_changed(emitter).await?;
                }
                if track || status {
                    p.playback_status_changed(emitter).await?;
                    p.can_play_changed(emitter).await?;
                    p.can_pause_changed(emitter).await?;
                    p.can_seek_changed(emitter).await?;
                    p.can_go_next_changed(emitter).await?;
                    p.can_go_previous_changed(emitter).await?;
                }
                if volume {
                    p.volume_changed(emitter).await?;
                }
            }
            Update::Seeked(pos) => Player::seeked(emitter, (pos * 1e6) as i64).await?,
        }
    }
    Ok(())
}

struct Root {
    send: Send,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {
        (self.send)(Remote::Quit);
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        "ymusic".into()
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    current: Arc<Mutex<Current>>,
    send: Send,
}

fn owned<'a>(v: impl Into<Variant<'a>>) -> OwnedVariant {
    // Only values carrying file descriptors fail to convert.
    v.into().try_into().expect("no file descriptors here")
}

impl Player {
    fn has_track(&self) -> bool {
        self.current.lock().unwrap().state.track.is_some()
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        (self.send)(Remote::Next);
    }

    fn previous(&self) {
        (self.send)(Remote::Previous);
    }

    fn pause(&self) {
        (self.send)(Remote::Pause);
    }

    fn play_pause(&self) {
        (self.send)(Remote::PlayPause);
    }

    /// The program has no «stopped with a track» state: stopping pauses.
    fn stop(&self) {
        (self.send)(Remote::Pause);
    }

    fn play(&self) {
        (self.send)(Remote::Play);
    }

    fn seek(&self, offset: i64) {
        (self.send)(Remote::Seek(offset as f64 / 1e6));
    }

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        let current = self.current.lock().unwrap().state.track.as_ref().map(|t| track_path(&t.id));
        // A request for another track than the current one is ignored, as the specification says.
        if current.as_deref() == Some(track_id.as_str()) && position >= 0 {
            (self.send)(Remote::SeekTo(position as f64 / 1e6));
        }
    }

    fn open_uri(&self, _uri: String) {}

    #[zbus(property)]
    fn playback_status(&self) -> String {
        match self.current.lock().unwrap().state.status {
            Status::Playing => "Playing",
            Status::Paused => "Paused",
            Status::Stopped => "Stopped",
        }
        .into()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedVariant> {
        let cur = self.current.lock().unwrap();
        let mut m = HashMap::new();
        let Some(t) = &cur.state.track else {
            m.insert("mpris:trackid".into(), owned(ObjectPath::from_static_str_unchecked(NO_TRACK)));
            return m;
        };
        if let Ok(path) = ObjectPath::try_from(track_path(&t.id)) {
            m.insert("mpris:trackid".into(), owned(path));
        }
        m.insert("mpris:length".into(), owned((t.length * 1e6) as i64));
        m.insert("xesam:title".into(), owned(t.title.as_str()));
        m.insert("xesam:artist".into(), owned(vec![t.artists.as_str()]));
        m.insert("xesam:album".into(), owned(t.album.as_str()));
        m.insert("xesam:url".into(), owned(t.url.as_str()));
        if let Some(art) = &t.art_url {
            m.insert("mpris:artUrl".into(), owned(art.as_str()));
        }
        m
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.current.lock().unwrap().state.volume / 100.0
    }

    #[zbus(property)]
    fn set_volume(&mut self, v: f64) {
        (self.send)(Remote::Volume(v.max(0.0) * 100.0));
    }

    /// Asked for when needed (the desktop counts it on itself between `Seeked` signals).
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        (self.current.lock().unwrap().position * 1e6) as i64
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.has_track()
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.has_track()
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.has_track()
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.has_track()
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.has_track()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TrackInfo;
    use std::time::Instant;

    fn state(id: &str, status: Status) -> State {
        let track = TrackInfo {
            id: id.into(),
            title: "t".into(),
            artists: "a".into(),
            album: "al".into(),
            length: 200.0,
            art_url: None,
            url: String::new(),
        };
        State { track: Some(track), status, volume: 50.0 }
    }

    /// The tracking part alone, without the D-Bus thread.
    fn mpris() -> Mpris {
        Mpris {
            inbox: Arc::new(Inbox::default()),
            current: Arc::new(Mutex::new(Current::default())),
            last: State::default(),
            clock: Clock::default(),
        }
    }

    fn drain(m: &Mpris) -> Vec<String> {
        let mut q = m.inbox.0.lock().unwrap();
        q.0.drain(..)
            .map(|u| match u {
                Update::Changed { track, status, volume } => format!("changed {track} {status} {volume}"),
                Update::Seeked(p) => format!("seeked {p}"),
            })
            .collect()
    }

    #[test]
    fn changes_and_seeks() {
        let mut m = mpris();
        m.sync(state("1", Status::Playing), 0.0);
        assert_eq!(drain(&m), ["changed true true true"]);
        // Playback moving on is not a seek; the same state is not told again.
        m.sync(state("1", Status::Playing), 0.25);
        assert!(drain(&m).is_empty());
        // A jump is.
        m.sync(state("1", Status::Playing), 10.25);
        assert_eq!(drain(&m), ["seeked 10.25"]);
        // A stall (the position stood still for 3 s) is not.
        m.clock.at = Instant::now() - std::time::Duration::from_secs(3);
        m.sync(state("1", Status::Playing), 10.5);
        assert!(drain(&m).is_empty());
        m.sync(state("1", Status::Paused), 10.5);
        assert_eq!(drain(&m), ["changed false true false"]);
        // A new track starting from 0 is not a seek.
        m.sync(state("2", Status::Playing), 0.0);
        assert_eq!(drain(&m), ["changed true true false"]);
        assert_eq!(m.current.lock().unwrap().position, 0.0);
    }

    #[test]
    fn closed_inbox_drops_updates() {
        let m = mpris();
        m.inbox.close();
        m.inbox.push(Update::Seeked(1.0));
        assert!(drain(&m).is_empty());
    }

    #[test]
    fn track_paths() {
        assert_eq!(track_path("39089632"), "/org/ymusic/track/t39089632");
        assert_eq!(track_path("bfc384d6-bd20"), "/org/ymusic/track/tbfc384d6_bd20");
        assert!(ObjectPath::try_from(track_path("a:b-c")).is_ok());
    }
}
