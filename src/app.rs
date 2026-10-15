//! Application state and logic.

use crate::api::{default_stations, iso_now, Account, Api, Station, Track, WaveBatch, MY_WAVE};
use crate::player::{Player, PlayerEvent};
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
    Search(Result<Vec<Track>, String>),
    WaveStarted { station: String, name: String, result: Result<WaveBatch, String> },
    WaveMore(Result<WaveBatch, String>),
    LikeFailed { track: Track, like: bool, err: String },
    Cover { url: String, result: Result<image::DynamicImage, String> },
    Preload { gen: u64, idx: usize, id: String, result: Result<String, String> },
    Status(String),
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
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Wave, Tab::Stations, Tab::Liked, Tab::Search];
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

pub enum StationRow {
    Header(String),
    Station(Station),
}

pub struct App {
    pub api: Api,
    tx: Sender<Msg>,
    player: Player,
    pub account: Account,
    pub tab: Tab,
    pub quit: bool,
    pub show_help: bool,

    pub queue: Vec<Track>,
    pub pos: Option<usize>,
    pub source: Source,
    pub wave: Option<Wave>,
    pub queue_state: TableState,
    gen: u64,
    errors_in_row: u32,
    pub loading_track: bool,

    pub liked_ids: HashSet<String>,
    pub liked: Vec<Track>,
    pub liked_loading: bool,
    pub liked_state: TableState,

    pub stations: Vec<StationRow>,
    pub stations_state: ListState,

    pub search_input: String,
    pub search_editing: bool,
    pub search_results: Vec<Track>,
    pub search_loading: bool,
    pub search_state: TableState,

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
    /// Recently used covers: (url, encoded image), newest last.
    pub covers: Vec<(String, StatefulProtocol)>,
    covers_loading: HashSet<String>,

    /// Next track already resolved and appended to mpv's playlist: (queue index, track id).
    preload: Option<(usize, String)>,
    preload_pending: bool,
    /// Current track was started from the preloaded entry (its URL may have expired on error).
    from_preload: bool,
}

impl App {
    pub fn new(api: Api, account: Account, tx: Sender<Msg>, player: Player, volume: u8) -> App {
        let mut app = App {
            api,
            tx,
            player,
            account,
            tab: Tab::Wave,
            quit: false,
            show_help: false,
            queue: Vec::new(),
            pos: None,
            source: Source::None,
            wave: None,
            queue_state: TableState::default(),
            gen: 0,
            errors_in_row: 0,
            loading_track: false,
            liked_ids: HashSet::new(),
            liked: Vec::new(),
            liked_loading: true,
            liked_state: TableState::default(),
            stations: Vec::new(),
            stations_state: ListState::default(),
            search_input: String::new(),
            search_editing: false,
            search_results: Vec::new(),
            search_loading: false,
            search_state: TableState::default(),
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
            covers_loading: HashSet::new(),
            preload: None,
            preload_pending: false,
            from_preload: false,
        };
        app.set_stations(default_stations());
        app.load_liked();
        app.load_stations();
        if !app.account.has_plus {
            app.set_status("Внимание: без подписки Плюс доступны только 30-секундные фрагменты");
        }
        app
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

    fn load_liked(&mut self) {
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
        self.spawn(move |api| Some(Msg::Url { gen, result: api.track_url(&id).map_err(|e| format!("{e:#}")) }));
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
        self.spawn(move |api| {
            let result = api.track_url(&track.full_id()).map_err(|e| format!("{e:#}"));
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
        self.report_end(End::Skip);
        self.queue = tracks;
        self.preload = None;
        self.source = Source::List(name.to_string());
        self.wave = None;
        self.pos = None;
        let start = (start..self.queue.len()).find(|&i| self.queue[i].available).unwrap_or(start);
        self.play_index(start);
        self.set_status(format!("Играет: {name}"));
    }

    fn start_wave(&mut self, station: Station) {
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
            Some(Msg::WaveStarted { station: id, name, result })
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
        self.covers.iter_mut().find(|(u, _)| *u == want).map(|(_, p)| p)
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
                PlayerEvent::Duration(d) => self.duration = d,
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
                    self.set_status("mpv завершился неожиданно");
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
            }
            Msg::Liked(r) => {
                self.liked_loading = false;
                match r {
                    Ok(list) => {
                        self.liked = list;
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
            Msg::Search(r) => {
                self.search_loading = false;
                match r {
                    Ok(list) => {
                        if list.is_empty() {
                            self.set_status("Ничего не найдено");
                        }
                        self.search_results = list;
                        self.search_state.select((!self.search_results.is_empty()).then_some(0));
                    }
                    Err(e) => self.set_status(format!("Ошибка поиска: {e}")),
                }
            }
            Msg::WaveStarted { station, name, result } => match result {
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
            Msg::Cover { url, result } => {
                self.covers_loading.remove(&url);
                if let (Ok(img), Some(picker)) = (result, &self.picker) {
                    // Keep the current, the next and one previous cover.
                    if self.covers.len() >= 3 {
                        self.covers.remove(0);
                    }
                    self.covers.push((url, picker.new_resize_protocol(img)));
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

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.search_editing {
            self.on_search_key(key);
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        let code = match key.code {
            KeyCode::Char(c) => KeyCode::Char(ru_to_en(c)),
            c => c,
        };
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
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
            KeyCode::Char('s') => self.shuffle_play(),
            KeyCode::Char('R') => {
                self.load_liked();
                self.load_stations();
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
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % 4],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + 3) % 4],
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

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.search_editing = false,
            KeyCode::Enter => {
                self.search_editing = false;
                let q = self.search_input.trim().to_string();
                if !q.is_empty() {
                    self.search_loading = true;
                    self.spawn(move |api| Some(Msg::Search(api.search(&q).map_err(|e| format!("{e:#}")))));
                }
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => self.search_input.clear(),
            KeyCode::Char(c) => self.search_input.push(c),
            _ => {}
        }
    }

    fn change_volume(&mut self, d: f64) {
        self.volume = (self.volume + d).clamp(0.0, 130.0);
        self.player.set_volume(self.volume);
    }

    fn selected_track(&self) -> Option<&Track> {
        match self.tab {
            Tab::Liked => self.liked_state.selected().and_then(|i| self.liked.get(i)),
            Tab::Search => self.search_state.selected().and_then(|i| self.search_results.get(i)),
            Tab::Wave => self.queue_state.selected().and_then(|i| self.queue.get(i)),
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
            Tab::Search => self.search_state.select(step(self.search_state.selected(), self.search_results.len())),
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
                    self.play_list("Поиск", self.search_results.clone(), i);
                }
            }
        }
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

    fn shuffle_play(&mut self) {
        let (name, mut list) = match self.tab {
            Tab::Search => ("Поиск (перемешано)", self.search_results.clone()),
            _ => ("Мне нравится (перемешано)", self.liked.clone()),
        };
        let mut seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
        for i in (1..list.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            list.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        self.play_list(name, list, 0);
    }

    pub fn save_volume(&self) -> u8 {
        self.volume.clamp(0.0, 100.0) as u8
    }
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
        assert_eq!(super::ru_to_en('x'), 'x');
    }

    /// Preload state machine (uses a real mpv). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn preload_flow() {
        use super::*;
        use crate::api::Account;
        let (tx, _rx) = std::sync::mpsc::channel();
        let player = Player::spawn(tx.clone(), 0).expect("mpv");
        let acc = Account { uid: "1".into(), login: "t".into(), name: "T".into(), has_plus: true };
        let mut app = App::new(Api::new("x"), acc, tx, player, 0);
        let t = |id: &str| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: "A".into(),
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

        // Stale response (older generation) is ignored.
        app.on_msg(Msg::Preload { gen: app.gen - 1, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert!(app.preload.is_none());
        // Fresh response is appended.
        app.on_msg(Msg::Preload { gen: app.gen, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert_eq!(app.preload, Some((1, "b".to_string())));

        // End of track: mpv moves on by itself, state follows without a new URL request.
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.pos, Some(1));
        assert!(app.from_preload && app.preload.is_none());

        // Manual skip uses the preloaded entry too.
        app.loading_track = false;
        app.on_msg(Msg::Preload { gen: app.gen, idx: 2, id: "c".into(), result: Ok("http://x/c".into()) });
        app.next(End::Skip);
        assert_eq!(app.pos, Some(2));

        // Error on a preloaded track retries the same track with a fresh URL.
        app.on_track_error();
        assert_eq!(app.pos, Some(2));
        assert!(!app.from_preload);
    }
}
