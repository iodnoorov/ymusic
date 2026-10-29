//! The account's lists as the program knows them: «Мне нравится», the playlists and the search.
//! Each keeps what was loaded and what is loading, and drops answers that came too late; the
//! requests themselves are made by the app (`Task`).

use ymusic_api::{Playlist, Track};
use std::collections::HashSet;

/// «Мне нравится».
#[derive(Default)]
pub struct Likes {
    /// Every liked track (books and podcasts too): for the ♥ marks.
    ids: HashSet<String>,
    /// The music among them, newest first.
    tracks: Vec<Track>,
    loading: bool,
    /// Likes changed while loading: applied again over the server's answer.
    pending: Vec<(Track, bool)>,
}

impl Likes {
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    /// A load starts; false if one is already under way (holding `R` would start one per key
    /// repeat).
    pub fn begin_load(&mut self) -> bool {
        !std::mem::replace(&mut self.loading, true)
    }

    /// The track is liked or not any more.
    pub fn set(&mut self, track: &Track, like: bool) {
        if like {
            self.ids.insert(track.id.clone());
            if !self.tracks.iter().any(|t| t.id == track.id) {
                self.tracks.insert(0, track.clone());
            }
        } else {
            self.ids.remove(&track.id);
            self.tracks.retain(|t| t.id != track.id);
        }
        if self.loading {
            self.pending.push((track.clone(), like));
        }
    }

    /// Tests: these tracks are liked, loaded.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_loaded(&mut self, tracks: Vec<Track>) {
        self.ids = tracks.iter().map(|t| t.id.clone()).collect();
        self.tracks = tracks;
        self.loading = false;
    }

    /// The ids came (`id:album` or `id`), before the tracks.
    pub fn ids_loaded(&mut self, ids: &[String]) {
        self.ids = ids.iter().map(|i| i.split(':').next().unwrap_or(i).to_string()).collect();
        for (t, like) in &self.pending {
            if *like {
                self.ids.insert(t.id.clone());
            } else {
                self.ids.remove(&t.id);
            }
        }
    }

    /// The load is over.
    pub fn loaded<E>(&mut self, result: Result<Vec<Track>, E>) -> Result<(), E> {
        self.loading = false;
        let pending = std::mem::take(&mut self.pending);
        self.tracks = result?;
        for (t, like) in &pending {
            self.set(t, *like);
        }
        Ok(())
    }
}

/// The search tab: the query typed, its results and the pages still to load.
#[derive(Default)]
pub struct Search {
    pub input: String,
    /// The query field takes the keys.
    pub editing: bool,
    /// The query the results are for.
    query: String,
    generation: u64,
    results: Vec<Track>,
    loading: bool,
    /// Pages loaded so far and the number of tracks found (`None`: not known yet — the website
    /// API tells it only on the last page).
    pages: usize,
    total: Option<usize>,
    /// The queue is played from these results: pages loaded later are appended to it, and playing
    /// close to its end loads the next page.
    pub feeds_queue: bool,
}

/// What a search page brought; `E`: the error it may have failed with.
pub enum Page<E> {
    /// An answer to an older query or a page no longer expected.
    Stale,
    Failed(E),
    /// `new`: the tracks not among the results yet; `first`: page 0 of a new query.
    Loaded { first: bool, new: Vec<Track> },
}

impl Search {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn results(&self) -> &[Track] {
        &self.results
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn total(&self) -> Option<usize> {
        self.total
    }

    /// The typed query is searched for; false if nothing is typed. The playing queue keeps the
    /// old results but no longer grows.
    pub fn submit(&mut self) -> bool {
        self.editing = false;
        let q = self.input.trim();
        if q.is_empty() {
            return false;
        }
        self.generation += 1;
        self.query = q.to_string();
        self.pages = 0;
        self.total = None;
        self.feeds_queue = false;
        true
    }

    /// The next page to ask for: (query, page).
    pub fn next_page(&mut self) -> (String, usize) {
        self.loading = true;
        (self.query.clone(), self.pages)
    }

    /// There are pages not loaded yet and none is loading now.
    pub fn can_load_more(&self) -> bool {
        !self.loading && self.pages > 0 && self.total.is_none_or(|t| self.results.len() < t)
    }

    /// Page `page` of `query` came: (tracks, total found).
    pub fn page_loaded<E>(&mut self, generation: u64, query: &str, page: usize, result: Result<(Vec<Track>, Option<usize>), E>) -> Page<E> {
        if generation != self.generation || query != self.query || page != self.pages {
            return Page::Stale;
        }
        self.loading = false;
        let (list, total) = match result {
            Ok(r) => r,
            Err(e) => return Page::Failed(e),
        };
        self.pages += 1;
        self.total = total;
        if page == 0 {
            self.results.clear();
        }
        // Pages may overlap if the results shift between requests.
        let seen: HashSet<&str> = self.results.iter().map(|t| t.id.as_str()).collect();
        let new: Vec<Track> = list.into_iter().filter(|t| !seen.contains(t.id.as_str())).collect();
        if new.is_empty() && page > 0 {
            // Nothing new: stop asking for more pages.
            self.total = Some(self.results.len());
        }
        self.results.extend(new.iter().cloned());
        Page::Loaded { first: page == 0, new }
    }
}

/// Playlist opened in the «Плейлисты» tab.
pub struct OpenPlaylist {
    pub pl: Playlist,
    pub tracks: Vec<Track>,
    pub loading: bool,
    /// Number of the latest track request: replies to older ones are dropped.
    pub req: u64,
}

/// The account's playlists.
#[derive(Default)]
pub struct Playlists {
    list: Vec<Playlist>,
    loading: bool,
    /// Number of the latest request: an older reply (e.g. from `R` before an edit) is dropped.
    req: u64,
    pub open: Option<OpenPlaylist>,
    open_req: u64,
    /// Key of the playlist the queue was started from.
    pub playing: Option<String>,
}

impl Playlists {
    pub fn list(&self) -> &[Playlist] {
        &self.list
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    /// The account's own playlists (others' can be played, not changed).
    pub fn own(&self, uid: &str) -> Vec<Playlist> {
        self.list.iter().filter(|p| p.owner == uid).cloned().collect()
    }

    /// Tests: these playlists are loaded.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_loaded(&mut self, list: Vec<Playlist>) {
        self.list = list;
        self.loading = false;
    }

    /// A load starts; returns its number.
    pub fn begin_load(&mut self) -> u64 {
        self.loading = true;
        self.req += 1;
        self.req
    }

    /// Answer to load `req`; `None` if a newer load was started meanwhile. The open playlist
    /// follows (renamed), or is closed if it is gone.
    pub fn loaded<E>(&mut self, req: u64, result: Result<Vec<Playlist>, E>) -> Option<Result<(), E>> {
        if req != self.req {
            return None;
        }
        self.loading = false;
        let list = match result {
            Ok(list) => list,
            Err(e) => return Some(Err(e)),
        };
        self.list = list;
        if let Some(open) = &mut self.open {
            match self.list.iter().find(|p| p.key() == open.pl.key()) {
                Some(p) => open.pl = p.clone(),
                // Deleted (here or elsewhere): back to the list.
                None => self.open = None,
            }
        }
        Some(Ok(()))
    }

    /// Opens `pl`: its tracks are to be loaded.
    pub fn open(&mut self, pl: Playlist) {
        self.open = Some(OpenPlaylist { pl, tracks: Vec::new(), loading: true, req: 0 });
    }

    /// A new load of the open playlist's tracks: (playlist, request number).
    pub fn reload_open(&mut self) -> Option<(Playlist, u64)> {
        let open = self.open.as_mut()?;
        open.loading = true;
        self.open_req += 1;
        open.req = self.open_req;
        Some((open.pl.clone(), open.req))
    }

    /// The tracks of playlist `key` came for request `req`; `None` if it is not open any more or a
    /// newer request was made. `Some(Ok(n))`: `n` tracks.
    pub fn open_loaded<E>(&mut self, key: &str, req: u64, result: Result<Vec<Track>, E>) -> Option<Result<usize, E>> {
        let open = self.open.as_mut().filter(|o| o.pl.key() == key && o.req == req)?;
        open.loading = false;
        Some(result.map(|tracks| {
            open.tracks = tracks;
            open.tracks.len()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track::from_json(&serde_json::json!({"id": id, "title": id})).unwrap()
    }

    fn ids(tracks: &[Track]) -> Vec<&str> {
        tracks.iter().map(|t| t.id.as_str()).collect()
    }

    /// Likes made while the list loads are not lost under the server's older answer.
    #[test]
    fn likes_during_a_load() {
        let mut likes = Likes::default();
        assert!(likes.begin_load());
        assert!(!likes.begin_load(), "already loading");
        likes.set(&track("5"), true);
        likes.set(&track("2"), false);
        likes.ids_loaded(&["1:10".into(), "2:20".into()]);
        assert!(likes.contains("5") && likes.contains("1") && !likes.contains("2"));
        likes.loaded::<String>(Ok(vec![track("1"), track("2")])).unwrap();
        assert_eq!(ids(likes.tracks()), ["5", "1"]);
        assert!(likes.begin_load());
        assert!(likes.loaded(Err("сеть")).is_err());
        assert_eq!(ids(likes.tracks()), ["5", "1"], "a failed load keeps the list");
    }

    #[test]
    fn search_pages() {
        let mut s = Search { input: "  queen ".into(), ..Search::default() };
        assert!(s.submit());
        assert_eq!(s.next_page(), ("queen".to_string(), 0));
        assert!(!s.can_load_more(), "loading");
        assert!(matches!(s.page_loaded::<()>(s.generation(), "other", 0, Ok((vec![track("x")], None))), Page::Stale));
        let Page::Loaded { first: true, new } = s.page_loaded::<()>(s.generation(), "queen", 0, Ok((vec![track("a"), track("b")], None))) else {
            panic!()
        };
        assert_eq!(ids(&new), ["a", "b"]);
        assert!(s.can_load_more());
        assert_eq!(s.next_page().1, 1);
        // Overlapping pages: only the new tracks; a page with nothing new ends the search.
        let Page::Loaded { new, .. } = s.page_loaded::<()>(s.generation(), "queen", 1, Ok((vec![track("b"), track("c")], None))) else { panic!() };
        assert_eq!(ids(&new), ["c"]);
        s.next_page();
        assert!(matches!(s.page_loaded::<()>(s.generation(), "queen", 2, Ok((vec![track("a")], None))), Page::Loaded { first: false, .. }));
        assert_eq!(s.total(), Some(3));
        assert!(!s.can_load_more());
        s.input.clear();
        assert!(!s.submit(), "nothing typed");
        assert_eq!(s.query(), "queen");
    }

    #[test]
    fn playlists() {
        let pl = |k: &str, title: &str| Playlist { owner: "1".into(), kind: k.into(), title: title.into(), track_count: 0 };
        let mut p = Playlists::default();
        let old = p.begin_load();
        let new = p.begin_load();
        assert!(p.loaded::<()>(old, Ok(vec![pl("old", "Old")])).is_none(), "a newer load was started");
        assert!(p.loading());
        p.loaded::<()>(new, Ok(vec![pl("3", "Mine"), Playlist { owner: "2".into(), ..pl("4", "Theirs") }])).unwrap().unwrap();
        assert_eq!(p.own("1").len(), 1);

        p.open(pl("3", "Mine"));
        let (open, req) = p.reload_open().unwrap();
        assert!(p.open_loaded::<()>(&open.key(), req - 1, Ok(vec![])).is_none(), "an older request");
        assert_eq!(p.open_loaded::<()>(&open.key(), req, Ok(vec![track("a")])), Some(Ok(1)));
        // Renamed elsewhere: the open one follows; deleted: it is closed.
        let r = p.begin_load();
        p.loaded::<()>(r, Ok(vec![pl("3", "Renamed")])).unwrap().unwrap();
        assert_eq!(p.open.as_ref().unwrap().pl.title, "Renamed");
        let r = p.begin_load();
        p.loaded::<()>(r, Ok(vec![])).unwrap().unwrap();
        assert!(p.open.is_none());
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    #[test]
    fn reopening_a_playlist_rejects_the_previous_opening() {
        let mut p = Playlists::default();
        let pl = Playlist { owner: "1".into(), kind: "1".into(), title: "p".into(), track_count: 0 };
        p.open(pl.clone());
        let (_, old) = p.reload_open().unwrap();
        p.open = None;
        p.open(pl.clone());
        let (_, new) = p.reload_open().unwrap();
        assert_ne!(old, new);
        assert_eq!(p.open_loaded::<()>(&pl.key(), new, Ok(vec![])), Some(Ok(0)));
        assert!(p.open_loaded::<()>(&pl.key(), old, Ok(vec![])).is_none());
    }
    #[test]
    fn resubmitting_the_same_search_rejects_the_previous_submission() {
        let mut s = Search { input: "q".into(), ..Search::default() };
        s.submit();
        let old = s.generation();
        s.next_page();
        s.input = "other".into();
        s.submit();
        s.next_page();
        s.input = "q".into();
        s.submit();
        s.next_page();
        assert!(matches!(s.page_loaded::<()>(old, "q", 0, Ok((vec![], None))), Page::Stale));
        assert!(s.loading());
        assert!(matches!(s.page_loaded::<()>(s.generation(), "q", 0, Ok((vec![], None))), Page::Loaded { .. }));
    }
}
