//! The play queue: its tracks and position, where they come from, the repeat mode and random
//! order. Only the order of things — nothing here talks to the player, the network or the screen.

use ymusic_api::{random_below, StreamProgress, Track};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Where the queue comes from.
#[derive(PartialEq, Eq, Debug, Default)]
pub enum Source {
    #[default]
    None,
    Wave,
    List(String),
}

/// Repeat mode (`T`): what plays after a track that has ended. Stored in the config as `off`,
/// `all` or `one`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    /// After the last track of the list — the first one again (a wave has no end).
    All,
    /// The same track again; `n` still moves on.
    One,
}

impl Repeat {
    pub const ALL: [Repeat; 3] = [Repeat::Off, Repeat::All, Repeat::One];

    pub fn name(self) -> &'static str {
        match self {
            Repeat::Off => "выкл",
            Repeat::All => "вся очередь",
            Repeat::One => "один трек",
        }
    }

    /// The next mode in the `T` cycle.
    pub fn next(self) -> Repeat {
        let i = Repeat::ALL.iter().position(|r| *r == self).unwrap_or(0);
        Repeat::ALL[(i + 1) % Repeat::ALL.len()]
    }
}

#[derive(Default)]
pub struct Queue {
    tracks: Vec<Track>,
    pos: Option<usize>,
    source: Source,
    repeat: Repeat,
    /// The list plays in random order: its own order, to go back to.
    unshuffled: Option<Vec<Track>>,
    /// The first track is the one that was playing when the list was switched with
    /// «Доигрывать трек» on: it belongs to the previous list.
    carried: bool,
}

impl Queue {
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Keep resume state coherent when restoring the list's original order.
    pub fn set_stream_progress(&mut self, id: &str, progress: StreamProgress) {
        for track in self.tracks.iter_mut().chain(self.unshuffled.iter_mut().flatten()) {
            if track.id == id { track.progress = Some(progress.clone()); }
        }
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn pos(&self) -> Option<usize> {
        self.pos
    }

    pub fn current(&self) -> Option<&Track> {
        self.pos.and_then(|p| self.tracks.get(p))
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn is_wave(&self) -> bool {
        self.source == Source::Wave
    }

    pub fn is_list(&self) -> bool {
        matches!(self.source, Source::List(_))
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    pub fn shuffled(&self) -> bool {
        self.unshuffled.is_some()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tracks.iter().any(|t| t.id == id)
    }

    /// Whether track `i` is one carried over from the previous list.
    pub fn is_carried(&self, i: usize) -> bool {
        i == 0 && self.carried
    }

    /// The carried-over track is playing.
    pub fn on_carried(&self) -> bool {
        self.pos.is_some_and(|p| self.is_carried(p))
    }

    /// A new queue: nothing plays from it yet.
    pub fn replace(&mut self, source: Source, tracks: Vec<Track>) {
        *self = Queue { tracks, source, repeat: self.repeat, ..Queue::default() };
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    /// The list being played is shown in random order; `own` is its order.
    pub fn keep_own_order(&mut self, own: Vec<Track>) {
        self.unshuffled = Some(own);
    }

    /// Replaces the tracks but keeps the playing one as the first entry, so the new list goes on
    /// from there when it ends. False if nothing plays. A track already carried stays so: it still
    /// belongs to its first list (`was_carried` tells the caller which).
    pub fn after_current(&mut self, source: Source, tracks: Vec<Track>) -> Option<AfterCurrent> {
        let cur = self.current()?.clone();
        let was_carried = self.on_carried();
        self.tracks = std::iter::once(cur.clone()).chain(tracks.into_iter().filter(|t| t.id != cur.id)).collect();
        self.pos = Some(0);
        self.source = source;
        self.unshuffled = None;
        self.carried = true;
        Some(AfterCurrent { was_carried })
    }

    /// Track `i` becomes the current one.
    pub fn select(&mut self, i: usize) {
        self.pos = Some(i);
    }

    /// Nothing is current any more (the queue ran out).
    pub fn stop(&mut self) {
        self.pos = None;
    }

    /// The track to play after the current one: the next available one, or with repeat the same
    /// one when it has `ended` (`Repeat::One`) or the first one after the last (`Repeat::All`).
    /// `more_coming`: the list will grow (later search pages), so it has not ended yet.
    pub fn following(&self, ended: bool, more_coming: bool) -> Option<usize> {
        let available = |i: usize| self.tracks[i].available;
        if self.repeat == Repeat::One && ended && self.pos.is_some_and(available) {
            return self.pos;
        }
        let from = self.pos.map_or(0, |p| p + 1);
        if let Some(i) = (from..self.tracks.len()).find(|&i| available(i)) {
            return Some(i);
        }
        // A track carried over from another list is not part of this one.
        if self.repeat == Repeat::All && self.is_list() && !more_coming {
            return (0..self.tracks.len()).find(|&i| available(i) && !self.is_carried(i));
        }
        None
    }

    /// The available track before the current one.
    pub fn previous(&self) -> Option<usize> {
        let p = self.pos?;
        (0..p).rev().find(|&i| self.tracks[i].available)
    }

    /// Appends the tracks not in the queue yet (a wave's next batch, a search page); in random
    /// order they are shuffled in, and the list's own order gets them too.
    pub fn append(&mut self, tracks: Vec<Track>) {
        let mut known: HashSet<String> = self.tracks.iter().map(|t| t.id.clone()).collect();
        let mut new: Vec<Track> = tracks.into_iter().filter(|t| known.insert(t.id.clone())).collect();
        if let Some(own) = &mut self.unshuffled {
            own.extend(new.iter().cloned());
            shuffle(&mut new);
        }
        self.tracks.extend(new);
    }

    /// Bound a wave's old history; keeps `keep` predecessors for Previous. Lists are untouched.
    pub fn trim_history(&mut self, keep: usize) -> usize {
        if !self.is_wave() { return 0; }
        let removed = self.pos.unwrap_or(0).saturating_sub(keep);
        if removed > 0 {
            self.tracks.drain(..removed);
            self.pos = self.pos.map(|p| p - removed);
            self.carried = false;
        }
        removed
    }

    /// Random order turned on or off from the system (MPRIS, Now Playing): the playing list is
    /// shuffled with the current track first, or put back in its own order; the current track
    /// goes on. False if nothing changed (already so, or no list plays).
    pub fn set_shuffled(&mut self, on: bool) -> bool {
        if on == self.shuffled() {
            return false;
        }
        let Some(pos) = self.pos.filter(|_| self.is_list()) else { return false };
        let cur = self.tracks[pos].clone();
        if on {
            // A track carried over from another list stays first; any other moves there now.
            if pos != 0 {
                self.carried = false;
            }
            self.unshuffled = Some(self.tracks.clone());
            self.tracks = shuffled_from(std::mem::take(&mut self.tracks), pos);
            self.pos = Some(0);
        } else if let Some(own) = self.unshuffled.take() {
            match own.iter().position(|t| t.id == cur.id).filter(|_| !self.is_carried(pos)) {
                Some(i) => {
                    self.tracks = own;
                    self.pos = Some(i);
                }
                // Carried over from another list: it stays first, the list follows in its order.
                None => {
                    let id = cur.id.clone();
                    self.tracks = std::iter::once(cur).chain(own.into_iter().filter(|t| t.id != id)).collect();
                    self.pos = Some(0);
                }
            }
        }
        true
    }
}

/// What `Queue::after_current` found.
pub struct AfterCurrent {
    /// The playing track was carried over already (switched again before it ended).
    pub was_carried: bool,
}

/// `tracks` in random order, but with track `first` first.
pub fn shuffled_from(mut tracks: Vec<Track>, first: usize) -> Vec<Track> {
    if first < tracks.len() {
        let t = tracks.remove(first);
        tracks.insert(0, t);
        shuffle(&mut tracks[1..]);
    }
    tracks
}

/// Fisher–Yates: every order equally likely.
pub fn shuffle(list: &mut [Track]) {
    for i in (1..list.len()).rev() {
        list.swap(i, random_below(i + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: String::new(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: None,
            batch: None,
            kind: ymusic_api::TrackKind::Music,
            progress: None,
            published: None,
        }
    }

    fn list(ids: &[&str]) -> Queue {
        let mut q = Queue::default();
        q.replace(Source::List("list".into()), ids.iter().map(|id| track(id)).collect());
        q
    }

    fn ids(q: &Queue) -> Vec<&str> {
        q.tracks().iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn following_and_repeat() {
        let mut q = list(&["a", "b", "c"]);
        q.tracks[1].available = false;
        assert_eq!(q.following(true, false), Some(0), "nothing plays: the first one");
        q.select(0);
        assert_eq!(q.following(true, false), Some(2), "unavailable ones are skipped");
        assert_eq!(q.previous(), None);
        q.select(2);
        assert_eq!(q.previous(), Some(0));
        assert_eq!(q.following(true, false), None);
        q.set_repeat(Repeat::All);
        assert_eq!(q.following(true, false), Some(0));
        assert_eq!(q.following(true, true), None, "more pages come first");
        q.set_repeat(Repeat::One);
        assert_eq!(q.following(true, false), Some(2));
        assert_eq!(q.following(false, false), None, "a skip moves on");
        // A wave has no end to start over from.
        q.replace(Source::Wave, vec![track("w")]);
        assert_eq!(q.repeat(), Repeat::One, "the mode outlives the queue");
        q.set_repeat(Repeat::All);
        q.select(0);
        assert_eq!(q.following(true, false), None);
    }

    #[test]
    fn carried_track() {
        let mut q = list(&["a", "b"]);
        assert!(q.after_current(Source::List("new".into()), vec![track("x")]).is_none(), "nothing plays");
        q.select(1);
        let after = q.after_current(Source::List("new".into()), vec![track("b"), track("x"), track("y")]).unwrap();
        assert!(!after.was_carried);
        assert_eq!(ids(&q), ["b", "x", "y"], "the playing track first, not twice");
        assert!(q.on_carried());
        // Switched again before it ended: still carried from its first list.
        assert!(q.after_current(Source::List("third".into()), vec![track("z")]).unwrap().was_carried);
        // Repeat starts the list over without the carried track.
        q.set_repeat(Repeat::All);
        q.select(1);
        assert_eq!(q.following(true, false), Some(1));
    }

    #[test]
    fn shuffle_on_and_off() {
        let names: Vec<String> = (0..30).map(|i| i.to_string()).collect();
        let mut q = list(&names.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(!q.set_shuffled(true), "nothing plays");
        q.select(5);
        assert!(q.set_shuffled(true));
        assert!(!q.set_shuffled(true), "already on");
        assert_eq!((q.pos(), q.current().unwrap().id.as_str()), (Some(0), "5"));
        let mut sorted = ids(&q);
        sorted.sort_by_key(|s| s.parse::<u32>().unwrap());
        assert_eq!(sorted, names);
        // New tracks join both orders.
        q.append(vec![track("1"), track("new")]);
        assert_eq!(q.len(), 31);
        q.select(3);
        let playing = q.current().unwrap().id.clone();
        assert!(q.set_shuffled(false));
        assert_eq!(q.current().unwrap().id, playing, "the current track goes on");
        assert_eq!(ids(&q)[30], "new");
        assert_eq!(&ids(&q)[..3], ["0", "1", "2"]);
        // Not for a wave.
        q.replace(Source::Wave, vec![track("w")]);
        q.select(0);
        assert!(!q.set_shuffled(true));
    }

    #[test]
    fn shuffle_keeps_a_carried_track_first() {
        let mut q = list(&["a", "b"]);
        q.select(0);
        q.after_current(Source::List("new".into()), vec![track("x"), track("y"), track("z")]);
        assert!(q.set_shuffled(true));
        assert_eq!(q.current().unwrap().id, "a");
        assert!(q.on_carried());
        assert!(q.set_shuffled(false));
        assert_eq!(ids(&q), ["a", "x", "y", "z"], "it stays first, the list follows in its order");
    }

    #[test]
    fn wave_history_is_bounded_and_previous_still_works() {
        let mut q = Queue::default();
        q.replace(Source::Wave, (0..1100).map(|i| track(&i.to_string())).collect());
        q.select(1050);
        assert_eq!(q.trim_history(1000), 50);
        assert_eq!(q.current().unwrap().id, "1050");
        assert_eq!(q.previous(), Some(999));
        assert_eq!(q.tracks()[999].id, "1049");
        q.append(vec![track("1100"), track("1100")]);
        assert_eq!(q.len(), 1051);
    }

    #[test]
    fn repeat_cycle() {
        assert_eq!(Repeat::Off.next(), Repeat::All);
        assert_eq!(Repeat::All.next(), Repeat::One);
        assert_eq!(Repeat::One.next(), Repeat::Off);
    }
}
