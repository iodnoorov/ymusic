//! Application state and logic.

use crate::api::{default_stations, iso_now, Account, Api, Link, Playlist, Station, Track, WaveBatch, MY_WAVE, QUALITIES};
use crate::player::{Audio, PlayerEvent};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub enum Msg {
    Player(PlayerEvent),
    Url { gen: u64, result: Result<String, String> },
    LikedIds(Vec<String>),
    Liked(Result<Vec<Track>, String>),
    Stations(Vec<Station>),
    /// Page `page` of the search for `query`: (tracks, total found).
    Search { query: String, page: usize, result: Result<(Vec<Track>, usize), String> },
    Playlists { req: u64, result: Result<Vec<Playlist>, String> },
    PlaylistOpened { key: String, req: u64, result: Result<Vec<Track>, String> },
    /// Result of renaming a playlist or changing its tracks: status text on success.
    PlaylistEdited { key: String, result: Result<String, String> },
    /// Tracks loaded in the background to be played as a list named `name`.
    ListLoaded { intent: u64, name: String, shuffle: bool, playlist: Option<String>, result: Result<Vec<Track>, String> },
    WaveStarted { intent: u64, station: String, name: String, result: Result<WaveBatch, String> },
    WaveMore(Result<WaveBatch, String>),
    LikeFailed { track: Track, like: bool, err: String },
    Cover { url: String, result: Result<image::DynamicImage, String> },
    Preload { gen: u64, idx: usize, id: String, result: Result<String, String> },
    /// «Меньше ИИ-музыки» loaded from the account.
    LessAi(Result<bool, String>),
    /// Result of switching «Меньше ИИ-музыки» to `on`.
    LessAiSet { on: bool, result: Result<(), String> },
    Status(String),
    /// A Yandex Music link found in the clipboard for «Играть по ссылке».
    ClipboardLink(String),
}

impl From<PlayerEvent> for Msg {
    fn from(e: PlayerEvent) -> Self {
        Msg::Player(e)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Wave,
    Stations,
    Liked,
    Search,
    Playlists,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Wave, Tab::Stations, Tab::Liked, Tab::Search, Tab::Playlists];
    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap()
    }
}

#[derive(PartialEq, Eq)]
pub enum Source {
    None,
    Wave,
    List(String),
}

pub struct Wave {
    pub station: String,
    pub name: String,
    pub session: Option<String>,
    pub batch: String,
    pub fetching: bool,
    pub pending_next: bool,
}

#[derive(Clone, Copy)]
enum End {
    Eof,
    Skip,
    Silent,
}

/// Playlist opened in the «Плейлисты» tab.
pub struct OpenPlaylist {
    pub pl: Playlist,
    pub tracks: Vec<Track>,
    pub loading: bool,
    /// Number of the latest track request: replies to older ones are dropped.
    pub req: u64,
}

/// Pop-up dialogs; while one is shown it receives all keys.
pub enum Modal {
    /// Choose a playlist to add `track` to.
    AddTo { track: Track, choices: Vec<Playlist>, sel: usize },
    ConfirmRemove { pl: Playlist, index: usize, track: Track },
    ConfirmDelete { pl: Playlist },
    Rename { pl: Playlist, input: String },
    /// Link to a track to play.
    OpenLink { input: String },
    /// Settings window; `sel` is an index into `Setting::ALL`.
    Settings { sel: usize },
    ConfirmLogout,
}

/// Rows of the settings window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Setting {
    Quality,
    Equalizer,
    Crossfade,
    LessAi,
    Icons,
    About,
    Logout,
}

impl Setting {
    pub const ALL: [Setting; 7] = [
        Setting::Quality,
        Setting::Equalizer,
        Setting::Crossfade,
        Setting::LessAi,
        Setting::Icons,
        Setting::About,
        Setting::Logout,
    ];

    fn index(self) -> usize {
        Setting::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// Loudest volume, % (mpv's default `volume-max`); kept between runs as is.
const MAX_VOLUME: f64 = 130.0;

/// Fade lengths offered in the settings window, seconds (0 — off).
pub const CROSSFADE_STEPS: [u8; 6] = [0, 3, 5, 8, 12, 15];

pub enum StationRow {
    Header(String),
    Station(Station),
}

pub struct App {
    pub api: Api,
    tx: Sender<Msg>,
    player: Box<dyn Audio>,
    /// Tests: background jobs (network requests) are not run.
    offline: bool,
    pub account: Account,
    pub tab: Tab,
    pub quit: bool,
    /// Why the program quit on its own: printed after the terminal is restored.
    pub exit_error: Option<String>,
    pub show_help: bool,
    pub show_about: bool,
    /// «О программе» was opened from the settings window: closing it returns there.
    about_from_settings: bool,
    /// The user chose «Выйти из аккаунта»: the token is removed on exit.
    pub logout: bool,

    pub queue: Vec<Track>,
    pub pos: Option<usize>,
    pub source: Source,
    pub wave: Option<Wave>,
    pub queue_state: TableState,
    gen: u64,
    /// Bumped whenever the user starts something to play: replies to an older request are dropped.
    intent: u64,
    errors_in_row: u32,
    pub loading_track: bool,

    pub liked_ids: HashSet<String>,
    pub liked: Vec<Track>,
    pub liked_loading: bool,
    /// Likes changed while «Мне нравится» is loading: applied again over the server's answer.
    liked_pending: Vec<(Track, bool)>,
    pub liked_state: TableState,

    pub stations: Vec<StationRow>,
    pub stations_state: ListState,

    pub search_input: String,
    pub search_editing: bool,
    pub search_results: Vec<Track>,
    pub search_loading: bool,
    /// Last submitted query, pages loaded so far and the total number of found tracks.
    search_query: String,
    search_pages: usize,
    pub search_total: usize,
    /// The queue is played from the current search (`Some(shuffled)`): pages loaded later are
    /// appended to it, and playing close to its end loads the next page.
    search_queue: Option<bool>,
    pub search_state: TableState,

    pub playlists: Vec<Playlist>,
    pub playlists_loading: bool,
    /// Number of the latest playlists request: an older reply (e.g. from `R` before an edit) is dropped.
    playlists_req: u64,
    pub playlists_state: TableState,
    /// Key of the playlist the queue was started from.
    pub playing_playlist: Option<String>,
    pub open_playlist: Option<OpenPlaylist>,
    pub open_state: TableState,
    pub modal: Option<Modal>,
    /// Current equalizer preset (index into `eq::PRESETS`).
    pub eq: usize,
    /// Sound quality (index into `api::QUALITIES`).
    pub quality: usize,
    /// Fade-in/fade-out length, seconds (0 — off).
    pub crossfade: u8,
    /// Nerd Font icons (false — plain Unicode symbols).
    pub icons: bool,
    /// «Меньше ИИ-музыки» from the account settings; `None` until loaded.
    pub less_ai: Option<bool>,

    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,

    pub status: String,
    status_at: Instant,
    /// Frame counter for animations.
    pub tick: u64,

    /// Terminal graphics (sixel / kitty / iTerm2 / half blocks); `None` in tests.
    pub picker: Option<Picker>,
    /// Recently used covers: (url, decoded image), newest last.
    pub covers: Vec<(String, image::DynamicImage)>,
    /// Cover on screen: (url, encoded image). Rebuilt when the cover changes or the terminal
    /// is resized, so kitty-protocol terminals always get the image data again: an image
    /// transmitted earlier may already be dropped by the terminal (Konsole drops them on resize).
    shown_cover: Option<(String, StatefulProtocol)>,
    covers_loading: HashSet<String>,

    /// Next track already resolved and appended to mpv's playlist: (queue index, track id).
    preload: Option<(usize, String)>,
    preload_pending: bool,
    /// Current track was started from the preloaded entry (its URL may have expired on error).
    from_preload: bool,
}

impl App {
    pub fn new(api: Api, account: Account, tx: Sender<Msg>, player: Box<dyn Audio>, volume: u8) -> App {
        let mut app = App {
            api,
            tx,
            player,
            offline: false,
            account,
            tab: Tab::Wave,
            quit: false,
            exit_error: None,
            show_help: false,
            show_about: false,
            about_from_settings: false,
            logout: false,
            queue: Vec::new(),
            pos: None,
            source: Source::None,
            wave: None,
            queue_state: TableState::default(),
            gen: 0,
            intent: 0,
            errors_in_row: 0,
            loading_track: false,
            liked_ids: HashSet::new(),
            liked: Vec::new(),
            liked_loading: false,
            liked_pending: Vec::new(),
            liked_state: TableState::default(),
            stations: Vec::new(),
            stations_state: ListState::default(),
            search_input: String::new(),
            search_editing: false,
            search_results: Vec::new(),
            search_loading: false,
            search_query: String::new(),
            search_pages: 0,
            search_total: 0,
            search_queue: None,
            search_state: TableState::default(),
            playlists: Vec::new(),
            playlists_loading: true,
            playlists_req: 0,
            playlists_state: TableState::default(),
            playing_playlist: None,
            open_playlist: None,
            open_state: TableState::default(),
            modal: None,
            eq: 0,
            quality: 1,
            crossfade: 0,
            icons: true,
            less_ai: None,
            time_pos: 0.0,
            duration: 0.0,
            paused: false,
            volume: f64::from(volume),
            muted: false,
            status: String::new(),
            status_at: Instant::now(),
            tick: 0,
            picker: None,
            covers: Vec::new(),
            shown_cover: None,
            covers_loading: HashSet::new(),
            preload: None,
            preload_pending: false,
            from_preload: false,
        };
        app.set_stations(default_stations());
        if !app.account.has_plus {
            app.set_status("Внимание: без подписки Плюс доступны только 30-секундные фрагменты");
        }
        app
    }

    /// Starts the initial loads: likes, stations, playlists, account settings.
    pub fn start(&mut self) {
        self.load_liked();
        self.load_stations();
        self.load_playlists();
        self.load_account_settings();
    }

    /// An app for tests: playback commands go to a recorder, no requests are made.
    #[cfg(test)]
    pub fn for_test() -> (App, crate::player::FakeAudio) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let audio = crate::player::FakeAudio::default();
        let acc = Account { uid: "1".into(), login: "tester".into(), name: "Tester".into(), has_plus: true };
        let mut api = Api::new("x");
        api.uid = "1".into();
        let mut app = App::new(api, acc, tx, Box::new(audio.clone()), 50);
        app.offline = true;
        (app, audio)
    }

    // ------------------------------------------------------------ helpers

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_at = Instant::now();
    }

    /// Whether the UI has something moving (progress, equalizer, spinners, cursor).
    pub fn is_animating(&self) -> bool {
        (self.pos.is_some() && !self.paused)
            || self.loading_track
            || self.search_editing
            || matches!(self.modal, Some(Modal::Rename { .. } | Modal::OpenLink { .. }))
            || self.search_loading
            || self.liked_loading
            || self.wave.as_ref().is_some_and(|w| w.fetching)
    }

    pub fn visible_status(&self) -> Option<&str> {
        (!self.status.is_empty() && self.status_at.elapsed().as_secs() < 6).then_some(self.status.as_str())
    }

    fn spawn<F>(&self, f: F)
    where
        F: FnOnce(Api) -> Option<Msg> + Send + 'static,
    {
        if self.offline {
            return;
        }
        let api = self.api.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Some(m) = f(api) {
                let _ = tx.send(m);
            }
        });
    }

    pub fn current(&self) -> Option<&Track> {
        self.pos.and_then(|p| self.queue.get(p))
    }

    pub fn is_liked(&self, t: &Track) -> bool {
        self.liked_ids.contains(&t.id)
    }

    pub fn source_name(&self) -> String {
        match (&self.source, &self.wave) {
            (Source::Wave, Some(w)) => w.name.clone(),
            (Source::List(n), _) => n.clone(),
            _ => String::new(),
        }
    }

    // ------------------------------------------------------------ loading

    /// A load already under way is not repeated (holding `R` would start one per key repeat).
    fn load_liked(&mut self) {
        if self.liked_loading {
            return;
        }
        self.liked_loading = true;
        let tx = self.tx.clone();
        self.spawn(move |api| {
            let ids = match api.liked_ids() {
                Ok(ids) => ids,
                Err(e) => return Some(Msg::Liked(Err(format!("{e:#}")))),
            };
            let _ = tx.send(Msg::LikedIds(ids.clone()));
            Some(Msg::Liked(api.tracks(&ids).map_err(|e| format!("{e:#}"))))
        });
    }

    fn load_playlists(&mut self) {
        self.playlists_loading = true;
        self.playlists_req += 1;
        let req = self.playlists_req;
        self.spawn(move |api| Some(Msg::Playlists { req, result: api.playlists().map_err(|e| format!("{e:#}")) }));
    }

    fn load_account_settings(&self) {
        self.spawn(|api| Some(Msg::LessAi(api.ai_reduction().map_err(|e| format!("{e:#}")))));
    }

    fn load_stations(&self) {
        self.spawn(|api| api.stations().ok().filter(|s| !s.is_empty()).map(Msg::Stations));
    }

    fn set_stations(&mut self, list: Vec<Station>) {
        let mut rows = Vec::new();
        let mut last_cat = String::new();
        for s in list {
            if s.category != last_cat {
                last_cat = s.category.clone();
                rows.push(StationRow::Header(last_cat.clone()));
            }
            rows.push(StationRow::Station(s));
        }
        self.stations = rows;
        let sel = self.stations_state.selected().unwrap_or(0).min(self.stations.len().saturating_sub(1));
        self.stations_state.select(Some(sel));
        self.fix_station_selection(1);
    }

    fn fix_station_selection(&mut self, dir: i32) {
        let n = self.stations.len();
        let mut i = self.stations_state.selected().unwrap_or(0) as i32;
        while i >= 0 && (i as usize) < n {
            if matches!(self.stations[i as usize], StationRow::Station(_)) {
                self.stations_state.select(Some(i as usize));
                return;
            }
            i += dir;
        }
        if dir > 0 {
            self.stations_state.select(Some(n.saturating_sub(1)));
            self.fix_station_selection(-1);
        }
    }

    // ------------------------------------------------------------ playback

    fn report_end(&mut self, end: End) {
        if self.source != Source::Wave {
            return;
        }
        let Some(t) = self.current().cloned() else { return };
        let ev = match end {
            End::Eof => json!({"type": "trackFinished", "timestamp": iso_now(), "trackId": t.full_id(),
                "totalPlayedSeconds": self.duration.max(self.time_pos)}),
            End::Skip => json!({"type": "skip", "timestamp": iso_now(), "trackId": t.full_id(),
                "totalPlayedSeconds": self.time_pos}),
            End::Silent => return,
        };
        self.wave_feedback(ev, t.batch.clone());
    }

    fn wave_feedback(&self, ev: Value, batch: Option<String>) {
        let Some(w) = &self.wave else { return };
        let (st, sess) = (w.station.clone(), w.session.clone());
        let batch = batch.unwrap_or_else(|| w.batch.clone());
        self.spawn(move |api| {
            let _ = api.wave_feedback(&st, sess.as_deref(), &batch, ev);
            None
        });
    }

    /// Starts track `idx`: resolves its URL and replaces mpv's playlist.
    fn play_index(&mut self, idx: usize) {
        if !self.begin_track(idx, false) {
            return;
        }
        let gen = self.gen;
        let id = self.queue[idx].full_id();
        let q = QUALITIES[self.quality].id;
        self.spawn(move |api| Some(Msg::Url { gen, result: api.track_url(&id, q).map_err(|e| format!("{e:#}")) }));
    }

    /// Updates state for a new current track. `preloaded`: mpv already switched to it.
    fn begin_track(&mut self, idx: usize, preloaded: bool) -> bool {
        let Some(track) = self.queue.get(idx).cloned() else { return false };
        self.pos = Some(idx);
        self.queue_state.select(Some(idx));
        self.gen += 1;
        self.time_pos = 0.0;
        self.duration = track.duration_ms as f64 / 1000.0;
        self.loading_track = true;
        self.preload = None;
        self.preload_pending = false;
        self.from_preload = preloaded;
        self.load_cover(track.cover.clone());
        self.maybe_extend_search_queue();
        if self.source == Source::Wave {
            self.wave_feedback(
                json!({"type": "trackStarted", "timestamp": iso_now(), "trackId": track.full_id()}),
                track.batch.clone(),
            );
            if idx + 2 >= self.queue.len() {
                self.fetch_more_wave();
            }
        }
        true
    }

    fn next_playable(&self) -> Option<usize> {
        let from = self.pos.map_or(0, |p| p + 1);
        (from..self.queue.len()).find(|&i| self.queue[i].available)
    }

    /// Resolves the next track's URL in advance and appends it to mpv's playlist.
    fn schedule_preload(&mut self) {
        if self.preload.is_some() || self.preload_pending || self.loading_track || self.pos.is_none() {
            return;
        }
        let Some(idx) = self.next_playable() else { return };
        let track = self.queue[idx].clone();
        self.preload_pending = true;
        self.load_cover(track.cover.clone());
        let gen = self.gen;
        let q = QUALITIES[self.quality].id;
        self.spawn(move |api| {
            let result = api.track_url(&track.full_id(), q).map_err(|e| format!("{e:#}"));
            Some(Msg::Preload { gen, idx, id: track.id, result })
        });
    }

    /// Switches to the preloaded track if it is the next one. Returns false if there is none.
    fn advance_preloaded(&mut self, end: End, mpv_advanced: bool) -> bool {
        let next = self.next_playable();
        match &self.preload {
            Some((idx, id)) if Some(*idx) == next && self.queue[*idx].id == *id => {
                let idx = *idx;
                self.report_end(end);
                if !mpv_advanced {
                    self.player.playlist_next();
                }
                self.begin_track(idx, true);
                true
            }
            _ => false,
        }
    }

    fn next(&mut self, end: End) {
        if self.advance_preloaded(end, false) {
            return;
        }
        self.report_end(end);
        let from = self.pos.map_or(0, |p| p + 1);
        if let Some(i) = (from..self.queue.len()).find(|&i| self.queue[i].available) {
            self.play_index(i);
        } else if self.source == Source::Wave {
            if let Some(w) = &mut self.wave {
                w.pending_next = true;
            }
            self.fetch_more_wave();
            self.player.stop();
            self.set_status("Загрузка следующих треков волны…");
        } else {
            self.player.stop();
            self.pos = None;
            self.set_status("Очередь закончилась");
        }
    }

    fn prev(&mut self) {
        if self.time_pos > 5.0 || self.pos == Some(0) {
            self.player.seek(-self.time_pos);
            return;
        }
        if let Some(p) = self.pos {
            if let Some(i) = (0..p).rev().find(|&i| self.queue[i].available) {
                self.report_end(End::Skip);
                self.play_index(i);
            }
        }
    }

    fn play_list(&mut self, name: &str, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.new_intent();
        self.report_end(End::Skip);
        self.queue = tracks;
        self.search_queue = None;
        self.preload = None;
        self.playing_playlist = None;
        self.source = Source::List(name.to_string());
        self.wave = None;
        self.pos = None;
        let start = (start..self.queue.len()).find(|&i| self.queue[i].available).unwrap_or(start);
        self.play_index(start);
        self.set_status(format!("Играет: {name}"));
    }

    /// The user started something new to play; returns its number for the async reply.
    fn new_intent(&mut self) -> u64 {
        self.intent += 1;
        self.intent
    }

    fn start_wave(&mut self, station: Station) {
        let intent = self.new_intent();
        let name = station.name.clone();
        self.set_status(format!("Запуск: {name}…"));
        let settings = station.settings;
        let is_my_wave = station.id == MY_WAVE;
        let id = station.id.clone();
        self.spawn(move |api| {
            if is_my_wave {
                let (mood, div) = settings.unwrap_or(("all", "default"));
                let _ = api.wave_settings(mood, div);
            }
            let result = api.wave_start(&id).map_err(|e| format!("{e:#}"));
            Some(Msg::WaveStarted { intent, station: id, name, result })
        });
    }

    fn load_cover(&mut self, url: Option<String>) {
        let Some(url) = url else { return };
        if self.picker.is_none() || self.covers.iter().any(|(u, _)| *u == url) || !self.covers_loading.insert(url.clone())
        {
            return;
        }
        self.spawn(move |api| {
            let result = api.fetch_image(&url).map_err(|e| format!("{e:#}"));
            Some(Msg::Cover { url, result })
        });
    }

    /// Cover of the current track, if it is loaded.
    pub fn current_cover(&mut self) -> Option<&mut StatefulProtocol> {
        let want = self.current()?.cover.clone()?;
        if self.shown_cover.as_ref().is_none_or(|(u, _)| *u != want) {
            let img = self.covers.iter().find(|(u, _)| *u == want)?.1.clone();
            self.shown_cover = Some((want, self.picker.as_ref()?.new_resize_protocol(img)));
        }
        self.shown_cover.as_mut().map(|(_, p)| p)
    }

    /// Terminal was resized: send the cover to the terminal again on the next draw.
    pub fn on_resize(&mut self) {
        self.shown_cover = None;
    }

    pub fn start_station_id(&mut self, id: &str) {
        let st = default_stations().into_iter().find(|s| s.id == id).unwrap_or_else(|| Station {
            id: id.to_string(),
            name: id.to_string(),
            category: String::new(),
            settings: None,
        });
        self.start_wave(st);
    }

    fn fetch_more_wave(&mut self) {
        let Some(w) = &mut self.wave else { return };
        if w.fetching {
            return;
        }
        w.fetching = true;
        let (st, sess) = (w.station.clone(), w.session.clone());
        let recent: Vec<String> = self.queue.iter().rev().take(10).rev().map(Track::full_id).collect();
        self.spawn(move |api| Some(Msg::WaveMore(api.wave_more(&st, sess.as_deref(), &recent).map_err(|e| format!("{e:#}")))));
    }

    // ------------------------------------------------------------ likes

    fn toggle_like(&mut self, track: Track) {
        let like = !self.is_liked(&track);
        self.apply_like(&track, like);
        if like && self.source == Source::Wave && self.queue.iter().any(|t| t.id == track.id) {
            self.wave_feedback(json!({"type": "like", "timestamp": iso_now(), "trackId": track.full_id()}), track.batch.clone());
        } else if !like && self.source == Source::Wave && self.queue.iter().any(|t| t.id == track.id) {
            self.wave_feedback(json!({"type": "unlike", "timestamp": iso_now(), "trackId": track.full_id()}), track.batch.clone());
        }
        self.set_status(if like {
            format!("♥ Добавлено в «Мне нравится»: {}", track.title)
        } else {
            format!("Убрано из «Мне нравится»: {}", track.title)
        });
        self.spawn(move |api| {
            api.set_like(&track, like).err().map(|e| Msg::LikeFailed { track, like, err: format!("{e:#}") })
        });
    }

    fn apply_like(&mut self, track: &Track, like: bool) {
        if like {
            self.liked_ids.insert(track.id.clone());
            if !self.liked.iter().any(|t| t.id == track.id) {
                self.liked.insert(0, track.clone());
            }
        } else {
            self.liked_ids.remove(&track.id);
            self.liked.retain(|t| t.id != track.id);
        }
        let n = self.liked.len();
        if let Some(s) = self.liked_state.selected() {
            self.liked_state.select(if n == 0 { None } else { Some(s.min(n - 1)) });
        }
        if self.liked_loading {
            self.liked_pending.push((track.clone(), like));
        }
    }

    fn dislike_current(&mut self) {
        let Some(track) = self.current().cloned() else { return };
        if self.is_liked(&track) {
            self.apply_like(&track, false);
        }
        if self.source == Source::Wave {
            self.wave_feedback(json!({"type": "dislike", "timestamp": iso_now(), "trackId": track.full_id()}), track.batch.clone());
        }
        self.set_status(format!("✗ Не нравится: {} — больше не попадётся", track.title));
        let t = track.clone();
        self.spawn(move |api| api.set_dislike(&t, true).err().map(|e| Msg::Status(format!("Ошибка дизлайка: {e:#}"))));
        self.next(End::Skip);
    }

    // ------------------------------------------------------------ messages

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Player(ev) => match ev {
                PlayerEvent::TimePos(t) => {
                    self.time_pos = t;
                    if self.loading_track && t > 0.0 {
                        self.loading_track = false;
                        self.errors_in_row = 0;
                        self.schedule_preload();
                    }
                }
                PlayerEvent::Duration(d) => {
                    self.duration = d;
                    self.player.set_track_duration(d);
                }
                PlayerEvent::Pause(p) => self.paused = p,
                PlayerEvent::Volume(v) => self.volume = v,
                PlayerEvent::Mute(m) => self.muted = m,
                PlayerEvent::Eof => {
                    // mpv moves to the appended (preloaded) entry on its own.
                    if !self.advance_preloaded(End::Eof, true) {
                        self.next(End::Eof);
                    }
                }
                PlayerEvent::Error(e) => {
                    self.set_status(format!("Ошибка воспроизведения: {e}"));
                    self.on_track_error();
                }
                PlayerEvent::Died => {
                    self.exit_error = Some(
                        "mpv завершился неожиданно; подробности можно записать в лог: YMUSIC_MPV_LOG=/tmp/mpv.log ymusic"
                            .into(),
                    );
                    self.quit = true;
                }
            },
            Msg::Url { gen, result } => {
                if gen != self.gen {
                    return;
                }
                match result {
                    Ok(url) => self.player.load(&url),
                    Err(e) => {
                        self.set_status(format!("Не удалось получить трек: {e}"));
                        self.on_track_error();
                    }
                }
            }
            Msg::LikedIds(ids) => {
                self.liked_ids = ids.iter().map(|i| i.split(':').next().unwrap_or(i).to_string()).collect();
                for (t, like) in &self.liked_pending {
                    if *like {
                        self.liked_ids.insert(t.id.clone());
                    } else {
                        self.liked_ids.remove(&t.id);
                    }
                }
            }
            Msg::Liked(r) => {
                self.liked_loading = false;
                let pending = std::mem::take(&mut self.liked_pending);
                match r {
                    Ok(list) => {
                        self.liked = list;
                        for (t, like) in &pending {
                            self.apply_like(t, *like);
                        }
                        if !self.liked.is_empty() && self.liked_state.selected().is_none() {
                            self.liked_state.select(Some(0));
                        }
                    }
                    Err(e) => self.set_status(format!("Не удалось загрузить «Мне нравится»: {e}")),
                }
            }
            Msg::Stations(list) => {
                let mut all = default_stations();
                all.retain(|s| s.id == MY_WAVE);
                let mut rest = list;
                let order = ["Жанры", "Настроение", "Занятия", "Эпохи"];
                rest.sort_by_key(|s| order.iter().position(|c| *c == s.category).unwrap_or(order.len()));
                all.extend(rest.into_iter().filter(|s| s.id != MY_WAVE));
                self.set_stations(all);
            }
            Msg::Search { query, page, result } => {
                // A reply to an older query or a page that is no longer expected.
                if query != self.search_query || page != self.search_pages {
                    return;
                }
                self.search_loading = false;
                match result {
                    Ok((list, total)) => {
                        self.search_pages += 1;
                        self.search_total = total;
                        if page == 0 {
                            if list.is_empty() {
                                self.set_status("Ничего не найдено");
                            }
                            self.search_results.clear();
                            self.search_state.select((!list.is_empty()).then_some(0));
                        }
                        // Pages may overlap if the results shift between requests.
                        let seen: HashSet<String> = self.search_results.iter().map(|t| t.id.clone()).collect();
                        let new: Vec<Track> = list.into_iter().filter(|t| !seen.contains(&t.id)).collect();
                        if new.is_empty() && page > 0 {
                            // Nothing new: stop asking for more pages.
                            self.search_total = self.search_results.len();
                        }
                        if let Some(shuffled) = self.search_queue {
                            let queued: HashSet<String> = self.queue.iter().map(|t| t.id.clone()).collect();
                            let mut more: Vec<Track> = new.iter().filter(|t| !queued.contains(&t.id)).cloned().collect();
                            if shuffled {
                                shuffle_tracks(&mut more);
                            }
                            self.queue.extend(more);
                        }
                        self.search_results.extend(new);
                    }
                    Err(e) => self.set_status(format!("Ошибка поиска: {e}")),
                }
            }
            Msg::Playlists { req, .. } if req != self.playlists_req => {}
            Msg::Playlists { result, .. } => {
                self.playlists_loading = false;
                match result {
                    Ok(list) => {
                        self.playlists = list;
                        if let Some(open) = &mut self.open_playlist {
                            match self.playlists.iter().find(|p| p.key() == open.pl.key()) {
                                Some(p) => open.pl = p.clone(),
                                // Deleted (here or elsewhere): back to the list.
                                None => self.open_playlist = None,
                            }
                        }
                        let n = self.playlists.len();
                        let sel = self.playlists_state.selected().unwrap_or(0);
                        self.playlists_state.select((n > 0).then(|| sel.min(n - 1)));
                    }
                    Err(e) => self.set_status(format!("Не удалось загрузить плейлисты: {e}")),
                }
            }
            Msg::PlaylistOpened { key, req, result } => {
                let Some(open) = self.open_playlist.as_mut().filter(|o| o.pl.key() == key && o.req == req) else {
                    return;
                };
                open.loading = false;
                match result {
                    Ok(tracks) => {
                        open.tracks = tracks;
                        let n = open.tracks.len();
                        let sel = self.open_state.selected().unwrap_or(0);
                        self.open_state.select((n > 0).then(|| sel.min(n - 1)));
                    }
                    Err(e) => self.set_status(format!("Не удалось открыть плейлист: {e}")),
                }
            }
            Msg::PlaylistEdited { key, result } => match result {
                Ok(status) => {
                    self.set_status(status);
                    self.load_playlists();
                    if self.open_playlist.as_ref().is_some_and(|o| o.pl.key() == key) {
                        self.reload_open_playlist();
                    }
                }
                Err(e) => self.set_status(format!("Ошибка: {e}")),
            },
            Msg::ListLoaded { intent, .. } | Msg::WaveStarted { intent, .. } if intent != self.intent => {}
            Msg::ListLoaded { name, shuffle, playlist, result, .. } => match result {
                Ok(list) if list.is_empty() => self.set_status(format!("{name}: нет треков")),
                Ok(mut list) => {
                    let name = if shuffle {
                        shuffle_tracks(&mut list);
                        format!("{name} (перемешано)")
                    } else {
                        name
                    };
                    self.play_list(&name, list, 0);
                    self.playing_playlist = playlist;
                    self.tab = Tab::Wave;
                }
                Err(e) => self.set_status(format!("{name}: не удалось загрузить треки: {e}")),
            },
            Msg::WaveStarted { station, name, result, .. } => match result {
                Ok(b) => {
                    self.report_end(End::Skip);
                    let from = station.replace(':', "-");
                    self.wave = Some(Wave {
                        station,
                        name: name.clone(),
                        session: b.session_id,
                        batch: b.batch_id.clone(),
                        fetching: false,
                        pending_next: false,
                    });
                    self.source = Source::Wave;
                    self.queue = b.tracks;
                    self.search_queue = None;
                    self.preload = None;
                    self.pos = None;
                    self.wave_feedback(json!({"type": "radioStarted", "timestamp": iso_now(), "from": from}), None);
                    self.tab = Tab::Wave;
                    self.set_status(format!("Играет: {name}"));
                    self.next(End::Silent);
                }
                Err(e) => self.set_status(format!("Не удалось запустить волну: {e}")),
            },
            Msg::WaveMore(r) => {
                let Some(w) = &mut self.wave else { return };
                w.fetching = false;
                match r {
                    Ok(b) => {
                        w.batch = b.batch_id;
                        if b.session_id.is_some() {
                            w.session = b.session_id;
                        }
                        let pending = std::mem::take(&mut w.pending_next);
                        let known: HashSet<String> = self.queue.iter().map(|t| t.id.clone()).collect();
                        self.queue.extend(b.tracks.into_iter().filter(|t| !known.contains(&t.id)));
                        if pending {
                            self.next(End::Silent);
                        } else {
                            self.schedule_preload();
                        }
                    }
                    Err(e) => self.set_status(format!("Волна: {e}")),
                }
            }
            Msg::LikeFailed { track, like, err } => {
                self.apply_like(&track, !like);
                self.set_status(format!("Ошибка лайка: {err}"));
            }
            Msg::Status(s) => self.set_status(s),
            // Only into an empty field: what the user has typed or pasted meanwhile stays.
            Msg::ClipboardLink(link) => {
                if let Some(Modal::OpenLink { input }) = &mut self.modal {
                    if input.is_empty() {
                        *input = link;
                    }
                }
            }
            Msg::LessAi(r) => match r {
                Ok(on) => self.less_ai = Some(on),
                Err(e) => self.set_status(format!("Не удалось загрузить настройки аккаунта: {e}")),
            },
            Msg::LessAiSet { on, result } => {
                if let Err(e) = result {
                    // Only roll back if nothing changed it since.
                    if self.less_ai == Some(on) {
                        self.less_ai = Some(!on);
                    }
                    self.set_status(format!("Не удалось изменить «Меньше ИИ-музыки»: {e}"));
                }
            }
            Msg::Cover { url, result } => {
                self.covers_loading.remove(&url);
                if let Ok(img) = result {
                    // Keep the current, the next and one previous cover.
                    if self.covers.len() >= 3 {
                        self.covers.remove(0);
                    }
                    self.covers.push((url, img));
                }
            }
            Msg::Preload { gen, idx, id, result } => {
                if gen != self.gen {
                    return;
                }
                self.preload_pending = false;
                if let Ok(url) = result {
                    if self.next_playable() == Some(idx) && self.queue[idx].id == id {
                        self.player.append(&url);
                        self.preload = Some((idx, id));
                    }
                }
            }
        }
    }

    fn on_track_error(&mut self) {
        // A preloaded link could have expired (e.g. after a long pause): retry with a fresh one.
        if std::mem::take(&mut self.from_preload) {
            if let Some(p) = self.pos {
                self.play_index(p);
                return;
            }
        }
        self.loading_track = false;
        self.errors_in_row += 1;
        if self.errors_in_row <= 5 {
            self.next(End::Silent);
        } else {
            self.player.stop();
        }
    }

    // ------------------------------------------------------------ keys

    /// Text pasted into the terminal: it goes into the open text field as is, and a Yandex Music
    /// link pasted on the main screen opens «Играть по ссылке» instead of acting as hotkeys.
    pub fn on_paste(&mut self, text: &str) {
        let text: String = text.trim().chars().filter(|c| !c.is_control()).collect();
        match &mut self.modal {
            Some(Modal::Rename { input, .. } | Modal::OpenLink { input }) => input.push_str(&text),
            Some(_) => {}
            None if self.search_editing => self.search_input.push_str(&text),
            None if self.show_help || self.show_about => {}
            None => match crate::api::parse_link(&text) {
                Ok(_) => self.modal = Some(Modal::OpenLink { input: text }),
                Err(e) => self.set_status(e),
            },
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.search_editing {
            self.on_search_key(key);
            return;
        }
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        if self.show_help || self.show_about {
            self.show_help = false;
            self.show_about = false;
            if std::mem::take(&mut self.about_from_settings) {
                self.modal = Some(Modal::Settings { sel: Setting::About.index() });
            }
            return;
        }
        let code = match key.code {
            KeyCode::Char(c) => KeyCode::Char(ru_to_en(c)),
            c => c,
        };
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            KeyCode::Char('o') => self.modal = Some(Modal::Settings { sel: 0 }),
            KeyCode::Char(' ') | KeyCode::Char('p') => self.player.toggle_pause(),
            KeyCode::Char('n') => self.next(End::Skip),
            KeyCode::Char('b') => self.prev(),
            KeyCode::Char('l') => {
                if let Some(t) = self.current().cloned() {
                    self.toggle_like(t);
                }
            }
            KeyCode::Char('L') => {
                if let Some(t) = self.selected_track().cloned() {
                    self.toggle_like(t);
                }
            }
            KeyCode::Char('d') => self.dislike_current(),
            KeyCode::Char('w') => self.start_wave(default_stations().remove(0)),
            KeyCode::Char('r') => {
                if let Some(t) = self.selected_track().or(self.current()).cloned() {
                    self.start_wave(Station {
                        id: format!("track:{}", t.id),
                        name: format!("Волна по треку «{}»", t.title),
                        category: String::new(),
                        settings: None,
                    });
                }
            }
            KeyCode::Char('y') => self.copy_link(),
            KeyCode::Char('Y') => self.ask_link(),
            KeyCode::Char('A') => self.play_artist(),
            KeyCode::Char('e') => self.play_album(),
            KeyCode::Char('s') if self.tab == Tab::Playlists && self.open_playlist.is_some() => self.play_open(true),
            KeyCode::Char('s') if self.tab == Tab::Playlists => self.play_playlist(true),
            KeyCode::Char('a') => self.ask_add_to_playlist(),
            KeyCode::Char('x') if self.tab == Tab::Playlists && self.open_playlist.is_some() => {
                self.ask_remove_from_playlist()
            }
            KeyCode::Char('x') if self.tab == Tab::Playlists => self.ask_delete_playlist(),
            KeyCode::Char('E') if self.tab == Tab::Playlists => self.ask_rename_playlist(),
            KeyCode::Esc | KeyCode::Backspace if self.tab == Tab::Playlists => self.open_playlist = None,
            KeyCode::Char('s') => self.shuffle_play(),
            KeyCode::Char('R') => {
                self.load_liked();
                self.load_stations();
                self.load_playlists();
                self.load_account_settings();
                self.reload_open_playlist();
                self.set_status("Обновление…");
            }
            KeyCode::Left => self.player.seek(-10.0),
            KeyCode::Right => self.player.seek(10.0),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(5.0),
            KeyCode::Char('-') | KeyCode::Char('_') => self.change_volume(-5.0),
            KeyCode::Char('m') => self.player.toggle_mute(),
            KeyCode::Char('1') => self.tab = Tab::Wave,
            KeyCode::Char('2') => self.tab = Tab::Stations,
            KeyCode::Char('3') => self.tab = Tab::Liked,
            KeyCode::Char('4') => self.tab = Tab::Search,
            KeyCode::Char('5') => self.tab = Tab::Playlists,
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Char('/') => {
                self.tab = Tab::Search;
                self.search_editing = true;
            }
            KeyCode::Char('i') if self.tab == Tab::Search => self.search_editing = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(i32::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(i32::MAX / 2),
            KeyCode::Enter => self.activate(),
            _ => {}
        }
    }

    /// Requests the next page of the current search.
    fn load_search_page(&mut self) {
        self.search_loading = true;
        let (query, page) = (self.search_query.clone(), self.search_pages);
        self.spawn(move |api| {
            let result = api.search(&query, page).map_err(|e| format!("{e:#}"));
            Some(Msg::Search { query, page, result })
        });
    }

    /// The current search has pages that are not loaded yet and none is loading now.
    fn can_load_more_search(&self) -> bool {
        !self.search_loading && self.search_pages > 0 && self.search_results.len() < self.search_total
    }

    /// Loads the next search page when the queue played from the search is close to its end.
    fn maybe_extend_search_queue(&mut self) {
        let near_end = self.pos.is_some_and(|p| p + 20 >= self.queue.len());
        if self.search_queue.is_some() && near_end && self.can_load_more_search() {
            self.load_search_page();
        }
    }

    /// Plays the loaded search results; later pages are appended to the queue.
    fn play_search(&mut self, name: &str, tracks: Vec<Track>, start: usize, shuffled: bool) {
        self.play_list(name, tracks, start);
        self.search_queue = Some(shuffled);
        self.maybe_extend_search_queue();
    }

    /// Loads more search results when the selection gets close to the end of the list.
    fn maybe_load_more_search(&mut self) {
        let sel = self.search_state.selected().unwrap_or(0);
        if sel + 20 >= self.search_results.len() && self.can_load_more_search() {
            self.load_search_page();
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.search_editing = false,
            KeyCode::Enter => {
                self.search_editing = false;
                let q = self.search_input.trim().to_string();
                if !q.is_empty() {
                    // The playing queue keeps the old results but no longer grows.
                    self.search_queue = None;
                    self.search_query = q;
                    self.search_pages = 0;
                    self.search_total = 0;
                    self.load_search_page();
                }
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.search_input.clear(),
            KeyCode::Char(c) if is_typed(&key) => self.search_input.push(c),
            _ => {}
        }
    }

    pub fn set_eq(&mut self, idx: usize) {
        let idx = idx.min(crate::eq::PRESETS.len() - 1);
        self.eq = idx;
        self.player.set_eq_filter(&crate::eq::PRESETS[idx].filter());
    }

    /// Fade-in/fade-out length in seconds (0 — off).
    pub fn set_crossfade(&mut self, secs: u8) {
        self.crossfade = secs.min(crate::config::MAX_CROSSFADE);
        self.player.set_fade(f64::from(self.crossfade));
    }

    pub fn set_icons(&mut self, nerd: bool) {
        self.icons = nerd;
        crate::ui::set_nerd_icons(nerd);
    }

    /// Current value of a settings row; `None` for rows that are actions.
    pub fn setting_value(&self, s: Setting) -> Option<String> {
        Some(match s {
            Setting::Quality => QUALITIES[self.quality].name.to_string(),
            Setting::Equalizer => crate::eq::PRESETS[self.eq].name.to_string(),
            Setting::Crossfade if self.crossfade == 0 => "выкл".to_string(),
            Setting::Crossfade => format!("{} с", self.crossfade),
            Setting::LessAi => match self.less_ai {
                Some(on) => if on { "вкл" } else { "выкл" }.to_string(),
                None => "…".to_string(),
            },
            Setting::Icons => if self.icons { "вкл" } else { "выкл" }.to_string(),
            Setting::About | Setting::Logout => return None,
        })
    }

    /// Switches a settings row to the next (`d` > 0) or previous value, wrapping around.
    fn change_setting(&mut self, s: Setting, d: i32) {
        let cycle = |cur: usize, len: usize| (cur as i64 + i64::from(d.signum())).rem_euclid(len as i64) as usize;
        match s {
            // The current (and already preloaded) track keeps its stream.
            Setting::Quality => self.quality = cycle(self.quality, QUALITIES.len()),
            // Applied right away, so the preset can be heard.
            Setting::Equalizer => self.set_eq(cycle(self.eq, crate::eq::PRESETS.len())),
            Setting::Crossfade => {
                // A value from the config may lie between the steps.
                let cur = self.crossfade;
                let next = if d > 0 {
                    CROSSFADE_STEPS.iter().find(|&&s| s > cur).copied().unwrap_or(CROSSFADE_STEPS[0])
                } else {
                    CROSSFADE_STEPS.iter().rev().find(|&&s| s < cur).copied().unwrap_or(CROSSFADE_STEPS[CROSSFADE_STEPS.len() - 1])
                };
                self.set_crossfade(next);
            }
            Setting::LessAi => match self.less_ai {
                // Shown right away; rolled back if the server refuses.
                Some(cur) => {
                    let on = !cur;
                    self.less_ai = Some(on);
                    self.spawn(move |api| {
                        Some(Msg::LessAiSet { on, result: api.set_ai_reduction(on).map_err(|e| format!("{e:#}")) })
                    });
                }
                None => self.set_status("Настройки аккаунта ещё загружаются"),
            },
            Setting::Icons => self.set_icons(!self.icons),
            Setting::About | Setting::Logout => {}
        }
    }

    fn change_volume(&mut self, d: f64) {
        self.volume = (self.volume + d).clamp(0.0, MAX_VOLUME);
        self.player.set_volume(self.volume);
    }

    fn selected_track(&self) -> Option<&Track> {
        match self.tab {
            Tab::Liked => self.liked_state.selected().and_then(|i| self.liked.get(i)),
            Tab::Search => self.search_state.selected().and_then(|i| self.search_results.get(i)),
            Tab::Wave => self.queue_state.selected().and_then(|i| self.queue.get(i)),
            Tab::Playlists => {
                let open = self.open_playlist.as_ref()?;
                self.open_state.selected().and_then(|i| open.tracks.get(i))
            }
            Tab::Stations => None,
        }
    }

    fn move_sel(&mut self, d: i32) {
        let step = |cur: Option<usize>, len: usize| -> Option<usize> {
            (len > 0).then(|| (cur.unwrap_or(0) as i64 + i64::from(d)).clamp(0, len as i64 - 1) as usize)
        };
        match self.tab {
            Tab::Wave => self.queue_state.select(step(self.queue_state.selected(), self.queue.len())),
            Tab::Liked => self.liked_state.select(step(self.liked_state.selected(), self.liked.len())),
            Tab::Search => {
                self.search_state.select(step(self.search_state.selected(), self.search_results.len()));
                self.maybe_load_more_search();
            }
            Tab::Playlists => match &self.open_playlist {
                Some(open) => self.open_state.select(step(self.open_state.selected(), open.tracks.len())),
                None => self.playlists_state.select(step(self.playlists_state.selected(), self.playlists.len())),
            },
            Tab::Stations => {
                self.stations_state.select(step(self.stations_state.selected(), self.stations.len()));
                self.fix_station_selection(if d >= 0 { 1 } else { -1 });
            }
        }
    }

    fn activate(&mut self) {
        match self.tab {
            Tab::Wave => {
                if let Some(i) = self.queue_state.selected().filter(|&i| i < self.queue.len()) {
                    self.report_end(End::Skip);
                    self.play_index(i);
                }
            }
            Tab::Stations => {
                if let Some(StationRow::Station(s)) = self.stations_state.selected().and_then(|i| self.stations.get(i)) {
                    self.start_wave(s.clone());
                }
            }
            Tab::Liked => {
                if let Some(i) = self.liked_state.selected() {
                    self.play_list("Мне нравится", self.liked.clone(), i);
                }
            }
            Tab::Search => {
                if let Some(i) = self.search_state.selected() {
                    self.play_search("Поиск", self.search_results.clone(), i, false);
                }
            }
            Tab::Playlists if self.open_playlist.is_some() => self.play_open(false),
            Tab::Playlists => self.open_selected_playlist(),
        }
    }

    // ------------------------------------------------------------ playlists

    fn selected_playlist(&self) -> Option<&Playlist> {
        self.playlists_state.selected().and_then(|i| self.playlists.get(i))
    }

    fn open_selected_playlist(&mut self) {
        let Some(pl) = self.selected_playlist().cloned() else { return };
        self.open_playlist = Some(OpenPlaylist { pl, tracks: Vec::new(), loading: true, req: 0 });
        self.open_state = TableState::default();
        self.reload_open_playlist();
    }

    fn reload_open_playlist(&mut self) {
        let Some(open) = &mut self.open_playlist else { return };
        open.loading = true;
        open.req += 1;
        let (pl, req) = (open.pl.clone(), open.req);
        self.spawn(move |api| {
            let result = api.playlist_tracks(&pl).map_err(|e| format!("{e:#}"));
            Some(Msg::PlaylistOpened { key: pl.key(), req, result })
        });
    }

    /// Plays the opened playlist from the selected track (or all of it shuffled).
    fn play_open(&mut self, shuffle: bool) {
        let Some(open) = &self.open_playlist else { return };
        let (key, name, mut tracks) = (open.pl.key(), format!("Плейлист: {}", open.pl.title), open.tracks.clone());
        if tracks.is_empty() {
            return;
        }
        if shuffle {
            shuffle_tracks(&mut tracks);
            self.play_list(&format!("{name} (перемешано)"), tracks, 0);
        } else {
            let start = self.open_state.selected().unwrap_or(0);
            self.play_list(&name, tracks, start);
        }
        self.playing_playlist = Some(key);
    }

    /// Own playlists (only they can be edited).
    fn own_playlists(&self) -> Vec<Playlist> {
        self.playlists.iter().filter(|p| p.owner == self.api.uid).cloned().collect()
    }

    /// Playlist targeted by rename: the opened one or the one selected in the list.
    fn target_playlist(&self) -> Option<Playlist> {
        match &self.open_playlist {
            Some(open) => Some(open.pl.clone()),
            None => self.selected_playlist().cloned(),
        }
    }

    fn ask_add_to_playlist(&mut self) {
        let Some(track) = self.selected_track().or(self.current()).cloned() else {
            self.set_status("Нет выбранного трека");
            return;
        };
        let choices = self.own_playlists();
        if choices.is_empty() {
            self.set_status(if self.playlists_loading { "Плейлисты ещё загружаются…" } else { "Нет своих плейлистов" });
            return;
        }
        self.modal = Some(Modal::AddTo { track, choices, sel: 0 });
    }

    fn ask_remove_from_playlist(&mut self) {
        let Some(open) = &self.open_playlist else { return };
        let Some(index) = self.open_state.selected().filter(|&i| i < open.tracks.len()) else { return };
        if open.pl.owner != self.api.uid {
            self.set_status("Можно менять только свои плейлисты");
            return;
        }
        self.modal = Some(Modal::ConfirmRemove { pl: open.pl.clone(), index, track: open.tracks[index].clone() });
    }

    fn ask_delete_playlist(&mut self) {
        let Some(pl) = self.selected_playlist().cloned() else { return };
        if pl.owner != self.api.uid {
            self.set_status("Можно менять только свои плейлисты");
            return;
        }
        self.modal = Some(Modal::ConfirmDelete { pl });
    }

    fn ask_rename_playlist(&mut self) {
        let Some(pl) = self.target_playlist() else { return };
        if pl.owner != self.api.uid {
            self.set_status("Можно менять только свои плейлисты");
            return;
        }
        let input = pl.title.clone();
        self.modal = Some(Modal::Rename { pl, input });
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        let code = match (&modal, key.code) {
            // Text input gets characters as typed; elsewhere map the Russian layout.
            (Modal::Rename { .. } | Modal::OpenLink { .. }, c) => c,
            (_, KeyCode::Char(c)) => KeyCode::Char(ru_to_en(c)),
            (_, c) => c,
        };
        match modal {
            Modal::AddTo { track, choices, mut sel } => match code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(choices.len() - 1);
                    self.modal = Some(Modal::AddTo { track, choices, sel });
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.saturating_sub(1);
                    self.modal = Some(Modal::AddTo { track, choices, sel });
                }
                KeyCode::Enter => {
                    let pl = choices[sel].clone();
                    self.set_status(format!("Добавление в «{}»…", pl.title));
                    self.spawn(move |api| {
                        let result = api
                            .add_to_playlist(&pl, &track)
                            .map(|()| format!("Добавлено в «{}»: {}", pl.title, track.title))
                            .map_err(|e| format!("{e:#}"));
                        Some(Msg::PlaylistEdited { key: pl.key(), result })
                    });
                }
                _ => self.modal = Some(Modal::AddTo { track, choices, sel }),
            },
            Modal::ConfirmRemove { pl, index, track } => {
                if code == KeyCode::Enter {
                    self.set_status(format!("Удаление из «{}»…", pl.title));
                    self.spawn(move |api| {
                        let result = api
                            .remove_from_playlist(&pl, index, &track.id)
                            .map(|()| format!("Удалено из «{}»: {}", pl.title, track.title))
                            .map_err(|e| format!("{e:#}"));
                        Some(Msg::PlaylistEdited { key: pl.key(), result })
                    });
                }
            }
            Modal::ConfirmDelete { pl } => {
                if code == KeyCode::Enter {
                    self.set_status(format!("Удаление плейлиста «{}»…", pl.title));
                    self.spawn(move |api| {
                        let result = api
                            .delete_playlist(&pl)
                            .map(|()| format!("Плейлист удалён: «{}»", pl.title))
                            .map_err(|e| format!("{e:#}"));
                        Some(Msg::PlaylistEdited { key: pl.key(), result })
                    });
                }
            }
            Modal::Settings { mut sel } => {
                let row = Setting::ALL[sel];
                match code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => return,
                    KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(Setting::ALL.len() - 1),
                    KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                    KeyCode::Left | KeyCode::Char('h') => self.change_setting(row, -1),
                    KeyCode::Right | KeyCode::Char('l') => self.change_setting(row, 1),
                    KeyCode::Enter => match row {
                        Setting::About => {
                            self.show_about = true;
                            self.about_from_settings = true;
                            return;
                        }
                        Setting::Logout => {
                            self.modal = Some(Modal::ConfirmLogout);
                            return;
                        }
                        _ => self.change_setting(row, 1),
                    },
                    _ => {}
                }
                self.modal = Some(Modal::Settings { sel });
            }
            Modal::ConfirmLogout => {
                if code == KeyCode::Enter {
                    self.logout = true;
                    self.quit = true;
                } else {
                    self.modal = Some(Modal::Settings { sel: Setting::Logout.index() });
                }
            }
            Modal::Rename { pl, mut input } => match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let title = input.trim().to_string();
                    if title.is_empty() || title == pl.title {
                        return;
                    }
                    self.spawn(move |api| {
                        let result = api
                            .rename_playlist(&pl, &title)
                            .map(|()| format!("Плейлист переименован: «{title}»"))
                            .map_err(|e| format!("{e:#}"));
                        Some(Msg::PlaylistEdited { key: pl.key(), result })
                    });
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::Rename { pl, input });
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.modal = Some(Modal::Rename { pl, input: String::new() });
                }
                KeyCode::Char(c) if is_typed(&key) => {
                    input.push(c);
                    self.modal = Some(Modal::Rename { pl, input });
                }
                _ => self.modal = Some(Modal::Rename { pl, input }),
            },
            Modal::OpenLink { mut input } => match code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.play_link(&input),
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::OpenLink { input });
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.modal = Some(Modal::OpenLink { input: String::new() });
                }
                KeyCode::Char(c) if is_typed(&key) => {
                    input.push(c);
                    self.modal = Some(Modal::OpenLink { input });
                }
                _ => self.modal = Some(Modal::OpenLink { input }),
            },
        }
    }

    /// Loads the selected playlist and plays it (in order or shuffled).
    fn play_playlist(&mut self, shuffle: bool) {
        let Some(pl) = self.playlists_state.selected().and_then(|i| self.playlists.get(i)).cloned() else {
            return;
        };
        self.set_status(format!("Загрузка плейлиста «{}»…", pl.title));
        let intent = self.new_intent();
        self.spawn(move |api| {
            let result = api.playlist_tracks(&pl).map_err(|e| format!("{e:#}"));
            Some(Msg::ListLoaded { intent, name: format!("Плейлист: {}", pl.title), shuffle, playlist: Some(pl.key()), result })
        });
    }

    /// Copies the link to the selected (or current) track to the clipboard.
    fn copy_link(&mut self) {
        let Some(t) = self.selected_track().or(self.current()).cloned() else {
            self.set_status("Нет выбранного трека");
            return;
        };
        let url = t.web_url();
        crate::clipboard::copy(&url);
        self.set_status(format!("Ссылка скопирована: {url}"));
    }

    /// Asks for a link; a Yandex Music link from the clipboard is filled in once read
    /// (clipboard tools may take a while, so the window opens at once).
    fn ask_link(&mut self) {
        self.modal = Some(Modal::OpenLink { input: String::new() });
        self.spawn(|_| {
            crate::clipboard::paste()
                .map(|s| s.trim().to_string())
                .filter(|s| crate::api::parse_link(s).is_ok())
                .map(Msg::ClipboardLink)
        });
    }

    /// Plays a track, album or playlist from a Yandex Music link; other links are not opened.
    fn play_link(&mut self, url: &str) {
        let link = match crate::api::parse_link(url) {
            Ok(link) => link,
            Err(e) => return self.set_status(e),
        };
        self.set_status("Загрузка по ссылке…");
        let intent = self.new_intent();
        self.spawn(move |api| {
            let titled = |kind: &str, r: anyhow::Result<(String, Vec<Track>)>| match r {
                Ok((title, tracks)) => {
                    let name = if title.is_empty() { format!("{kind} по ссылке") } else { format!("{kind}: {title}") };
                    (name, Ok(tracks))
                }
                Err(e) => (format!("{kind} по ссылке"), Err(format!("{e:#}"))),
            };
            let (name, result) = match &link {
                Link::Track(id) => ("Трек по ссылке".to_string(), api.tracks(std::slice::from_ref(id)).map_err(|e| format!("{e:#}"))),
                Link::Album(id) => titled("Альбом", api.album_tracks(id)),
                Link::Playlist { .. } | Link::PlaylistUuid(_) => titled("Плейлист", api.linked_playlist(&link)),
            };
            Some(Msg::ListLoaded { intent, name, shuffle: false, playlist: None, result })
        });
    }

    /// Plays all tracks of the selected (or current) track's main artist.
    fn play_artist(&mut self) {
        let Some(t) = self.selected_track().or(self.current()) else {
            self.set_status("Нет выбранного трека");
            return;
        };
        let Some((id, name)) = t.main_artist.clone() else {
            self.set_status("У трека не указан исполнитель");
            return;
        };
        self.set_status(format!("Загрузка треков: {name}…"));
        let intent = self.new_intent();
        self.spawn(move |api| {
            let result = api.artist_tracks(&id).map_err(|e| format!("{e:#}"));
            Some(Msg::ListLoaded { intent, name: format!("Исполнитель: {name}"), shuffle: false, playlist: None, result })
        });
    }

    /// Plays the album of the selected (or current) track.
    fn play_album(&mut self) {
        let Some(t) = self.selected_track().or(self.current()) else {
            self.set_status("Нет выбранного трека");
            return;
        };
        let Some(id) = t.album_id.clone() else {
            self.set_status("У трека не указан альбом");
            return;
        };
        let fallback = t.album.clone();
        self.set_status(format!("Загрузка альбома: {fallback}…"));
        let intent = self.new_intent();
        self.spawn(move |api| {
            let result = api.album_tracks(&id).map_err(|e| format!("{e:#}"));
            let name = match &result {
                Ok((title, _)) if !title.is_empty() => format!("Альбом: {title}"),
                _ => format!("Альбом: {fallback}"),
            };
            Some(Msg::ListLoaded { intent, name, shuffle: false, playlist: None, result: result.map(|(_, t)| t) })
        });
    }

    fn shuffle_play(&mut self) {
        let (name, mut list) = match self.tab {
            Tab::Search => ("Поиск (перемешано)", self.search_results.clone()),
            _ => ("Мне нравится (перемешано)", self.liked.clone()),
        };
        shuffle_tracks(&mut list);
        if self.tab == Tab::Search {
            self.play_search(name, list, 0, true);
        } else {
            self.play_list(name, list, 0);
        }
    }

    pub fn save_volume(&self) -> u8 {
        self.volume.clamp(0.0, MAX_VOLUME) as u8
    }
}

fn shuffle_tracks(list: &mut [Track]) {
    let mut seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
    for i in (1..list.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        list.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

/// A key that types its character into a text field (not a Ctrl/Alt shortcut).
fn is_typed(key: &KeyEvent) -> bool {
    !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Maps Russian (ЙЦУКЕН) layout characters to their QWERTY keys so hotkeys work on either layout.
fn ru_to_en(c: char) -> char {
    const RU: &str = "йцукенгшщзхъфывапролджэячсмитьбю.ЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮ,";
    const EN: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,./QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>?";
    RU.chars().position(|r| r == c).and_then(|i| EN.chars().nth(i)).unwrap_or(c)
}

#[cfg(test)]
mod tests {
    #[test]
    fn layout() {
        assert_eq!(super::ru_to_en('д'), 'l');
        assert_eq!(super::ru_to_en('т'), 'n');
        assert_eq!(super::ru_to_en('Д'), 'L');
        assert_eq!(super::ru_to_en('.'), '/');
        assert_eq!(super::ru_to_en('Ф'), 'A');
        assert_eq!(super::ru_to_en('x'), 'x');
    }

    /// Keys inside pop-up dialogs.
    #[test]
    fn modal_keys() {
        use super::*;
        use ratatui::crossterm::event::KeyEvent;
        let (mut app, _audio) = App::for_test();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let pl = |k: &str, owner: &str| Playlist { owner: owner.into(), kind: k.into(), title: format!("P{k}"), track_count: 0 };
        app.playlists = vec![pl("1", "1"), pl("2", "999"), pl("3", "1")];
        app.playlists_loading = false;
        let t = Track {
            id: "7".into(),
            album_id: Some("8".into()),
            title: "T".into(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: None,
            batch: None,
        };
        app.queue = vec![t.clone()];
        app.queue_state.select(Some(0));

        // `a` (and `ф`) offers only own playlists; selection stays in bounds; Esc closes.
        app.on_key(key(KeyCode::Char('ф')));
        let Some(Modal::AddTo { choices, .. }) = &app.modal else { panic!("no picker") };
        assert_eq!(choices.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        for _ in 0..5 {
            app.on_key(key(KeyCode::Char('j')));
        }
        assert!(matches!(app.modal, Some(Modal::AddTo { sel: 1, .. })));
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('k')));
        assert!(matches!(app.modal, Some(Modal::AddTo { sel: 0, .. })));
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        // Rename: Russian letters are typed as is (no layout mapping), Esc cancels.
        app.tab = Tab::Playlists;
        app.playlists_state.select(Some(0));
        app.on_key(key(KeyCode::Char('E')));
        for c in ['д', 'а'] {
            app.on_key(key(KeyCode::Char(c)));
        }
        // Ctrl/Alt shortcuts do not type their letter.
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        let Some(Modal::Rename { input, .. }) = &app.modal else { panic!("no rename") };
        assert_eq!(input, "P1да");
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        // Someone else's playlist cannot be renamed.
        app.playlists_state.select(Some(1));
        app.on_key(key(KeyCode::Char('E')));
        assert!(app.modal.is_none());

        // A link to another site is rejected without any request; text is typed as is.
        app.modal = Some(Modal::OpenLink { input: String::new() });
        for c in "https://evil.com/track/1".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let Some(Modal::OpenLink { input }) = &app.modal else { panic!("no link input") };
        assert_eq!(input, "https://evil.com/track/1");
        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.status, "Это не ссылка на Яндекс Музыку");

        // `x` asks for confirmation; any key other than Enter cancels.
        app.open_playlist = Some(OpenPlaylist { pl: app.playlists[0].clone(), tracks: vec![t], loading: false, req: 0 });
        app.open_state.select(Some(0));
        app.on_key(key(KeyCode::Char('x')));
        assert!(matches!(app.modal, Some(Modal::ConfirmRemove { index: 0, .. })));
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.modal.is_none());
        // Esc leaves the opened playlist.
        app.on_key(key(KeyCode::Esc));
        assert!(app.open_playlist.is_none());

        // In the list `x` deletes the whole playlist (own only), after confirmation.
        app.playlists_state.select(Some(2));
        app.on_key(key(KeyCode::Char('ч')));
        assert!(matches!(&app.modal, Some(Modal::ConfirmDelete { pl }) if pl.kind == "3"));
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.playlists_state.select(Some(1));
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.modal.is_none());

        // Settings window (`o`, `щ` in the Russian layout): ←/→ change values right away.
        app.set_eq(crate::eq::index("rock"));
        app.on_key(key(KeyCode::Char('щ')));
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 0 })));
        assert_eq!(QUALITIES[app.quality].id, "nq");
        app.on_key(key(KeyCode::Left));
        assert_eq!(QUALITIES[app.quality].id, "lq");
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.quality, QUALITIES.len() - 1, "wraps around");
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.eq, crate::eq::index("electronic"));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.crossfade, 0);
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.crossfade, 5);
        app.crossfade = 7;
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.crossfade, 5, "a config value between steps snaps to the nearest one");

        // «Меньше ИИ-музыки»: not loaded yet — nothing changes; then switched at once, rolled back on error.
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.less_ai, None);
        app.on_msg(Msg::LessAi(Ok(false)));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.less_ai, Some(true));
        app.on_msg(Msg::LessAiSet { on: true, result: Err("нет сети".into()) });
        assert_eq!(app.less_ai, Some(false));
        app.on_key(key(KeyCode::Enter));
        app.on_msg(Msg::LessAiSet { on: true, result: Ok(()) });
        assert_eq!(app.less_ai, Some(true));

        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert!(!app.icons);
        app.on_key(key(KeyCode::Right));
        assert!(app.icons);

        // «О программе» opens from the settings and returns there.
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.show_about && app.modal.is_none());
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.show_about);
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 5 })));

        // Logout asks first; Esc goes back to the settings.
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::ConfirmLogout)));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 6 })));
        assert!(!app.logout && !app.quit);
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_key(key(KeyCode::Char('o')));
        for _ in 0..10 {
            app.on_key(key(KeyCode::Char('j')));
        }
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        assert!(app.logout && app.quit);
    }

    /// Preload state machine.
    #[test]
    fn preload_flow() {
        use super::*;
        let (mut app, audio) = App::for_test();
        let t = |id: &str| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: None,
            batch: None,
        };
        app.queue = vec![t("a"), t("b"), t("c")];
        app.source = Source::List("test".into());
        app.begin_track(0, false);
        app.loading_track = false;
        audio.take();

        // Stale response (older generation) is ignored.
        app.on_msg(Msg::Preload { gen: app.gen - 1, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert!(app.preload.is_none());
        assert!(audio.take().is_empty());
        // Fresh response is appended.
        app.on_msg(Msg::Preload { gen: app.gen, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert_eq!(app.preload, Some((1, "b".to_string())));
        assert_eq!(audio.take(), ["append http://x/b"]);

        // End of track: mpv moves on by itself, state follows without a new URL request.
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.pos, Some(1));
        assert!(app.from_preload && app.preload.is_none());
        assert!(audio.take().is_empty());

        // Manual skip uses the preloaded entry too.
        app.loading_track = false;
        app.on_msg(Msg::Preload { gen: app.gen, idx: 2, id: "c".into(), result: Ok("http://x/c".into()) });
        app.next(End::Skip);
        assert_eq!(app.pos, Some(2));
        assert_eq!(audio.take(), ["append http://x/c", "next"]);

        // Error on a preloaded track retries the same track with a fresh URL.
        app.on_track_error();
        assert_eq!(app.pos, Some(2));
        assert!(!app.from_preload);
    }

    #[test]
    fn search_paging() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        let t = |id: usize| Track {
            id: id.to_string(),
            album_id: None,
            title: id.to_string(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: None,
            batch: None,
        };
        let page = |r: std::ops::Range<usize>| r.map(t).collect::<Vec<_>>();
        app.tab = Tab::Search;
        app.search_query = "q".into();
        app.search_loading = true;
        app.on_msg(Msg::Search { query: "q".into(), page: 0, result: Ok((page(0..100), 250)) });
        assert_eq!((app.search_results.len(), app.search_total, app.search_loading), (100, 250, false));

        // Far from the end: nothing more is requested.
        app.move_sel(10);
        assert!(!app.search_loading);
        // Close to the end: the next page is requested.
        app.move_sel(75);
        assert!(app.search_loading);

        // Replies to another query or page are ignored.
        app.on_msg(Msg::Search { query: "old".into(), page: 1, result: Ok((page(500..600), 900)) });
        app.on_msg(Msg::Search { query: "q".into(), page: 0, result: Ok((page(500..600), 900)) });
        assert_eq!(app.search_results.len(), 100);

        // Overlapping tracks are not duplicated; selection stays in place.
        app.on_msg(Msg::Search { query: "q".into(), page: 1, result: Ok((page(90..190), 250)) });
        assert_eq!(app.search_results.len(), 190);
        assert_eq!(app.search_state.selected(), Some(85));

        // Playing from the search: later pages go to the queue, playing near its end loads more.
        app.search_total = 400;
        app.search_state.select(Some(0));
        app.activate();
        assert_eq!((app.queue.len(), app.pos, app.search_loading), (190, Some(0), false));
        app.begin_track(175, false);
        assert!(app.search_loading);
        app.on_msg(Msg::Search { query: "q".into(), page: 2, result: Ok((page(180..290), 400)) });
        assert_eq!((app.search_results.len(), app.queue.len()), (290, 290));
        assert_eq!(app.queue[289].id, "289");
        // A new search does not touch the playing queue.
        app.search_input = "new".into();
        app.on_search_key(KeyEvent::from(KeyCode::Enter));
        app.on_msg(Msg::Search { query: "new".into(), page: 0, result: Ok((page(1000..1100), 900)) });
        assert_eq!(app.queue.len(), 290);
        app.search_query = "q".into();
        app.search_pages = 3;
        app.search_results = page(0..290);

        // A page without new tracks ends the paging.
        app.search_loading = true;
        app.on_msg(Msg::Search { query: "q".into(), page: 3, result: Ok((page(0..100), 400)) });
        assert_eq!(app.search_total, 290);
        app.move_sel(1000);
        assert!(!app.search_loading);
    }

    /// A like set while «Мне нравится» is loading survives the server's (older) answer;
    /// an older playlists reply does not replace a newer one.
    #[test]
    fn replies_during_reload() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        let t = |id: &str| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: None,
            batch: None,
        };
        let ids = |app: &App| app.liked.iter().map(|t| t.id.clone()).collect::<Vec<_>>();

        // Liked before the first load finished: the server's list does not have it yet.
        app.start();
        assert!(app.liked_loading);
        app.toggle_like(t("5"));
        app.on_msg(Msg::LikedIds(vec!["1:10".into(), "2:20".into()]));
        assert!(app.liked_ids.contains("5") && app.liked_ids.contains("1"));
        app.on_msg(Msg::Liked(Ok(vec![t("1"), t("2")])));
        assert_eq!(ids(&app), ["5", "1", "2"]);
        assert!(!app.liked_loading && app.liked_pending.is_empty());

        // `R`, then a like is removed while the old list is still coming.
        app.load_liked();
        app.load_liked();
        app.toggle_like(t("1"));
        app.on_msg(Msg::LikedIds(vec!["5:1".into(), "1:10".into(), "2:20".into()]));
        assert!(!app.liked_ids.contains("1"));
        app.on_msg(Msg::Liked(Ok(vec![t("5"), t("1"), t("2")])));
        assert_eq!(ids(&app), ["5", "2"]);

        // After the load, likes apply directly.
        app.toggle_like(t("2"));
        assert!(app.liked_pending.is_empty());
        assert_eq!(ids(&app), ["5"]);

        let pl = |k: &str| Playlist { owner: "1".into(), kind: k.into(), title: k.into(), track_count: 0 };
        app.load_playlists();
        app.on_msg(Msg::Playlists { req: app.playlists_req - 1, result: Ok(vec![pl("old")]) });
        assert!(app.playlists.is_empty() && app.playlists_loading);
        app.on_msg(Msg::Playlists { req: app.playlists_req, result: Ok(vec![pl("new")]) });
        assert_eq!(app.playlists[0].kind, "new");
    }

    /// Pasted text goes into the open field; on the main screen only a Yandex Music link is taken.
    #[test]
    fn paste() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        let link = "https://music.yandex.ru/album/9/track/123";

        // Not a hotkey stream: `q` in the text does not quit.
        app.on_paste("https://example.com/q");
        assert!(!app.quit && app.modal.is_none());
        assert_eq!(app.status, "Это не ссылка на Яндекс Музыку");

        app.on_paste(&format!(" {link}\n"));
        let Some(Modal::OpenLink { input }) = &app.modal else { panic!("no link input") };
        assert_eq!(input, link);
        app.modal = None;

        // `Y` opens the window at once; the clipboard link arrives later and fills an empty field only.
        let input = |app: &App| match &app.modal {
            Some(Modal::OpenLink { input }) => input.clone(),
            _ => panic!("no link input"),
        };
        app.on_key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT));
        assert_eq!(input(&app), "");
        app.on_msg(Msg::ClipboardLink(link.into()));
        assert_eq!(input(&app), link);
        app.modal = Some(Modal::OpenLink { input: "typed".into() });
        app.on_msg(Msg::ClipboardLink(link.into()));
        assert_eq!(input(&app), "typed");
        app.modal = None;

        app.search_editing = true;
        app.on_paste("рок\tq");
        assert_eq!(app.search_input, "рокq");
    }

    /// Replies to an older play request do not replace what the user started later.
    #[test]
    fn stale_play_replies() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        let t = |id: &str| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: None,
            batch: None,
        };
        let list = |intent, name: &str, tracks| Msg::ListLoaded {
            intent,
            name: name.into(),
            shuffle: false,
            playlist: None,
            result: Ok(tracks),
        };

        // `w`, then `A` before the wave replied: the late wave is dropped.
        let wave = app.new_intent();
        let album = app.new_intent();
        let batch = WaveBatch { session_id: None, batch_id: "b".into(), tracks: vec![t("w")] };
        app.on_msg(Msg::WaveStarted { intent: wave, station: MY_WAVE.into(), name: "Моя волна".into(), result: Ok(batch) });
        assert!(app.wave.is_none());
        app.on_msg(list(album, "Альбом", vec![t("a")]));
        assert!(app.source == Source::List("Альбом".into()));

        // A list started by hand drops a link that was still loading.
        let link = app.new_intent();
        app.play_list("Мне нравится", vec![t("c")], 0);
        app.on_msg(list(link, "Трек по ссылке", vec![t("d")]));
        assert_eq!(app.queue[0].id, "c");
    }
}
