//! Application state and logic.

use crate::api::{
    default_stations, iso_now, my_wave, random_uuid, Account, Api, Link, PlayContext, PlayReport, Playlist, Station, Track,
    WaveBatch, MY_WAVE, QUALITIES,
};
use crate::player::{Audio, PlayerEvent};
use crate::input::Press;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub enum Msg {
    Player(PlayerEvent),
    /// A command from the desktop (media keys, player widgets).
    Remote(crate::media::Remote),
    Url { gen: u64, result: Result<String, String> },
    LikedIds(Vec<String>),
    Liked(Result<Vec<Track>, String>),
    Stations(Vec<Station>),
    /// Page `page` of the search for `query`: (tracks, total found).
    Search { query: String, page: usize, result: Result<(Vec<Track>, Option<usize>), String> },
    Playlists { req: u64, result: Result<Vec<Playlist>, String> },
    PlaylistOpened { key: String, req: u64, result: Result<Vec<Track>, String> },
    /// Result of renaming a playlist or changing its tracks: status text on success.
    PlaylistEdited { key: String, result: Result<String, String> },
    /// Tracks loaded in the background to be played as a list named `name`.
    ListLoaded {
        intent: u64,
        name: String,
        context: PlayContext,
        shuffle: bool,
        playlist: Option<String>,
        result: Result<Vec<Track>, String>,
    },
    WaveStarted { intent: u64, station: String, name: String, result: Result<WaveBatch, String> },
    WaveMore(Result<WaveBatch, String>),
    LikeFailed { track: Track, like: bool, err: String },
    Cover { url: String, result: Result<image::DynamicImage, String> },
    Preload { gen: u64, idx: usize, id: String, result: Result<String, String> },
    /// «Меньше ИИ-музыки» loaded from the account.
    LessAi(Result<bool, String>),
    /// Result of switching «Меньше ИИ-музыки» to `on`.
    LessAiSet { on: bool, result: Result<(), String> },
    Error(String),
    /// The link was sent to the terminal and, if `by_tool`, copied by a clipboard utility too.
    Copied { url: String, by_tool: bool },
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

/// The first track of the queue is the one that was playing when the list was switched with
/// «Доигрывать трек» on.
struct Carried {
    /// Wave it was played from: the end of the track is reported there.
    wave: Option<Wave>,
}

#[derive(Clone, Copy)]
enum End {
    Eof,
    Skip,
    /// Back to the previous track.
    Back,
    Dislike,
    /// Another track or list was chosen.
    Other,
    Silent,
}

/// Playback of the current track, reported to the listening history (see `Api::send_play`).
struct Play {
    report: PlayReport,
    /// Last position seen: played time grows by the steps between positions, jumps are seeks.
    last_pos: f64,
    /// The report after 30 seconds of listening has been sent.
    partial_sent: bool,
}

/// Seconds of listening after which a track is reported as listened, as the website does.
const PARTIAL_PLAY_SECS: f64 = 30.1;

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
    FinishTrack,
    Repeat,
    LessAi,
    Api,
    Art,
    Icons,
    About,
    Logout,
}

impl Setting {
    pub const ALL: [Setting; 11] = [
        Setting::Quality,
        Setting::Equalizer,
        Setting::Crossfade,
        Setting::FinishTrack,
        Setting::Repeat,
        Setting::LessAi,
        Setting::Api,
        Setting::Art,
        Setting::Icons,
        Setting::About,
        Setting::Logout,
    ];

    fn index(self) -> usize {
        Setting::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// Repeat mode (`T`): what plays after a track that has ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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

    /// Stored in the config.
    pub fn id(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }

    pub fn from_id(id: &str) -> Option<Repeat> {
        Repeat::ALL.into_iter().find(|r| r.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Repeat::Off => "выкл",
            Repeat::All => "вся очередь",
            Repeat::One => "один трек",
        }
    }
}

/// What «Сейчас играет» shows above the track.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Art {
    Cover,
    Spectrum,
    None,
}

impl Art {
    pub const ALL: [Art; 3] = [Art::Cover, Art::Spectrum, Art::None];

    /// Stored in the config.
    pub fn id(self) -> &'static str {
        match self {
            Art::Cover => "cover",
            Art::Spectrum => "spectrum",
            Art::None => "none",
        }
    }

    pub fn from_id(id: &str) -> Option<Art> {
        Art::ALL.into_iter().find(|a| a.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Art::Cover => "обложка",
            Art::Spectrum => "спектр",
            Art::None => "ничего",
        }
    }
}

/// How often the spectrum is asked for while a track plays.
const SPECTRUM_PERIOD: Duration = Duration::from_millis(80);
/// Band levels shown as empty and as full, dB (see `player::spectrum_levels`). Measured on several
/// tracks of different genres: a typical level is in the middle of a bar, loud parts reach its top.
const SPECTRUM_FLOOR: f32 = -90.0;
const SPECTRUM_CEIL: f32 = -66.0;
/// Music gets quieter towards high frequencies (about 1.5–2 dB per octave above 500 Hz in the same
/// measurements): bands above it are raised, so all bars move alike.
const SPECTRUM_TILT_FROM: f64 = 500.0;
const SPECTRUM_TILT_DB: f64 = 1.5;
/// A bar falls at most this much per query (it rises at once).
const SPECTRUM_FALL: f32 = 0.08;

/// Loudest volume, % (as mpv allowed, on the same cubic curve); kept between runs as is.
const MAX_VOLUME: f64 = 130.0;
/// A track normally starts within a few seconds. A bit longer than the player's own network
/// timeout (10 s), so that a connection it can detect as dead fails there first.
const LOAD_TIMEOUT: Duration = Duration::from_secs(12);

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
    /// The player has not started the current track yet: the positions it reports until then are
    /// of the previous one, still playing while the new link is resolved.
    awaiting_start: bool,

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
    /// Last submitted query, pages loaded so far and the total number of found tracks
    /// (`None`: not known yet, the website API tells it only on the last page).
    search_query: String,
    search_pages: usize,
    pub search_total: Option<usize>,
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
    /// A new list waits for the current track to end instead of cutting it off.
    pub finish_track: bool,
    pub repeat: Repeat,
    /// The list plays in random order (`s`, or shuffle turned on from the system): its own
    /// order, to go back to when shuffle is turned off.
    unshuffled: Option<Vec<Track>>,
    /// Nerd Font icons (false — plain Unicode symbols).
    pub icons: bool,
    /// What «Сейчас играет» shows above the track; covers are downloaded only for `Art::Cover`.
    pub art: Art,
    /// Spectrum bars, 0..1 per band, low to high.
    pub spectrum: Vec<f32>,
    spectrum_asked: Instant,
    /// The terminal keeps a drawn image until it is deleted: set when the cover is hidden.
    pub clear_images: bool,
    /// «Меньше ИИ-музыки» from the account settings; `None` until loaded.
    pub less_ai: Option<bool>,

    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,

    pub status: String,
    status_at: Instant,
    /// The status reports an error (see `set_error`).
    pub status_error: bool,
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

    /// Next track already resolved and appended to the player: (queue index, track id).
    preload: Option<(usize, String)>,
    preload_pending: bool,
    /// The player is loading the current track: if it has not started playing by then, it is restarted.
    load_deadline: Option<Instant>,
    /// The current track has already been restarted with a fresh link (after an error or a stall).
    retried: bool,
    /// Playback was stopped (the queue ran out, the wave is loading, too many errors).
    stopped: bool,
    /// Set while the first track of the queue is one carried over from the previous list.
    carried: Option<Carried>,
    /// Where the queue (a list, not the wave) comes from, for the listening history.
    context: PlayContext,
    play: Option<Play>,
    /// Tests: reports that would have been sent.
    #[cfg(test)]
    plays_sent: Vec<PlayReport>,
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
            awaiting_start: false,
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
            search_total: None,
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
            finish_track: false,
            repeat: Repeat::Off,
            unshuffled: None,
            icons: true,
            art: Art::Cover,
            spectrum: vec![0.0; crate::player::SPECTRUM_BANDS],
            spectrum_asked: Instant::now(),
            clear_images: false,
            less_ai: None,
            time_pos: 0.0,
            duration: 0.0,
            paused: false,
            volume: f64::from(volume),
            muted: false,
            status: String::new(),
            status_at: Instant::now(),
            status_error: false,
            tick: 0,
            picker: None,
            covers: Vec::new(),
            shown_cover: None,
            covers_loading: HashSet::new(),
            preload: None,
            preload_pending: false,
            load_deadline: None,
            retried: false,
            stopped: false,
            carried: None,
            context: PlayContext::default(),
            play: None,
            #[cfg(test)]
            plays_sent: Vec::new(),
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
        self.status_error = false;
    }

    /// A status shown as an error (in red).
    pub fn set_error(&mut self, s: impl Into<String>) {
        self.set_status(s);
        self.status_error = true;
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
            || self.spectrum.iter().any(|&v| v > 0.0)
    }

    /// The spectrum is shown and moves: the screen is redrawn more often.
    pub fn spectrum_moving(&self) -> bool {
        self.art == Art::Spectrum && self.spectrum.iter().any(|&v| v > 0.0)
    }

    /// A track is heard right now.
    fn playing(&self) -> bool {
        self.pos.is_some() && !self.paused && !self.loading_track && !self.stopped
    }

    /// New band levels (dBFS): bars rise at once and fall slowly.
    fn on_spectrum(&mut self, levels: &[f32]) {
        if !self.playing() {
            return;
        }
        for (i, (bar, &db)) in self.spectrum.iter_mut().zip(levels).enumerate() {
            let tilt = SPECTRUM_TILT_DB * (crate::player::spectrum_freq(i) / SPECTRUM_TILT_FROM).log2().max(0.0);
            let db = db + tilt as f32;
            let v = ((db - SPECTRUM_FLOOR) / (SPECTRUM_CEIL - SPECTRUM_FLOOR)).clamp(0.0, 1.0);
            *bar = v.max(*bar - SPECTRUM_FALL);
        }
    }

    /// Asks the player for the spectrum while a track plays; otherwise the bars go down.
    fn tick_spectrum(&mut self) {
        if self.art != Art::Spectrum || self.spectrum_asked.elapsed() < SPECTRUM_PERIOD {
            return;
        }
        self.spectrum_asked = Instant::now();
        if self.playing() {
            self.player.request_spectrum();
        } else {
            for bar in &mut self.spectrum {
                *bar = (*bar - SPECTRUM_FALL).max(0.0);
            }
        }
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
        let mut api = self.api.clone();
        api.less_ai = self.less_ai == Some(true);
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

    /// What the desktop's player widgets show, and the position in the track (seconds).
    pub fn media_state(&self) -> (crate::media::State, f64) {
        use crate::media::{State, Status, TrackInfo};
        let track = self.current().map(|t| TrackInfo {
            id: t.id.clone(),
            title: t.title.clone(),
            artists: t.artists.clone(),
            album: t.album.clone(),
            length: if self.duration > 0.0 { self.duration } else { t.duration_ms as f64 / 1000.0 },
            art_url: t.cover.clone(),
            url: t.web_url(),
        });
        let status = match () {
            _ if track.is_none() || self.stopped => Status::Stopped,
            _ if self.paused => Status::Paused,
            _ => Status::Playing,
        };
        let volume = if self.muted { 0.0 } else { self.volume };
        (State { track, status, volume, repeat: self.repeat, shuffle: self.shuffled() }, self.time_pos)
    }

    /// A command from the desktop: the same as the matching key.
    fn on_remote(&mut self, r: crate::media::Remote) {
        use crate::media::Remote;
        let has_track = self.pos.is_some() && !self.stopped;
        match r {
            Remote::PlayPause if has_track => self.player.toggle_pause(),
            Remote::Play if has_track && self.paused => self.player.toggle_pause(),
            Remote::Pause if has_track && !self.paused => self.player.toggle_pause(),
            Remote::Next => self.next(End::Skip),
            Remote::Previous => self.prev(),
            Remote::Seek(d) if has_track => self.player.seek(d),
            Remote::SeekTo(t) if has_track => self.player.seek(t - self.time_pos),
            Remote::Volume(v) => self.change_volume(v - self.volume),
            Remote::Repeat(r) => self.set_repeat(r),
            Remote::Shuffle(on) => self.set_shuffled(on),
            Remote::Quit => self.quit = true,
            _ => {}
        }
    }

    pub fn is_liked(&self, t: &Track) -> bool {
        self.liked_ids.contains(&t.id)
    }

    pub fn source_name(&self) -> String {
        match (&self.source, &self.wave) {
            (Source::Wave, Some(w)) => w.name.clone(),
            (Source::List(n), _) if self.shuffled() => format!("{n} (в случайном порядке)"),
            (Source::List(n), _) => n.clone(),
            _ => String::new(),
        }
    }

    // ------------------------------------------------------------ loading

    /// Loads all lists again (`R`, switching the API).
    fn reload_all(&mut self) {
        self.load_liked();
        self.load_stations();
        self.load_playlists();
        self.load_account_settings();
        self.reload_open_playlist();
    }

    /// A load already under way is not repeated (holding `R` would start one per key repeat).
    fn load_liked(&mut self) {
        if self.liked_loading {
            return;
        }
        self.liked_loading = true;
        let tx = self.tx.clone();
        self.spawn(move |api| {
            let (ids, tracks) = match api.liked() {
                Ok(r) => r,
                Err(e) => return Some(Msg::Liked(Err(format!("{e:#}")))),
            };
            let _ = tx.send(Msg::LikedIds(ids.clone()));
            let tracks = match tracks {
                Some(t) => Ok(t),
                None => api.tracks(&ids),
            };
            Some(Msg::Liked(tracks.map_err(|e| format!("{e:#}"))))
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
        let reason = match end {
            End::Eof => "finish",
            End::Skip | End::Silent => "skip",
            End::Back => "back_skip",
            End::Dislike => "dislike",
            End::Other => "other_track",
        };
        self.end_play(Some(reason));
        let wave = match &self.carried {
            Some(c) if self.pos == Some(0) => c.wave.as_ref(),
            _ if self.source == Source::Wave => self.wave.as_ref(),
            _ => None,
        };
        let (Some(w), Some(t)) = (wave, self.current()) else { return };
        let ev = match end {
            End::Eof => json!({"type": "trackFinished", "timestamp": iso_now(), "trackId": t.full_id(),
                "totalPlayedSeconds": self.duration.max(self.time_pos)}),
            End::Skip | End::Back | End::Dislike | End::Other => json!({"type": "skip", "timestamp": iso_now(),
                "trackId": t.full_id(), "totalPlayedSeconds": self.time_pos}),
            End::Silent => return,
        };
        self.feedback(w, ev, t.batch.clone());
    }

    // ------------------------------------------------------------ listening history

    /// A new playback of `track` begins: reported with nothing played yet.
    fn start_play(&mut self, track: &Track) {
        let (context, radio) = match (&self.source, &self.wave) {
            (Source::Wave, Some(w)) => (
                PlayContext::new("radio", w.station.clone()),
                Some((w.session.clone().unwrap_or_default(), track.batch.clone().unwrap_or_else(|| w.batch.clone()))),
            ),
            _ => (self.context.clone(), None),
        };
        let report = PlayReport {
            play_id: random_uuid(),
            track: track.clone(),
            context,
            radio,
            started: iso_now(),
            played: 0.0,
            end_position: 0.0,
            seek: false,
            pause: false,
            reason: None,
        };
        self.send_play(report.clone());
        self.play = Some(Play { report, last_pos: 0.0, partial_sent: false });
    }

    /// The position moved to `t`: counts the time played; sends a report once 30 s are played.
    fn track_play(&mut self, t: f64) {
        let paused = self.paused;
        let Some(play) = &mut self.play else { return };
        let step = t - play.last_pos;
        if (0.0..3.0).contains(&step) {
            if !paused {
                play.report.played += step;
            }
        } else {
            play.report.seek = true;
        }
        play.last_pos = t;
        play.report.end_position = t;
        if !play.partial_sent && play.report.played >= PARTIAL_PLAY_SECS {
            play.partial_sent = true;
            let report = PlayReport { reason: Some("in_progress"), ..play.report.clone() };
            self.send_play(report);
        }
    }

    /// Playback was paused: reported as it is now.
    fn pause_play(&mut self) {
        let Some(play) = &mut self.play else { return };
        play.report.pause = true;
        let report = PlayReport { reason: None, ..play.report.clone() };
        self.send_play(report);
    }

    /// The playback of the current track is over (`reason`: why), or the program quits (`None`).
    fn end_play(&mut self, reason: Option<&'static str>) -> Option<PlayReport> {
        let play = self.play.take()?;
        let report = PlayReport { reason, ..play.report };
        if reason.is_some() {
            self.send_play(report.clone());
        }
        Some(report)
    }

    /// Report for the moment the program quits; sent by the caller, which waits for it.
    pub fn quit_play(&mut self) -> Option<PlayReport> {
        let mut report = self.end_play(None)?;
        report.pause = true;
        Some(report)
    }

    fn send_play(&mut self, report: PlayReport) {
        #[cfg(test)]
        if self.offline {
            self.plays_sent.push(report);
            return;
        }
        self.spawn(move |api| {
            let _ = api.send_play(&report);
            None
        });
    }

    fn wave_feedback(&self, ev: Value, batch: Option<String>) {
        if let Some(w) = &self.wave {
            self.feedback(w, ev, batch);
        }
    }

    fn feedback(&self, w: &Wave, ev: Value, batch: Option<String>) {
        let (st, sess) = (w.station.clone(), w.session.clone());
        let batch = batch.unwrap_or_else(|| w.batch.clone());
        self.spawn(move |api| {
            let _ = api.wave_feedback(&st, sess.as_deref(), &batch, ev);
            None
        });
    }

    /// Starts track `idx`: resolves its URL and replaces what the player plays.
    fn play_index(&mut self, idx: usize) {
        if !self.begin_track(idx, false) {
            return;
        }
        let gen = self.gen;
        let id = self.queue[idx].full_id();
        let q = QUALITIES[self.quality].id;
        self.spawn(move |api| Some(Msg::Url { gen, result: api.track_url(&id, q).map_err(|e| format!("{e:#}")) }));
    }

    /// Updates state for a new current track. `preloaded`: the player already switched to it.
    fn begin_track(&mut self, idx: usize, preloaded: bool) -> bool {
        let Some(track) = self.queue.get(idx).cloned() else { return false };
        self.pos = Some(idx);
        self.queue_state.select(Some(idx));
        self.gen += 1;
        self.time_pos = 0.0;
        self.duration = track.duration_ms as f64 / 1000.0;
        self.loading_track = true;
        self.awaiting_start = true;
        self.preload = None;
        self.preload_pending = false;
        self.load_deadline = preloaded.then(|| Instant::now() + LOAD_TIMEOUT);
        self.retried = false;
        self.stopped = false;
        self.load_cover(track.cover.clone());
        self.maybe_extend_search_queue();
        // A restart of the same track (after an error) goes on with its playback.
        if self.play.as_ref().is_none_or(|p| p.report.track.id != track.id) {
            self.end_play(Some("skip"));
            self.start_play(&track);
        }
        // A track carried over from another list is not part of this wave.
        let carried = idx == 0 && self.carried.is_some();
        if self.source == Source::Wave && !carried {
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

    /// The track to play after the current one: the next available one, or with repeat the same
    /// one when it has ended (`Repeat::One`) or the first one after the last (`Repeat::All`).
    fn following(&self, end: End) -> Option<usize> {
        let available = |i: usize| self.queue[i].available;
        if self.repeat == Repeat::One && matches!(end, End::Eof) && self.pos.is_some_and(available) {
            return self.pos;
        }
        let from = self.pos.map_or(0, |p| p + 1);
        if let Some(i) = (from..self.queue.len()).find(|&i| available(i)) {
            return Some(i);
        }
        // A search queue goes on with its next pages first; a track carried over from another
        // list is not part of this one.
        let ended = matches!(self.source, Source::List(_)) && !(self.search_queue.is_some() && self.can_load_more_search());
        let carried = |i: usize| i == 0 && self.carried.is_some();
        if self.repeat == Repeat::All && ended {
            return (0..self.queue.len()).find(|&i| available(i) && !carried(i));
        }
        None
    }

    /// What follows the current track changed: the one already appended to the player is dropped.
    fn following_changed(&mut self) {
        self.preload = None;
        self.player.clear_next();
        self.schedule_preload();
    }

    /// Resolves the next track's URL in advance and appends it to the player.
    fn schedule_preload(&mut self) {
        if self.preload.is_some() || self.preload_pending || self.loading_track || self.pos.is_none() {
            return;
        }
        let Some(idx) = self.following(End::Eof) else { return };
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
    fn advance_preloaded(&mut self, end: End, player_advanced: bool) -> bool {
        let next = self.following(end);
        match &self.preload {
            Some((idx, id)) if Some(*idx) == next && self.queue[*idx].id == *id => {
                let idx = *idx;
                self.report_end(end);
                if !player_advanced {
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
        if let Some(i) = self.following(end) {
            self.play_index(i);
        } else if self.source == Source::Wave {
            if let Some(w) = &mut self.wave {
                w.pending_next = true;
            }
            self.fetch_more_wave();
            self.load_deadline = None;
            self.player.stop();
            self.stopped = true;
            self.set_status("Загрузка следующих треков волны…");
        } else {
            self.load_deadline = None;
            self.player.stop();
            self.stopped = true;
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
                self.report_end(End::Back);
                self.play_index(i);
            }
        }
    }

    fn play_list(&mut self, name: &str, context: PlayContext, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.new_intent();
        let start = (start..tracks.len()).find(|&i| tracks[i].available).unwrap_or(start);
        if self.finishing_track() {
            self.queue_after_current(tracks.into_iter().skip(start).collect(), Source::List(name.to_string()), None);
            self.context = context;
            self.set_status(format!("После этого трека: {name}"));
            return;
        }
        self.report_end(End::Other);
        self.context = context;
        self.queue = tracks;
        self.unshuffled = None;
        self.search_queue = None;
        self.preload = None;
        self.carried = None;
        self.playing_playlist = None;
        self.source = Source::List(name.to_string());
        self.wave = None;
        self.pos = None;
        self.play_index(start);
        self.set_status(format!("Играет: {name}"));
    }

    /// «Доигрывать трек»: a new list waits for the playing track to end (a paused one is
    /// switched at once: it is not being listened to).
    fn finishing_track(&self) -> bool {
        self.finish_track && self.pos.is_some() && !self.paused && !self.loading_track && !self.stopped
    }

    /// Replaces the queue with `tracks` but keeps the playing track as its first entry, so the
    /// list goes on from there when the track ends (`n` switches at once).
    fn queue_after_current(&mut self, tracks: Vec<Track>, source: Source, wave: Option<Wave>) {
        let Some(cur) = self.current().cloned() else { return };
        // Switched again before the track ended: it still belongs to its first list.
        let carried = match self.carried.take() {
            Some(c) if self.pos == Some(0) => c,
            _ => Carried { wave: if self.source == Source::Wave { self.wave.take() } else { None } },
        };
        self.carried = Some(carried);
        self.queue = std::iter::once(cur.clone()).chain(tracks.into_iter().filter(|t| t.id != cur.id)).collect();
        self.unshuffled = None;
        self.pos = Some(0);
        self.queue_state.select(Some(0));
        self.source = source;
        self.wave = wave;
        self.search_queue = None;
        self.playing_playlist = None;
        // The next track of the old list may already be in the player: replies for it are dropped too.
        self.gen += 1;
        self.preload = None;
        self.preload_pending = false;
        self.player.clear_next();
        self.schedule_preload();
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
        let id = station.id.clone();
        self.spawn(move |api| {
            let result = api.wave_start(&id, settings).map_err(|e| format!("{e:#}"));
            Some(Msg::WaveStarted { intent, station: id, name, result })
        });
    }

    fn load_cover(&mut self, url: Option<String>) {
        let Some(url) = url else { return };
        if self.art != Art::Cover
            || self.picker.is_none() || self.covers.iter().any(|(u, _)| *u == url) || !self.covers_loading.insert(url.clone())
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
        if self.source == Source::Wave && self.queue.iter().any(|t| t.id == track.id) {
            let kind = if like { "like" } else { "unlike" };
            self.wave_feedback(json!({"type": kind, "timestamp": iso_now(), "trackId": track.full_id()}), track.batch.clone());
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
        self.spawn(move |api| api.set_dislike(&t, true).err().map(|e| Msg::Error(format!("Ошибка дизлайка: {e:#}"))));
        self.next(End::Dislike);
    }

    // ------------------------------------------------------------ messages

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Remote(r) => self.on_remote(r),
            Msg::Player(ev) => match ev {
                PlayerEvent::TimePos(_) if self.awaiting_start => {}
                PlayerEvent::TimePos(t) => {
                    self.time_pos = t;
                    if !self.loading_track || t > 0.0 {
                        self.track_play(t);
                    }
                    if self.loading_track && t > 0.0 {
                        self.loading_track = false;
                        self.load_deadline = None;
                        self.errors_in_row = 0;
                        self.schedule_preload();
                    }
                }
                PlayerEvent::Duration(d) => {
                    self.awaiting_start = false;
                    // Unknown to the player: the length from the API stays.
                    if d > 0.0 {
                        self.duration = d;
                    }
                }
                PlayerEvent::Pause(p) => {
                    if p && !self.paused && !self.loading_track {
                        self.pause_play();
                    }
                    self.paused = p;
                }
                PlayerEvent::Volume(v) => self.volume = v,
                PlayerEvent::Spectrum(levels) => self.on_spectrum(&levels),
                PlayerEvent::Mute(m) => self.muted = m,
                PlayerEvent::Eof => {
                    // The player moves to the appended (preloaded) track on its own.
                    if !self.advance_preloaded(End::Eof, true) {
                        self.next(End::Eof);
                    }
                }
                PlayerEvent::Error(e) => {
                    self.set_error(format!("Ошибка воспроизведения: {e}"));
                    self.on_track_error();
                }
                PlayerEvent::Died(msg) => {
                    self.exit_error = Some(msg);
                    self.quit = true;
                }
            },
            Msg::Url { gen, result } => {
                if gen != self.gen {
                    return;
                }
                match result {
                    Ok(url) => {
                        self.player.load(&url);
                        self.load_deadline = Some(Instant::now() + LOAD_TIMEOUT);
                    }
                    Err(e) => {
                        self.set_error(format!("Не удалось получить трек: {e}"));
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
                    Err(e) => self.set_error(format!("Не удалось загрузить «Мне нравится»: {e}")),
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
                        let seen: HashSet<&str> = self.search_results.iter().map(|t| t.id.as_str()).collect();
                        let new: Vec<Track> = list.into_iter().filter(|t| !seen.contains(t.id.as_str())).collect();
                        if new.is_empty() && page > 0 {
                            // Nothing new: stop asking for more pages.
                            self.search_total = Some(self.search_results.len());
                        }
                        if let Some(shuffled) = self.search_queue {
                            let queued: HashSet<&str> = self.queue.iter().map(|t| t.id.as_str()).collect();
                            let mut more: Vec<Track> =
                                new.iter().filter(|t| !queued.contains(t.id.as_str())).cloned().collect();
                            if let Some(own) = &mut self.unshuffled {
                                own.extend(more.iter().cloned());
                            }
                            if shuffled {
                                shuffle_tracks(&mut more);
                            }
                            self.queue.extend(more);
                        }
                        self.search_results.extend(new);
                    }
                    Err(e) => self.set_error(format!("Ошибка поиска: {e}")),
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
                    Err(e) => self.set_error(format!("Не удалось загрузить плейлисты: {e}")),
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
                    Err(e) => self.set_error(format!("Не удалось открыть плейлист: {e}")),
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
                Err(e) => self.set_error(format!("Ошибка: {e}")),
            },
            Msg::ListLoaded { intent, .. } | Msg::WaveStarted { intent, .. } if intent != self.intent => {}
            Msg::ListLoaded { name, context, shuffle, playlist, result, .. } => match result {
                Ok(list) if list.is_empty() => self.set_status(format!("{name}: нет треков")),
                Ok(list) => {
                    if shuffle {
                        self.play_shuffled(&name, context, list);
                    } else {
                        self.play_list(&name, context, list, 0);
                    }
                    self.playing_playlist = playlist;
                    self.tab = Tab::Wave;
                }
                Err(e) => self.set_error(format!("{name}: не удалось загрузить треки: {e}")),
            },
            Msg::WaveStarted { station, name, result, .. } => match result {
                Ok(b) => {
                    let from = station.replace(':', "-");
                    let wave = Wave {
                        station,
                        name: name.clone(),
                        session: b.session_id,
                        batch: b.batch_id.clone(),
                        fetching: false,
                        pending_next: false,
                    };
                    let finishing = self.finishing_track();
                    if finishing {
                        self.queue_after_current(b.tracks, Source::Wave, Some(wave));
                    } else {
                        self.report_end(End::Other);
                        self.wave = Some(wave);
                        self.source = Source::Wave;
                        self.queue = b.tracks;
                        self.unshuffled = None;
                        self.search_queue = None;
                        self.preload = None;
                        self.carried = None;
                        self.pos = None;
                    }
                    self.wave_feedback(json!({"type": "radioStarted", "timestamp": iso_now(), "from": from}), None);
                    self.tab = Tab::Wave;
                    if finishing {
                        self.set_status(format!("После этого трека: {name}"));
                    } else {
                        self.set_status(format!("Играет: {name}"));
                        self.next(End::Silent);
                    }
                }
                Err(e) => self.set_error(format!("Не удалось запустить волну: {e}")),
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
                        let known: HashSet<&str> = self.queue.iter().map(|t| t.id.as_str()).collect();
                        let fresh: Vec<Track> = b.tracks.into_iter().filter(|t| !known.contains(t.id.as_str())).collect();
                        self.queue.extend(fresh);
                        if pending {
                            self.next(End::Silent);
                        } else {
                            self.schedule_preload();
                        }
                    }
                    Err(e) => self.set_error(format!("Волна: {e}")),
                }
            }
            Msg::LikeFailed { track, like, err } => {
                self.apply_like(&track, !like);
                self.set_error(format!("Ошибка лайка: {err}"));
            }
            Msg::Error(s) => self.set_error(s),
            // Without a utility it is up to the terminal (OSC 52), which may ignore it: the link
            // stays in the status to be selected by hand.
            Msg::Copied { url, by_tool } => self.set_status(if by_tool {
                format!("Ссылка скопирована: {url}")
            } else {
                format!("Ссылка передана терминалу (OSC 52): {url}")
            }),
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
                Err(e) => self.set_error(format!("Не удалось загрузить настройки аккаунта: {e}")),
            },
            Msg::LessAiSet { on, result } => {
                if let Err(e) = result {
                    // Only roll back if nothing changed it since.
                    if self.less_ai == Some(on) {
                        self.less_ai = Some(!on);
                    }
                    self.set_error(format!("Не удалось изменить «Меньше ИИ-музыки»: {e}"));
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
                    if self.following(End::Eof) == Some(idx) && self.queue[idx].id == id {
                        self.player.append(&url);
                        self.preload = Some((idx, id));
                    } else {
                        // The queue or the repeat mode changed meanwhile.
                        self.schedule_preload();
                    }
                }
            }
        }
    }

    /// Treats a track that has been loading for too long as failed (so it is restarted with a
    /// fresh link once): a server that sends the headers and then stalls makes the player reconnect
    /// forever without an error, and switching away and back used to be the only cure.
    pub fn tick(&mut self) {
        self.tick_spectrum();
        let Some(deadline) = self.load_deadline else { return };
        if self.paused {
            // A paused track is not expected to start: count from the moment it is resumed.
            self.load_deadline = Some(Instant::now() + LOAD_TIMEOUT);
            return;
        }
        if Instant::now() < deadline {
            return;
        }
        if self.retried {
            self.set_error("Трек не загрузился: сервер не отвечает");
        }
        self.on_track_error();
    }

    fn on_track_error(&mut self) {
        self.load_deadline = None;
        // The link could have expired (a preloaded one after a long pause) or the connection
        // stalled: retry once with a fresh one.
        if let Some(p) = self.pos.filter(|_| !self.retried) {
            self.play_index(p);
            self.retried = true;
            return;
        }
        self.loading_track = false;
        self.errors_in_row += 1;
        if self.errors_in_row <= 5 {
            self.next(End::Silent);
        } else {
            self.player.stop();
            self.stopped = true;
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

    /// A key press: hotkeys go by the physical key, text fields get what was typed.
    pub fn on_key(&mut self, key: impl Into<Press>) {
        let key = key.into();
        if is_ctrl(&key.hotkey, 'c') {
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
        let code = key.hotkey.code;
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
            KeyCode::Char('w') => self.start_wave(my_wave()),
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
            KeyCode::Char('T') => {
                let i = Repeat::ALL.iter().position(|r| *r == self.repeat).unwrap_or(0);
                self.set_repeat(Repeat::ALL[(i + 1) % Repeat::ALL.len()]);
            }
            KeyCode::Char('R') => {
                self.reload_all();
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
        !self.search_loading && self.search_pages > 0 && self.search_total.is_none_or(|t| self.search_results.len() < t)
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
        let context = PlayContext::new("search", self.search_query.clone());
        if shuffled {
            self.play_shuffled(name, context, tracks);
        } else {
            self.play_list(name, context, tracks, start);
        }
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

    fn on_search_key(&mut self, press: Press) {
        let key = press.typed;
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
                    self.search_total = None;
                    self.load_search_page();
                }
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            _ if is_ctrl(&press.hotkey, 'u') => self.search_input.clear(),
            KeyCode::Char(c) if is_typed(&key) => self.search_input.push(c),
            _ => {}
        }
    }

    pub fn set_eq(&mut self, idx: usize) {
        let idx = idx.min(crate::eq::PRESETS.len() - 1);
        self.eq = idx;
        self.player.set_eq(&crate::eq::PRESETS[idx]);
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

    pub fn set_art(&mut self, art: Art) {
        let was = self.art;
        self.art = art;
        // The analysis filter costs a little CPU: it runs only while the spectrum is shown.
        self.player.set_spectrum(art == Art::Spectrum);
        if art != Art::Spectrum {
            self.spectrum.iter_mut().for_each(|v| *v = 0.0);
        }
        if art == Art::Cover {
            let cover = self.current().and_then(|t| t.cover.clone());
            self.load_cover(cover);
        } else if was == Art::Cover {
            // The terminal keeps a drawn image until it is deleted.
            self.shown_cover = None;
            self.clear_images = true;
        }
    }

    /// Plays the list in random order; its own order is kept for turning shuffle off.
    fn play_shuffled(&mut self, name: &str, context: PlayContext, tracks: Vec<Track>) {
        let mut shuffled = tracks.clone();
        shuffle_tracks(&mut shuffled);
        let finishing = self.finishing_track();
        self.play_list(name, context, shuffled, 0);
        if matches!(&self.source, Source::List(n) if n == name) {
            self.unshuffled = Some(tracks);
            let when = if finishing { "После этого трека" } else { "Играет" };
            self.set_status(format!("{when}: {}", self.source_name()));
        }
    }

    /// The list plays in random order.
    pub fn shuffled(&self) -> bool {
        self.unshuffled.is_some()
    }

    /// Shuffle turned on or off from the system (MPRIS, Now Playing): the playing list is shuffled
    /// with the current track first, or put back in its own order; the current track goes on.
    fn set_shuffled(&mut self, on: bool) {
        if on == self.shuffled() {
            return;
        }
        let Some(pos) = self.pos.filter(|_| matches!(self.source, Source::List(_))) else { return };
        let cur = self.queue[pos].clone();
        if on {
            // A track carried over from another list stays first; any other moves there now.
            if pos != 0 {
                self.carried = None;
            }
            self.unshuffled = Some(self.queue.clone());
            self.queue = shuffled_from(std::mem::take(&mut self.queue), pos);
            self.pos = Some(0);
        } else if let Some(own) = self.unshuffled.take() {
            let carried = pos == 0 && self.carried.is_some();
            match own.iter().position(|t| t.id == cur.id).filter(|_| !carried) {
                Some(i) => {
                    self.queue = own;
                    self.pos = Some(i);
                }
                // Carried over from another list: it stays first, the list follows in its order.
                None => {
                    let id = cur.id.clone();
                    self.queue = std::iter::once(cur).chain(own.into_iter().filter(|t| t.id != id)).collect();
                    self.pos = Some(0);
                }
            }
        }
        if let Some(shuffled) = &mut self.search_queue {
            *shuffled = on;
        }
        self.queue_state.select(self.pos);
        self.following_changed();
        self.set_status(format!("В случайном порядке: {}", if on { "вкл" } else { "выкл" }));
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
        self.following_changed();
        let wave = if repeat == Repeat::All && self.source == Source::Wave { " · волна и так бесконечна" } else { "" };
        self.set_status(format!("Повтор: {}{wave}", repeat.name()));
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
            Setting::FinishTrack => if self.finish_track { "вкл" } else { "выкл" }.to_string(),
            Setting::Repeat => self.repeat.name().to_string(),
            Setting::Api => if self.api.web { "сайт" } else { "мобильный" }.to_string(),
            Setting::Art => self.art.name().to_string(),
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
            Setting::FinishTrack => self.finish_track = !self.finish_track,
            Setting::Repeat => {
                let i = Repeat::ALL.iter().position(|r| *r == self.repeat).unwrap_or(0);
                self.set_repeat(Repeat::ALL[cycle(i, Repeat::ALL.len())]);
            }
            Setting::Api => {
                self.api.web = !self.api.web;
                self.reload_all();
                self.set_status(format!("API: {} · списки обновляются…", if self.api.web { "сайт" } else { "мобильный" }));
            }
            Setting::Art => {
                let i = Art::ALL.iter().position(|a| *a == self.art).unwrap_or(0);
                self.set_art(Art::ALL[cycle(i, Art::ALL.len())]);
            }
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
                    self.report_end(End::Other);
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
                    self.play_list("Мне нравится", self.liked_context(), self.liked.clone(), i);
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
        let (key, name, tracks) = (open.pl.key(), format!("Плейлист: {}", open.pl.title), open.tracks.clone());
        if tracks.is_empty() {
            return;
        }
        let context = PlayContext::new("playlist", key.clone());
        if shuffle {
            self.play_shuffled(&name, context, tracks);
        } else {
            let start = self.open_state.selected().unwrap_or(0);
            self.play_list(&name, context, tracks, start);
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

    fn on_modal_key(&mut self, press: Press) {
        let Some(modal) = self.modal.take() else { return };
        let key = press.typed;
        // Text input gets characters as typed; elsewhere keys go by the physical key.
        let code = match &modal {
            Modal::Rename { .. } | Modal::OpenLink { .. } => key.code,
            _ => press.hotkey.code,
        };
        let clear = is_ctrl(&press.hotkey, 'u');
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
                _ if clear => self.modal = Some(Modal::Rename { pl, input: String::new() }),
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
                _ if clear => self.modal = Some(Modal::OpenLink { input: String::new() }),
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
            let (name, context) = (format!("Плейлист: {}", pl.title), PlayContext::new("playlist", pl.key()));
            Some(Msg::ListLoaded { intent, name, context, shuffle, playlist: Some(pl.key()), result })
        });
    }

    /// Copies the link to the selected (or current) track to the clipboard.
    fn copy_link(&mut self) {
        let Some(t) = self.selected_track().or(self.current()).cloned() else {
            self.set_status("Нет выбранного трека");
            return;
        };
        let url = t.web_url();
        crate::clipboard::osc52(&url);
        self.set_status(format!("Копирование: {url}"));
        self.spawn(move |_| Some(Msg::Copied { by_tool: crate::clipboard::copy_with_tool(&url), url }));
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
            let context = match &link {
                Link::Album(id) => PlayContext::new("album", id.clone()),
                // A login is not a uid: the history needs `uid:kind`.
                Link::Playlist { owner, kind } if owner.bytes().all(|b| b.is_ascii_digit()) => {
                    PlayContext::new("playlist", format!("{owner}:{kind}"))
                }
                _ => PlayContext::new("various", ""),
            };
            Some(Msg::ListLoaded { intent, name, context, shuffle: false, playlist: None, result })
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
            let context = PlayContext::new("artist", id);
            Some(Msg::ListLoaded { intent, name: format!("Исполнитель: {name}"), context, shuffle: false, playlist: None, result })
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
            let context = PlayContext::new("album", id);
            Some(Msg::ListLoaded { intent, name, context, shuffle: false, playlist: None, result: result.map(|(_, t)| t) })
        });
    }

    fn shuffle_play(&mut self) {
        if self.tab == Tab::Search {
            self.play_search("Поиск", self.search_results.clone(), 0, true);
        } else {
            self.play_shuffled("Мне нравится", self.liked_context(), self.liked.clone());
        }
    }

    /// «Мне нравится» is the user's playlist 3.
    fn liked_context(&self) -> PlayContext {
        PlayContext::new("playlist", format!("{}:3", self.account.uid))
    }

    pub fn save_volume(&self) -> u8 {
        self.volume.clamp(0.0, MAX_VOLUME) as u8
    }
}

/// The track at `first` goes to the start, the rest are shuffled.
fn shuffled_from(mut tracks: Vec<Track>, first: usize) -> Vec<Track> {
    if first < tracks.len() {
        let t = tracks.remove(first);
        tracks.insert(0, t);
        shuffle_tracks(&mut tracks[1..]);
    }
    tracks
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

/// Ctrl + the key `c` (by the physical key).
fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

#[cfg(test)]
mod tests {
    /// Keys inside pop-up dialogs.
    #[test]
    fn modal_keys() {
        use super::*;
        use ratatui::crossterm::event::KeyEvent;
        let (mut app, audio) = App::for_test();
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

        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert!(app.finish_track);
        app.on_key(key(KeyCode::Left));
        assert!(!app.finish_track);

        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.repeat, Repeat::All);
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.repeat, Repeat::One, "wraps around");
        app.set_repeat(Repeat::Off);

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

        // API: switched at once, both ways.
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.setting_value(Setting::Api).as_deref(), Some("мобильный"));
        app.on_key(key(KeyCode::Right));
        assert!(app.api.web);
        assert_eq!(app.setting_value(Setting::Api).as_deref(), Some("сайт"));
        app.on_key(key(KeyCode::Enter));
        assert!(!app.api.web);

        // Cover → spectrum → nothing → cover: the shown cover is dropped and new ones are not
        // downloaded; the analysis filter runs only for the spectrum.
        app.on_key(key(KeyCode::Char('j')));
        audio.take();
        app.on_key(key(KeyCode::Right));
        assert!(app.art == Art::Spectrum && app.clear_images);
        assert_eq!(audio.take(), ["spectrum true"]);
        app.picker = Some(Picker::from_fontsize((8, 16)));
        app.load_cover(Some("http://x/cover".into()));
        assert!(app.covers_loading.is_empty());
        app.on_key(key(KeyCode::Right));
        assert_eq!((app.art, audio.take()), (Art::None, vec!["spectrum false".to_string()]));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.art, Art::Cover);
        app.load_cover(Some("http://x/cover".into()));
        assert!(app.covers_loading.contains("http://x/cover"));
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.art, Art::None);
        app.on_key(key(KeyCode::Right));
        app.picker = None;

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
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 9 })));

        // Logout asks first; Esc goes back to the settings.
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::ConfirmLogout)));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 10 })));
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

    /// «Доигрывать трек»: a new list goes after the playing track.
    #[test]
    fn finish_track() {
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
        let ids = |app: &App| app.queue.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        app.finish_track = true;
        app.queue = vec![t("a"), t("b")];
        app.source = Source::Wave;
        app.wave = Some(Wave {
            station: "user:onyourwave".into(),
            name: "Моя волна".into(),
            session: None,
            batch: String::new(),
            fetching: false,
            pending_next: false,
        });
        app.begin_track(0, false);
        app.loading_track = false;
        app.on_msg(Msg::Preload { gen: app.gen, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        audio.take();

        // The playing track stays and comes first; the new list starts from the chosen track,
        // without a second copy of the playing one; the player drops the preloaded old next track.
        let gen = app.gen;
        app.play_list("Новый", PlayContext::default(), vec![t("x"), t("y"), t("a"), t("z")], 1);
        assert_eq!(ids(&app), ["a", "y", "z"]);
        assert_eq!((app.pos, app.source_name()), (Some(0), "Новый".to_string()));
        assert!(app.wave.is_none() && app.preload.is_none());
        assert_eq!(audio.take(), ["clear"]);
        app.on_msg(Msg::Preload { gen, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert!(audio.take().is_empty(), "late preload of the old list is dropped");
        assert!(app.carried.as_ref().is_some_and(|c| c.wave.is_some()), "its end is reported to the old wave");

        // Another list before the track ends replaces the waiting one; the track keeps its wave.
        app.play_list("Ещё", PlayContext::default(), vec![t("p"), t("q")], 0);
        assert_eq!(ids(&app), ["a", "p", "q"]);
        assert!(app.carried.as_ref().is_some_and(|c| c.wave.is_some()));

        // The track ends: the new list plays.
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!((app.pos, app.loading_track), (Some(1), true));
        app.loading_track = false;

        // Paused: switched at once.
        app.paused = true;
        app.play_list("Сразу", PlayContext::default(), vec![t("m"), t("n")], 1);
        assert_eq!((ids(&app), app.pos), (vec!["m".to_string(), "n".to_string()], Some(1)));
        assert!(app.carried.is_none());
        app.paused = false;
        app.loading_track = false;

        // Off: switched at once.
        app.finish_track = false;
        app.play_list("Выкл", PlayContext::default(), vec![t("k")], 0);
        assert_eq!((ids(&app), app.pos), (vec!["k".to_string()], Some(0)));

        // Nothing plays after the queue ran out: switched at once.
        app.finish_track = true;
        app.loading_track = false;
        app.next(End::Eof);
        app.play_list("Снова", PlayContext::default(), vec![t("u"), t("v")], 0);
        assert_eq!((ids(&app), app.pos), (vec!["u".to_string(), "v".to_string()], Some(0)));
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

        // End of track: the player moves on by itself, state follows without a new URL request.
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.pos, Some(1));
        assert!(!app.retried && app.preload.is_none());
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
        assert!(app.retried);
    }

    #[test]
    fn shuffle_from_system() {
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
        let ids = |app: &App| app.queue.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        let own: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        app.play_list("Альбом", PlayContext::default(), own.iter().map(|id| t(id)).collect(), 5);
        app.loading_track = false;
        assert!(!app.shuffled());

        // On: the current track goes first, the rest are shuffled; the name tells it.
        use crate::media::Remote;
        app.on_remote(Remote::Shuffle(true));
        assert!(app.shuffled());
        assert_eq!((app.pos, app.queue[0].id.as_str()), (Some(0), "5"));
        assert_ne!(ids(&app), own);
        let mut sorted = ids(&app);
        sorted.sort_by_key(|id| id.parse::<u32>().unwrap());
        assert_eq!(sorted, own);
        assert_eq!(app.source_name(), "Альбом (в случайном порядке)");
        assert!(app.media_state().0.shuffle);

        // Off: the list's own order, the current track where it belongs.
        app.begin_track(3, false);
        let cur = app.queue[3].id.clone();
        app.on_remote(Remote::Shuffle(false));
        assert_eq!(ids(&app), own);
        assert_eq!(app.current().map(|t| t.id.clone()), Some(cur));
        assert_eq!(app.source_name(), "Альбом");

        // `s` plays a list shuffled: shuffle shows as on and turns off to the list's order.
        app.liked = own.iter().map(|id| t(id)).collect();
        app.tab = Tab::Liked;
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.shuffled());
        app.on_remote(Remote::Shuffle(false));
        assert_eq!(ids(&app), own);

        // A wave is not shuffled.
        app.source = Source::Wave;
        app.on_remote(Remote::Shuffle(true));
        assert!(!app.shuffled());
    }

    #[test]
    fn repeat_modes() {
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

        // One track: the ended track plays again, the preload asks for it; `n` still moves on.
        app.set_repeat(Repeat::One);
        assert_eq!(app.following(End::Eof), Some(0));
        assert_eq!(app.following(End::Skip), Some(1));
        assert_eq!(audio.take(), ["clear"]);
        app.on_msg(Msg::Preload { gen: app.gen, idx: 0, id: "a".into(), result: Ok("http://x/a".into()) });
        assert_eq!(app.preload, Some((0, "a".to_string())));
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.pos, Some(0));
        assert!(app.play.is_some(), "a new playback is reported");
        // Switching the mode drops the preloaded repeat.
        app.loading_track = false;
        app.on_msg(Msg::Preload { gen: app.gen, idx: 0, id: "a".into(), result: Ok("http://x/a".into()) });
        audio.take();
        app.set_repeat(Repeat::Off);
        assert!(app.preload.is_none());
        assert_eq!(audio.take(), ["clear"]);

        // The whole queue: after the last track — the first one.
        app.begin_track(2, false);
        app.loading_track = false;
        assert_eq!(app.following(End::Eof), None);
        app.set_repeat(Repeat::All);
        assert_eq!(app.following(End::Eof), Some(0));
        assert_eq!(app.following(End::Skip), Some(0));
        // A wave has no end to go round.
        app.source = Source::Wave;
        assert_eq!(app.following(End::Eof), None);
        app.source = Source::List("test".into());
        app.next(End::Eof);
        assert_eq!(app.pos, Some(0));
    }

    #[test]
    fn stalled_load() {
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
        app.queue = vec![t("a"), t("b")];
        app.source = Source::List("test".into());
        app.begin_track(0, false);
        let past = || Some(Instant::now() - Duration::from_secs(1));
        app.on_msg(Msg::Url { gen: app.gen, result: Ok("http://x/a".into()) });
        assert_eq!(audio.take(), ["load http://x/a"]);

        // Still loading while paused: the timer restarts instead of firing.
        app.load_deadline = past();
        app.paused = true;
        app.tick();
        assert!(app.load_deadline.is_some_and(|d| d > Instant::now()));
        app.paused = false;

        // First stall: the same track is requested again (fresh link).
        let gen = app.gen;
        app.load_deadline = past();
        app.tick();
        assert_eq!((app.pos, app.gen, app.retried), (Some(0), gen + 1, true));
        assert!(!app.status_error);
        assert!(app.load_deadline.is_none());

        // Second stall: an error, then the next track.
        app.on_msg(Msg::Url { gen: app.gen, result: Ok("http://x/a2".into()) });
        app.load_deadline = past();
        app.tick();
        assert_eq!(app.pos, Some(1));
        assert!(app.status_error);
        assert!(!app.retried && app.load_deadline.is_none());

        // Once playback starts, the watchdog is off.
        app.on_msg(Msg::Url { gen: app.gen, result: Ok("http://x/b".into()) });
        app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(0.5)));
        assert!(app.load_deadline.is_none());
    }

    #[test]
    fn spectrum_bars() {
        use super::*;
        let (mut app, audio) = App::for_test();
        app.set_art(Art::Spectrum);
        let n = crate::player::SPECTRUM_BANDS;
        // Nothing plays: nothing is asked for and levels are ignored.
        app.spectrum_asked -= SPECTRUM_PERIOD;
        audio.take();
        app.tick();
        app.on_msg(Msg::Player(PlayerEvent::Spectrum(vec![0.0; n])));
        assert!(!app.spectrum_moving() && !audio.take().contains(&"spectrum?".to_string()));

        app.queue = vec![Track::from_json(&json!({"id": 1})).unwrap()];
        app.pos = Some(0);
        app.spectrum_asked -= SPECTRUM_PERIOD;
        app.tick();
        assert!(audio.take().contains(&"spectrum?".to_string()));
        // Bars rise at once, fall slowly; the floor and the ceiling clamp them.
        let mut levels = vec![SPECTRUM_FLOOR - 10.0; n];
        levels[0] = SPECTRUM_CEIL + 5.0;
        levels[1] = (SPECTRUM_FLOOR + SPECTRUM_CEIL) / 2.0;
        app.on_msg(Msg::Player(PlayerEvent::Spectrum(levels.clone())));
        assert_eq!((app.spectrum[0], app.spectrum[1], app.spectrum[2]), (1.0, 0.5, 0.0));
        levels[0] = SPECTRUM_FLOOR;
        app.on_msg(Msg::Player(PlayerEvent::Spectrum(levels)));
        assert!((app.spectrum[0] - (1.0 - SPECTRUM_FALL)).abs() < 1e-6);
        assert!(app.spectrum_moving());
        // Paused: the bars go down on their own.
        app.paused = true;
        for _ in 0..20 {
            app.spectrum_asked -= SPECTRUM_PERIOD;
            app.tick();
        }
        assert!(!app.spectrum_moving());
        // Another mode clears them.
        app.spectrum[3] = 0.7;
        app.set_art(Art::None);
        assert!(app.spectrum.iter().all(|&v| v == 0.0));
    }

    /// Presses as the kitty keyboard protocol gives them: the physical key decides hotkeys (any
    /// layout, Caps Lock ignored), text fields get what was typed.
    #[test]
    fn physical_keys() {
        use super::*;
        let press = |typed: char, hotkey: char, mods: KeyModifiers| Press {
            typed: KeyEvent::new(KeyCode::Char(typed), mods),
            hotkey: KeyEvent::new(KeyCode::Char(hotkey), mods),
        };
        let (mut app, _audio) = App::for_test();
        // `o` with Caps Lock in the Russian layout: «Щ», the settings window all the same.
        app.on_key(press('Щ', 'o', KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::Settings { .. })));
        app.on_key(KeyCode::Esc);
        // The search field takes the letters as typed; Ctrl+U (Ctrl+Г) clears it.
        app.tab = Tab::Search;
        app.search_editing = true;
        app.on_key(press('Д', 'l', KeyModifiers::NONE));
        app.on_key(press('а', 'f', KeyModifiers::NONE));
        assert_eq!(app.search_input, "Да");
        app.on_key(press('г', 'u', KeyModifiers::CONTROL));
        assert_eq!(app.search_input, "");
    }

    /// While the link of the next track is resolved the previous one still plays: its positions
    /// are not the new track's, and do not count as its start.
    #[test]
    fn positions_of_the_previous_track_are_ignored() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        let t = |id: &str| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: String::new(),
            main_artist: None,
            album: String::new(),
            duration_ms: 200_000,
            available: true,
            cover: None,
            batch: None,
        };
        app.play_list("Альбом", PlayContext::new("album", "9"), vec![t("a"), t("b")], 0);
        app.on_msg(Msg::Player(PlayerEvent::Duration(180.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(120.0)));
        assert_eq!((app.time_pos, app.loading_track), (120.0, false));
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(120.5)));
        assert_eq!((app.time_pos, app.loading_track), (0.0, true));
        // «b» starts; its length is unknown to the player, the one from the API stays.
        app.on_msg(Msg::Player(PlayerEvent::Duration(0.0)));
        assert_eq!(app.duration, 200.0);
        app.on_msg(Msg::Player(PlayerEvent::TimePos(0.25)));
        assert_eq!((app.time_pos, app.loading_track), (0.25, false));
    }

    #[test]
    fn play_reports() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        let t = |id: &str| Track {
            id: id.to_string(),
            album_id: Some("9".into()),
            title: id.to_string(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 200_000,
            available: true,
            cover: None,
            batch: None,
        };
        // The player reports the start of a track (`Duration`) before its positions.
        let pos = |app: &mut App, t: f64| {
            app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
            app.on_msg(Msg::Player(PlayerEvent::TimePos(t)));
        };
        app.play_list("Альбом", PlayContext::new("album", "9"), vec![t("a"), t("b"), t("c")], 0);
        let start = app.plays_sent.last().unwrap().clone();
        assert_eq!((start.track.id.as_str(), start.reason, start.played), ("a", None, 0.0));
        assert_eq!(start.context, PlayContext::new("album", "9"));

        // 30 seconds of listening are reported once, with the same play id.
        for i in 1..=70 {
            pos(&mut app, f64::from(i) * 0.5);
        }
        assert_eq!(app.plays_sent.len(), 2);
        let partial = &app.plays_sent[1];
        assert_eq!((partial.play_id.as_str(), partial.reason), (start.play_id.as_str(), Some("in_progress")));
        assert!((30.1..31.0).contains(&partial.played), "{}", partial.played);

        // A seek does not count as listening; a pause is reported.
        pos(&mut app, 150.0);
        app.on_msg(Msg::Player(PlayerEvent::Pause(true)));
        pos(&mut app, 150.0);
        app.on_msg(Msg::Player(PlayerEvent::Pause(false)));
        let paused = app.plays_sent.last().unwrap();
        assert_eq!((paused.reason, paused.pause, paused.seek), (None, true, true));
        assert_eq!(app.plays_sent.len(), 3);

        // `n`: the end of «a» (skip) and the start of «b».
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        let end = &app.plays_sent[3];
        assert_eq!((end.track.id.as_str(), end.reason, end.end_position), ("a", Some("skip"), 150.0));
        assert!((34.0..36.0).contains(&end.played), "{}", end.played);
        let b = app.plays_sent[4].clone();
        assert_eq!((b.track.id.as_str(), b.reason), ("b", None));
        assert_ne!(b.play_id, start.play_id);

        // A restart of the same track after an error goes on with its playback.
        pos(&mut app, 1.0);
        app.on_track_error();
        assert_eq!(app.plays_sent.len(), 5);

        // The end of the file, then back to the previous track.
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.plays_sent[5].reason, Some("finish"));
        assert_eq!(app.plays_sent[6].track.id, "c");
        pos(&mut app, 1.0);
        app.on_key(KeyEvent::from(KeyCode::Char('b')));
        assert_eq!((app.plays_sent[7].track.id.as_str(), app.plays_sent[7].reason), ("c", Some("back_skip")));

        // Another list ends the track as `other_track`; quitting gives the last report to send.
        app.play_list("Мне нравится", app.liked_context(), vec![t("x")], 0);
        let other = &app.plays_sent[9];
        assert_eq!((other.track.id.as_str(), other.reason), ("b", Some("other_track")));
        assert_eq!(app.plays_sent[10].context, PlayContext::new("playlist", "1:3"));
        let quit = app.quit_play().unwrap();
        assert_eq!((quit.track.id.as_str(), quit.reason, quit.pause), ("x", None, true));
        assert!(app.quit_play().is_none());
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
        app.on_msg(Msg::Search { query: "q".into(), page: 0, result: Ok((page(0..100), Some(250))) });
        assert_eq!((app.search_results.len(), app.search_total, app.search_loading), (100, Some(250), false));

        // Far from the end: nothing more is requested.
        app.move_sel(10);
        assert!(!app.search_loading);
        // Close to the end: the next page is requested.
        app.move_sel(75);
        assert!(app.search_loading);

        // Replies to another query or page are ignored.
        app.on_msg(Msg::Search { query: "old".into(), page: 1, result: Ok((page(500..600), Some(900))) });
        app.on_msg(Msg::Search { query: "q".into(), page: 0, result: Ok((page(500..600), Some(900))) });
        assert_eq!(app.search_results.len(), 100);

        // Overlapping tracks are not duplicated; selection stays in place.
        app.on_msg(Msg::Search { query: "q".into(), page: 1, result: Ok((page(90..190), Some(250))) });
        assert_eq!(app.search_results.len(), 190);
        assert_eq!(app.search_state.selected(), Some(85));

        // Playing from the search: later pages go to the queue, playing near its end loads more.
        app.search_total = Some(400);
        app.search_state.select(Some(0));
        app.activate();
        assert_eq!((app.queue.len(), app.pos, app.search_loading), (190, Some(0), false));
        app.begin_track(175, false);
        assert!(app.search_loading);
        app.on_msg(Msg::Search { query: "q".into(), page: 2, result: Ok((page(180..290), Some(400))) });
        assert_eq!((app.search_results.len(), app.queue.len()), (290, 290));
        assert_eq!(app.queue[289].id, "289");
        // A new search does not touch the playing queue.
        app.search_input = "new".into();
        app.on_search_key(KeyCode::Enter.into());
        app.on_msg(Msg::Search { query: "new".into(), page: 0, result: Ok((page(1000..1100), Some(900))) });
        assert_eq!(app.queue.len(), 290);
        app.search_query = "q".into();
        app.search_pages = 3;
        app.search_results = page(0..290);

        // A page without new tracks ends the paging.
        app.search_loading = true;
        app.on_msg(Msg::Search { query: "q".into(), page: 3, result: Ok((page(0..100), Some(400))) });
        assert_eq!(app.search_total, Some(290));
        app.move_sel(1000);
        assert!(!app.search_loading);

        // The website API does not count the results: pages are loaded until the last one.
        app.search_input = "web".into();
        app.on_search_key(KeyCode::Enter.into());
        app.on_msg(Msg::Search { query: "web".into(), page: 0, result: Ok((page(0..100), None)) });
        assert_eq!(app.search_total, None);
        app.move_sel(1000);
        assert!(app.search_loading);
        app.on_msg(Msg::Search { query: "web".into(), page: 1, result: Ok((page(100..130), Some(130))) });
        assert_eq!((app.search_results.len(), app.search_total), (130, Some(130)));
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

    /// Errors are marked by a flag, not by the wording; copying reports how the link was copied.
    #[test]
    fn status_kinds() {
        use super::*;
        let (mut app, _audio) = App::for_test();
        app.on_msg(Msg::Error("Ошибка дизлайка: 500".into()));
        assert!(app.status_error);
        app.set_status("Играет: Ошибка 404");
        assert!(!app.status_error);
        let url = "https://music.yandex.ru/track/1";
        app.on_msg(Msg::Copied { url: url.into(), by_tool: false });
        assert_eq!(app.status, format!("Ссылка передана терминалу (OSC 52): {url}"));
        app.on_msg(Msg::Copied { url: url.into(), by_tool: true });
        assert_eq!(app.status, format!("Ссылка скопирована: {url}"));
        assert!(!app.status_error);
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
            context: PlayContext::default(),
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
        app.play_list("Мне нравится", PlayContext::default(), vec![t("c")], 0);
        app.on_msg(list(link, "Трек по ссылке", vec![t("d")]));
        assert_eq!(app.queue[0].id, "c");
    }
}
