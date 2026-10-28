//! Application state and logic.

use crate::api::{
    ApiError,
    default_stations, iso_now, my_wave, random_uuid, Account, Api, PlayContext, PlayReport, Playlist, Station, Track,
    WaveBatch, MY_WAVE, QUALITIES,
};
use crate::api::lastfm::{self, Scrobble};
use crate::effect::{Effect, Task};
use crate::player::{PlayerCmd, PlayerEvent, TrackFile};
use crate::input::Press;
use crate::keymap::{self, Action};
use crate::library::{Likes, Page, Playlists, Search};
pub use crate::queue::{Repeat, Source};
use crate::queue::Queue;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub enum Msg {
    Player(PlayerEvent),
    /// A command from the desktop (media keys, player widgets).
    Remote(crate::media::Remote),
    Url { generation: u64, result: Result<String, ApiError> },
    LikedIds(Vec<String>),
    Liked(Result<Vec<Track>, ApiError>),
    Stations(Vec<Station>),
    /// Page `page` of the search for `query`: (tracks, total found).
    Search { query: String, page: usize, result: Result<(Vec<Track>, Option<usize>), ApiError> },
    Playlists { req: u64, result: Result<Vec<Playlist>, ApiError> },
    PlaylistOpened { key: String, req: u64, result: Result<Vec<Track>, ApiError> },
    /// Result of renaming a playlist or changing its tracks: status text on success.
    PlaylistEdited { key: String, result: Result<String, ApiError> },
    /// Tracks loaded in the background to be played as a list named `name`.
    ListLoaded {
        intent: u64,
        name: String,
        context: PlayContext,
        shuffle: bool,
        playlist: Option<String>,
        result: Result<Vec<Track>, ApiError>,
    },
    WaveStarted { intent: u64, station: String, name: String, result: Result<WaveBatch, ApiError> },
    WaveMore(Result<WaveBatch, ApiError>),
    LikeFailed { track: Track, like: bool, err: ApiError },
    Cover { url: String, result: Result<image::DynamicImage, ApiError> },
    Preload { generation: u64, idx: usize, id: String, result: Result<String, ApiError> },
    /// «Меньше ИИ-музыки» loaded from the account.
    LessAi(Result<bool, ApiError>),
    /// Result of switching «Меньше ИИ-музыки» to `on`.
    LessAiSet { on: bool, result: Result<(), ApiError> },
    Error(String),
    /// The link was sent to the terminal and, if `by_tool`, copied by a clipboard utility too.
    Copied { url: String, by_tool: bool },
    /// A Yandex Music link found in the clipboard for «Играть по ссылке».
    ClipboardLink(String),
    /// A link opened in the browser (`ok`) or not.
    Opened { url: String, ok: bool },
    /// A Last.fm token to be confirmed in the browser.
    LastfmToken(Result<String, lastfm::Error>),
    /// The session for a confirmed `token`: (user name, session key).
    LastfmSession { token: String, result: Result<(String, String), lastfm::Error> },
    /// «Now playing» (no `tracks`) or scrobbles were sent with `session`.
    LastfmSent { session: String, tracks: Vec<Scrobble>, result: Result<(), lastfm::Error> },
    /// For the main loop (never passed to `App::on_msg`): a key or pasted text, the window
    /// resized, SIGTERM.
    Input(crate::input::Input),
    Resized,
    Terminate,
}

impl Msg {
    /// The error a request ended with, if this is its answer.
    fn error(&self) -> Option<&ApiError> {
        match self {
            Msg::Url { result: Err(e), .. }
            | Msg::Liked(Err(e))
            | Msg::Search { result: Err(e), .. }
            | Msg::Playlists { result: Err(e), .. }
            | Msg::PlaylistOpened { result: Err(e), .. }
            | Msg::PlaylistEdited { result: Err(e), .. }
            | Msg::ListLoaded { result: Err(e), .. }
            | Msg::WaveStarted { result: Err(e), .. }
            | Msg::WaveMore(Err(e))
            | Msg::LikeFailed { err: e, .. }
            | Msg::Cover { result: Err(e), .. }
            | Msg::Preload { result: Err(e), .. }
            | Msg::LessAi(Err(e))
            | Msg::LessAiSet { result: Err(e), .. } => Some(e),
            _ => None,
        }
    }
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
    /// When the playback started, Unix time: the scrobble's timestamp.
    started_unix: u64,
    /// Last.fm has been told the track plays.
    now_playing_sent: bool,
    scrobbled: bool,
}

/// A connected Last.fm account, kept in the config.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LastfmSession {
    pub name: String,
    pub key: String,
}

/// How long the user has to confirm the access to Last.fm in the browser.
const LASTFM_CONFIRM: Duration = Duration::from_secs(300);
/// How often the program asks whether the access has been confirmed.
const LASTFM_POLL: Duration = Duration::from_secs(3);

pub enum Lastfm {
    Off,
    /// Waiting for a token (`None`), then for the user to confirm it in the browser by `until`.
    Connecting { token: Option<String>, until: Instant, poll_at: Instant, polling: bool },
    On(LastfmSession),
}

/// Seconds of listening after which a track is reported as listened, as the website does.
const PARTIAL_PLAY_SECS: f64 = 30.1;

/// Pop-up dialogs; while one is shown it receives all keys.
pub enum Modal {
    /// Choose a playlist to add `track` to.
    AddTo { track: Track, choices: Vec<Playlist>, sel: usize },
    ConfirmRemove { pl: Playlist, index: usize, track: Track },
    ConfirmDelete { pl: Playlist },
    Rename { pl: Playlist, input: String },
    /// Name of a new playlist; `track` is added to it once created.
    NewPlaylist { input: String, track: Option<Track> },
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
    Output,
    FinishTrack,
    Repeat,
    LessAi,
    Api,
    Art,
    Icons,
    RemainingTime,
    HideAccount,
    Lastfm,
    About,
    Logout,
}

impl Setting {
    pub const ALL: [Setting; 15] = [
        Setting::Quality,
        Setting::Equalizer,
        Setting::Crossfade,
        Setting::Output,
        Setting::FinishTrack,
        Setting::Repeat,
        Setting::LessAi,
        Setting::Api,
        Setting::Art,
        Setting::Icons,
        Setting::RemainingTime,
        Setting::HideAccount,
        Setting::Lastfm,
        Setting::About,
        Setting::Logout,
    ];

    fn index(self) -> usize {
        Setting::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// What «Сейчас играет» shows above the track. Stored in the config as `cover`, `spectrum` or
/// `none`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Art {
    Cover,
    Spectrum,
    None,
}

impl Art {
    pub const ALL: [Art; 3] = [Art::Cover, Art::Spectrum, Art::None];

    pub fn name(self) -> &'static str {
        match self {
            Art::Cover => "обложка",
            Art::Spectrum => "спектр",
            Art::None => "ничего",
        }
    }
}

/// Where the sound goes. Stored in the config as `auto`, `pulse` or `alsa`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Output {
    /// Linux: PulseAudio or PipeWire, ALSA without a sound server. macOS: Core Audio.
    #[default]
    Auto,
    /// The PulseAudio protocol: PulseAudio itself or PipeWire's pulse server.
    Pulse,
    /// Straight into the sound card, Linux only.
    Alsa,
}

impl Output {
    #[cfg(target_os = "linux")]
    pub const ALL: &[Output] = &[Output::Auto, Output::Pulse, Output::Alsa];
    #[cfg(not(target_os = "linux"))]
    pub const ALL: &[Output] = &[Output::Auto, Output::Pulse];

    pub fn name(self) -> &'static str {
        match self {
            Output::Auto if cfg!(target_os = "macos") => "Core Audio",
            Output::Auto => "авто",
            Output::Pulse if cfg!(target_os = "macos") => "PulseAudio",
            Output::Pulse => "PulseAudio/PipeWire",
            Output::Alsa => "ALSA",
        }
    }

    /// As the player names it (`player::Options::output`).
    pub fn id(self) -> Option<String> {
        match self {
            Output::Auto => None,
            Output::Pulse => Some("pulse".into()),
            Output::Alsa => Some("alsa".into()),
        }
    }

    /// From the player's name (or `YMUSIC_OUTPUT`); one not available here is the usual one.
    pub fn from_id(id: Option<&str>) -> Output {
        let o = match id {
            Some("pulse") => Output::Pulse,
            Some("alsa") => Output::Alsa,
            _ => Output::Auto,
        };
        if Output::ALL.contains(&o) { o } else { Output::Auto }
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
/// Tries of a track that does not come because of the network (no connection, the server does
/// not answer) before the next one is played; other failures get one more try.
const NETWORK_TRIES: u32 = 3;
/// A moment for the connection to come back before the track is tried again.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Fade lengths offered in the settings window, seconds (0 — off).
pub const CROSSFADE_STEPS: [u8; 6] = [0, 3, 5, 8, 12, 15];

pub enum StationRow {
    Header(String),
    Station(Station),
}

pub struct App {
    pub api: Api,
    /// What the logic has asked for since the last `take_effects`.
    effects: Vec<Effect>,
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

    pub queue: Queue,
    pub wave: Option<Wave>,
    /// The wave the queue's carried-over first track was played from: its end is reported there.
    carried_wave: Option<Wave>,
    pub queue_state: TableState,
    generation: u64,
    /// Bumped whenever the user starts something to play: replies to an older request are dropped.
    intent: u64,
    errors_in_row: u32,
    /// The server refused the token: tracks are not tried one after another any more.
    unauthorized: bool,
    pub loading_track: bool,
    /// The player has not started the current track yet: the positions it reports until then are
    /// of the previous one, still playing while the new link is resolved.
    awaiting_start: bool,

    pub likes: Likes,
    pub liked_state: TableState,

    pub stations: Vec<StationRow>,
    pub stations_state: ListState,

    pub search: Search,
    pub search_state: TableState,

    pub playlists: Playlists,
    pub playlists_state: TableState,
    /// Selection in the open playlist.
    pub open_state: TableState,
    pub modal: Option<Modal>,
    /// Current equalizer preset (index into `eq::PRESETS`).
    pub eq: usize,
    /// Sound quality (index into `api::QUALITIES`).
    pub quality: usize,
    /// Crossfade length, seconds (0 — off).
    pub crossfade: u8,
    /// A new list waits for the current track to end instead of cutting it off.
    pub finish_track: bool,
    /// Nerd Font icons (false — plain Unicode symbols).
    pub icons: bool,
    /// What «Сейчас играет» shows above the track; covers are downloaded only for `Art::Cover`.
    pub art: Art,
    pub output: Output,
    /// The header shows no login and no Plus badge.
    pub hide_account: bool,
    /// The progress bar shows the time left instead of the track's length.
    pub remaining_time: bool,
    pub lastfm: Lastfm,
    /// Scrobbles that did not go through (no network): sent with the next one.
    lastfm_backlog: Vec<Scrobble>,
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
    /// Failed tries of the current track (errors, stalls); each next one gets a fresh link.
    failures: u32,
    /// The current track is to be tried again then, from this position (seconds).
    retry_at: Option<(Instant, f64)>,
    /// `Load`, `Next` and `Stop` sent that the player has not confirmed yet (`PlayerEvent::Cleared`):
    /// until it has, what it reports is about the track played before.
    player_behind: u32,
    /// Where the restarted track is to go on from, seconds.
    resume_at: Option<f64>,
    /// Playback was stopped (the queue ran out, the wave is loading, too many errors).
    stopped: bool,
    /// Where the queue (a list, not the wave) comes from, for the listening history.
    context: PlayContext,
    play: Option<Play>,
    /// Tests: what the effects taken so far asked for.
    #[cfg(test)]
    player_log: Vec<String>,
    #[cfg(test)]
    tasks_log: Vec<Task>,
}

impl App {
    pub fn new(api: Api, account: Account, volume: u8) -> App {
        let mut app = App {
            api,
            effects: Vec::new(),
            account,
            tab: Tab::Wave,
            quit: false,
            exit_error: None,
            show_help: false,
            show_about: false,
            about_from_settings: false,
            logout: false,
            queue: Queue::default(),
            wave: None,
            carried_wave: None,
            queue_state: TableState::default(),
            generation: 0,
            intent: 0,
            errors_in_row: 0,
            unauthorized: false,
            loading_track: false,
            awaiting_start: false,
            likes: Likes::default(),
            liked_state: TableState::default(),
            stations: Vec::new(),
            stations_state: ListState::default(),
            search: Search::default(),
            search_state: TableState::default(),
            playlists: Playlists::default(),
            playlists_state: TableState::default(),
            open_state: TableState::default(),
            modal: None,
            eq: 0,
            quality: 1,
            crossfade: 0,
            finish_track: false,
            icons: true,
            art: Art::Cover,
            output: Output::Auto,
            hide_account: false,
            remaining_time: false,
            lastfm: Lastfm::Off,
            lastfm_backlog: Vec::new(),
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
            failures: 0,
            retry_at: None,
            player_behind: 0,
            resume_at: None,
            stopped: false,
            context: PlayContext::default(),
            play: None,
            #[cfg(test)]
            player_log: Vec::new(),
            #[cfg(test)]
            tasks_log: Vec::new(),
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

    /// An app for tests: nothing carries its effects out — no sound, no requests.
    #[cfg(test)]
    pub fn for_test() -> App {
        let acc = Account { uid: "1".into(), login: "tester".into(), name: "Tester".into(), has_plus: true };
        let mut api = Api::new("x");
        api.uid = "1".into();
        App::new(api, acc, 50)
    }

    /// Tests: moves the effects asked for so far to the logs.
    #[cfg(test)]
    fn log_effects(&mut self) {
        for effect in std::mem::take(&mut self.effects) {
            match effect {
                Effect::Player(cmd) => self.player_log.push(cmd.to_string()),
                Effect::Task(task) => self.tasks_log.push(*task),
            }
        }
    }

    /// Tests: the player commands since the last call, as text (`load <url>`, `pause`, …).
    #[cfg(test)]
    pub fn take_player(&mut self) -> Vec<String> {
        self.log_effects();
        // The player has carried them out.
        self.player_behind = 0;
        std::mem::take(&mut self.player_log)
    }

    /// Tests: the tasks since the last call.
    #[cfg(test)]
    pub fn take_tasks(&mut self) -> Vec<Task> {
        self.log_effects();
        std::mem::take(&mut self.tasks_log)
    }

    /// Tests: the listening history records sent so far (they stay in the log).
    #[cfg(test)]
    pub fn plays_sent(&mut self) -> Vec<PlayReport> {
        self.log_effects();
        self.tasks_log.iter().filter_map(|t| if let Task::SendPlay(r) = t { Some(r.clone()) } else { None }).collect()
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
        (self.queue.pos().is_some() && !self.paused)
            || self.loading_track
            || self.search.editing
            || matches!(self.modal, Some(Modal::Rename { .. } | Modal::NewPlaylist { .. } | Modal::OpenLink { .. }))
            || self.search.loading()
            || self.likes.loading()
            || self.wave.as_ref().is_some_and(|w| w.fetching)
            || self.spectrum.iter().any(|&v| v > 0.0)
    }

    /// The spectrum is shown and moves: the screen is redrawn more often.
    pub fn spectrum_moving(&self) -> bool {
        self.art == Art::Spectrum && self.spectrum.iter().any(|&v| v > 0.0)
    }

    /// A track is heard right now.
    fn playing(&self) -> bool {
        self.queue.pos().is_some() && !self.paused && !self.loading_track && !self.stopped
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
            self.play(PlayerCmd::RequestSpectrum);
        } else {
            for bar in &mut self.spectrum {
                *bar = (*bar - SPECTRUM_FALL).max(0.0);
            }
        }
    }

    pub fn visible_status(&self) -> Option<&str> {
        (!self.status.is_empty() && self.status_at.elapsed().as_secs() < 6).then_some(self.status.as_str())
    }

    fn play(&mut self, cmd: PlayerCmd) {
        if matches!(cmd, PlayerCmd::Load(_) | PlayerCmd::Next | PlayerCmd::Stop) {
            self.player_behind += 1;
        }
        self.effects.push(Effect::Player(cmd));
    }

    fn task(&mut self, task: Task) {
        self.effects.push(Effect::Task(Box::new(task)));
    }

    /// What the logic has asked for since the last call, for `Runtime` to carry out.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Bumped with every new current track: links asked for older ones are not needed any more.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The API as the tasks use it: with the account's «Меньше ИИ-музыки».
    pub fn task_api(&self) -> Api {
        let mut api = self.api.clone();
        api.less_ai = self.less_ai == Some(true);
        api
    }

    pub fn current(&self) -> Option<&Track> {
        self.queue.current()
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
        (State { track, status, volume, repeat: self.queue.repeat(), shuffle: self.queue.shuffled() }, self.time_pos)
    }

    /// A command from the desktop: the same as the matching key.
    fn on_remote(&mut self, r: crate::media::Remote) {
        use crate::media::Remote;
        let has_track = self.queue.pos().is_some() && !self.stopped;
        match r {
            Remote::PlayPause if has_track => self.play(PlayerCmd::TogglePause),
            Remote::Play if has_track && self.paused => self.play(PlayerCmd::TogglePause),
            Remote::Pause if has_track && !self.paused => self.play(PlayerCmd::TogglePause),
            Remote::Next => self.next(End::Skip),
            Remote::Previous => self.prev(),
            Remote::Seek(d) if has_track => self.play(PlayerCmd::Seek(d)),
            Remote::SeekTo(t) if has_track => self.play(PlayerCmd::Seek(t - self.time_pos)),
            Remote::Volume(v) => self.change_volume(v - self.volume),
            Remote::Repeat(r) => self.set_repeat(r),
            Remote::Shuffle(on) => self.set_shuffled(on),
            Remote::Quit => self.quit = true,
            _ => {}
        }
    }

    pub fn is_liked(&self, t: &Track) -> bool {
        self.likes.contains(&t.id)
    }

    pub fn source_name(&self) -> String {
        match (self.queue.source(), &self.wave) {
            (Source::Wave, Some(w)) => w.name.clone(),
            (Source::List(n), _) if self.queue.shuffled() => format!("{n} (в случайном порядке)"),
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
        if self.likes.begin_load() {
            self.task(Task::Liked);
        }
    }

    fn load_playlists(&mut self) {
        let req = self.playlists.begin_load();
        self.task(Task::Playlists { req });
    }

    fn load_account_settings(&mut self) {
        self.task(Task::LessAi);
    }

    fn load_stations(&mut self) {
        self.task(Task::Stations);
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
        let wave = match () {
            _ if self.queue.on_carried() => self.carried_wave.as_ref(),
            _ if self.queue.is_wave() => self.wave.as_ref(),
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
        let task = feedback(w, ev, t.batch.clone());
        self.task(task);
    }

    // ------------------------------------------------------------ listening history

    /// A new playback of `track` begins: reported with nothing played yet.
    fn start_play(&mut self, track: &Track) {
        let (context, radio) = match (self.queue.source(), &self.wave) {
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
        let started_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.play = Some(Play { report, last_pos: 0.0, partial_sent: false, started_unix, now_playing_sent: false, scrobbled: false });
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
        self.lastfm_play();
    }

    /// Last.fm learns the track plays once it does, and gets a scrobble once it is listened long
    /// enough (see `lastfm::scrobble_after`).
    fn lastfm_play(&mut self) {
        let Lastfm::On(s) = &self.lastfm else { return };
        let session = s.key.clone();
        let duration = self.duration;
        let Some(play) = &mut self.play else { return };
        let track = &play.report.track;
        let artist = track.main_artist.as_ref().map_or(track.artists.clone(), |(_, name)| name.clone());
        let item = Scrobble {
            artist,
            track: track.title.clone(),
            album: track.album.clone(),
            duration: duration.round() as u64,
            timestamp: play.started_unix,
        };
        if !std::mem::replace(&mut play.now_playing_sent, true) {
            self.task(Task::LastfmNowPlaying { session: session.clone(), track: item.clone() });
        }
        let Some(play) = &mut self.play else { return };
        if !play.scrobbled && lastfm::scrobble_after(duration).is_some_and(|after| play.report.played >= after) {
            play.scrobbled = true;
            let mut tracks = std::mem::take(&mut self.lastfm_backlog);
            tracks.push(item);
            self.task(Task::LastfmScrobble { session, tracks });
        }
    }

    /// Starts connecting Last.fm: a token is asked for, then confirmed by the user in the browser.
    fn connect_lastfm(&mut self) {
        let now = Instant::now();
        self.lastfm = Lastfm::Connecting { token: None, until: now + LASTFM_CONFIRM, poll_at: now, polling: false };
        self.set_status("Last.fm: подключение…");
        self.task(Task::LastfmToken);
    }

    /// While connecting: asks now and then whether the access has been confirmed.
    fn poll_lastfm(&mut self) {
        let now = Instant::now();
        let Lastfm::Connecting { token: Some(token), until, poll_at, polling } = &mut self.lastfm else { return };
        if *polling || now < *poll_at {
            return;
        }
        if now >= *until {
            self.lastfm = Lastfm::Off;
            self.set_error("Last.fm: доступ не подтверждён — подключите снова в настройках");
            return;
        }
        *polling = true;
        let token = token.clone();
        self.task(Task::LastfmSession(token));
    }

    fn on_lastfm_session(&mut self, token: String, result: Result<(String, String), lastfm::Error>) {
        let Lastfm::Connecting { token: Some(t), polling, poll_at, .. } = &mut self.lastfm else { return };
        if *t != token {
            return;
        }
        match result {
            Ok((name, key)) => {
                self.set_status(format!("Last.fm подключён: {name}"));
                self.lastfm = Lastfm::On(LastfmSession { name, key });
            }
            // Not confirmed yet.
            Err(e) if e.not_authorized() || e.is_temporary() => {
                *polling = false;
                *poll_at = Instant::now() + LASTFM_POLL;
            }
            Err(e) => {
                self.lastfm = Lastfm::Off;
                self.set_error(format!("Не удалось подключить Last.fm: {e}"));
            }
        }
    }

    fn on_lastfm_sent(&mut self, session: String, tracks: Vec<Scrobble>, result: Result<(), lastfm::Error>) {
        if !matches!(&self.lastfm, Lastfm::On(s) if s.key == session) {
            return;
        }
        match result {
            Ok(()) => {}
            Err(e) if e.is_session() => {
                self.lastfm = Lastfm::Off;
                self.lastfm_backlog.clear();
                self.set_error("Last.fm: доступ отозван — подключите снова в настройках");
            }
            // Sent again with the next scrobble; the oldest go first if too many wait.
            Err(e) if e.is_temporary() => {
                if !tracks.is_empty() {
                    if e.blocked {
                        self.set_error(format!("Скробблы ждут отправки: {e}"));
                    }
                    let mut all = tracks;
                    all.append(&mut self.lastfm_backlog);
                    let extra = all.len().saturating_sub(lastfm::MAX_BATCH);
                    all.drain(..extra);
                    self.lastfm_backlog = all;
                }
            }
            Err(e) if tracks.is_empty() => drop(e),
            Err(e) => self.set_error(format!("Не удалось отправить скроббл: {e}")),
        }
    }

    pub fn lastfm_session(&self) -> Option<&LastfmSession> {
        if let Lastfm::On(s) = &self.lastfm { Some(s) } else { None }
    }

    /// The terminal window's title: what plays now.
    pub fn window_title(&self) -> String {
        match self.current() {
            Some(t) if !t.artists.is_empty() => format!("{} — {}", t.artists, t.title),
            Some(t) => t.title.clone(),
            None => "ymusic".to_string(),
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
        self.task(Task::SendPlay(report));
    }

    fn wave_feedback(&mut self, ev: Value, batch: Option<String>) {
        if let Some(w) = &self.wave {
            let task = feedback(w, ev, batch);
            self.task(task);
        }
    }

    /// Starts track `idx`: resolves its URL and replaces what the player plays.
    fn play_index(&mut self, idx: usize) {
        if !self.begin_track(idx, false) {
            return;
        }
        // The previous track stops at once rather than plays on under the new title until the
        // link comes (seconds on a slow connection).
        self.play(PlayerCmd::Stop);
        let generation = self.generation;
        let id = self.queue.tracks()[idx].full_id();
        let q = QUALITIES[self.quality].id;
        self.task(Task::TrackUrl { generation, full_id: id, quality: q });
    }

    /// Updates state for a new current track. `preloaded`: the player already switched to it.
    fn begin_track(&mut self, idx: usize, preloaded: bool) -> bool {
        let Some(track) = self.queue.tracks().get(idx).cloned() else { return false };
        self.queue.select(idx);
        self.queue_state.select(Some(idx));
        self.generation += 1;
        self.time_pos = 0.0;
        self.duration = track.duration_ms as f64 / 1000.0;
        self.loading_track = true;
        self.awaiting_start = true;
        self.preload = None;
        self.preload_pending = false;
        self.load_deadline = preloaded.then(|| Instant::now() + LOAD_TIMEOUT);
        self.failures = 0;
        self.retry_at = None;
        self.resume_at = None;
        self.stopped = false;
        self.load_cover(track.cover.clone());
        self.maybe_extend_search_queue();
        // A restart of the same track (after an error) goes on with its playback.
        if self.play.as_ref().is_none_or(|p| p.report.track.id != track.id) {
            self.end_play(Some("skip"));
            self.start_play(&track);
        }
        // A track carried over from another list is not part of this wave.
        if self.queue.is_wave() && !self.queue.is_carried(idx) {
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

    /// The track to play after the current one (see `Queue::following`).
    fn following(&self, end: End) -> Option<usize> {
        // A search queue goes on with its next pages first.
        let more_coming = self.search.feeds_queue && self.search.can_load_more();
        self.queue.following(matches!(end, End::Eof), more_coming)
    }

    /// What follows the current track changed: the one already appended to the player is dropped.
    fn following_changed(&mut self) {
        self.preload = None;
        self.play(PlayerCmd::ClearNext);
        self.schedule_preload();
    }

    /// Resolves the next track's URL in advance and appends it to the player.
    fn schedule_preload(&mut self) {
        if self.preload.is_some() || self.preload_pending || self.loading_track || self.queue.pos().is_none() {
            return;
        }
        let Some(idx) = self.following(End::Eof) else { return };
        let track = self.queue.tracks()[idx].clone();
        self.preload_pending = true;
        self.load_cover(track.cover.clone());
        let generation = self.generation;
        let q = QUALITIES[self.quality].id;
        self.task(Task::Preload { generation, idx, track, quality: q });
    }

    /// Switches to the preloaded track if it is the next one. Returns false if there is none.
    fn advance_preloaded(&mut self, end: End, player_advanced: bool) -> bool {
        let next = self.following(end);
        match &self.preload {
            Some((idx, id)) if Some(*idx) == next && self.queue.tracks()[*idx].id == *id => {
                let idx = *idx;
                self.report_end(end);
                if !player_advanced {
                    self.play(PlayerCmd::Next);
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
        } else if self.queue.is_wave() {
            if let Some(w) = &mut self.wave {
                w.pending_next = true;
            }
            self.fetch_more_wave();
            self.load_deadline = None;
            self.play(PlayerCmd::Stop);
            self.stopped = true;
            self.set_status("Загрузка следующих треков волны…");
        } else {
            self.load_deadline = None;
            self.play(PlayerCmd::Stop);
            self.stopped = true;
            self.queue.stop();
            self.set_status("Очередь закончилась");
        }
    }

    fn prev(&mut self) {
        if self.time_pos > 5.0 || self.queue.pos() == Some(0) {
            self.play(PlayerCmd::Seek(-self.time_pos));
            return;
        }
        if let Some(i) = self.queue.previous() {
            self.report_end(End::Back);
            self.play_index(i);
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
        self.queue.replace(Source::List(name.to_string()), tracks);
        self.search.feeds_queue = false;
        self.preload = None;
        self.playlists.playing = None;
        self.wave = None;
        self.play_index(start);
        self.set_status(format!("Играет: {name}"));
    }

    /// «Доигрывать трек»: a new list waits for the playing track to end (a paused one is
    /// switched at once: it is not being listened to).
    fn finishing_track(&self) -> bool {
        self.finish_track && self.queue.pos().is_some() && !self.paused && !self.loading_track && !self.stopped
    }

    /// Replaces the queue with `tracks` but keeps the playing track as its first entry, so the
    /// list goes on from there when the track ends (`n` switches at once).
    fn queue_after_current(&mut self, tracks: Vec<Track>, source: Source, wave: Option<Wave>) {
        let from_wave = self.queue.is_wave();
        let Some(after) = self.queue.after_current(source, tracks) else { return };
        // Switched again before the track ended: it still belongs to its first list.
        if !after.was_carried {
            self.carried_wave = if from_wave { self.wave.take() } else { None };
        }
        self.queue_state.select(Some(0));
        self.wave = wave;
        self.search.feeds_queue = false;
        self.playlists.playing = None;
        // The next track of the old list may already be in the player: replies for it are dropped too.
        self.generation += 1;
        self.preload = None;
        self.preload_pending = false;
        self.play(PlayerCmd::ClearNext);
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
        self.task(Task::WaveStart { intent, station: id, name, settings });
    }

    fn load_cover(&mut self, url: Option<String>) {
        let Some(url) = url else { return };
        if self.art != Art::Cover
            || self.picker.is_none() || self.covers.iter().any(|(u, _)| *u == url) || !self.covers_loading.insert(url.clone())
        {
            return;
        }
        self.task(Task::Cover(url));
    }

    /// Cover of the current track, if it is loaded.
    pub fn current_cover(&mut self) -> Option<&mut StatefulProtocol> {
        let want = self.current().and_then(|t| t.cover.clone());
        let Some(want) = want.filter(|w| self.covers.iter().any(|(u, _)| u == w)) else {
            // No cover (or not loaded yet): a kitty or sixel image of the previous one would stay
            // on the terminal over the placeholder until erased.
            if self.shown_cover.take().is_some() {
                self.clear_images = true;
            }
            return None;
        };
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
        let recent: Vec<String> = self.queue.tracks().iter().rev().take(10).rev().map(Track::full_id).collect();
        self.task(Task::WaveMore { station: st, session: sess, recent });
    }

    // ------------------------------------------------------------ likes

    fn toggle_like(&mut self, track: Track) {
        let like = !self.is_liked(&track);
        self.apply_like(&track, like);
        if self.queue.is_wave() && self.queue.contains(&track.id) {
            let kind = if like { "like" } else { "unlike" };
            self.wave_feedback(json!({"type": kind, "timestamp": iso_now(), "trackId": track.full_id()}), track.batch.clone());
        }
        self.set_status(if like {
            format!("♥ Добавлено в «Мне нравится»: {}", track.title)
        } else {
            format!("Убрано из «Мне нравится»: {}", track.title)
        });
        self.task(Task::SetLike { track, like });
    }

    fn apply_like(&mut self, track: &Track, like: bool) {
        self.likes.set(track, like);
        let n = self.likes.tracks().len();
        if let Some(s) = self.liked_state.selected() {
            self.liked_state.select(if n == 0 { None } else { Some(s.min(n - 1)) });
        }
    }

    fn dislike_current(&mut self) {
        let Some(track) = self.current().cloned() else { return };
        if self.is_liked(&track) {
            self.apply_like(&track, false);
        }
        if self.queue.is_wave() {
            self.wave_feedback(json!({"type": "dislike", "timestamp": iso_now(), "trackId": track.full_id()}), track.batch.clone());
        }
        self.set_status(format!("✗ Не нравится: {} — больше не попадётся", track.title));
        let t = track.clone();
        self.task(Task::Dislike(t));
        self.next(End::Dislike);
    }

    // ------------------------------------------------------------ messages

    pub fn on_msg(&mut self, msg: Msg) {
        if msg.error().is_some_and(ApiError::is_unauthorized) {
            self.unauthorized = true;
        }
        match msg {
            Msg::Remote(r) => self.on_remote(r),
            Msg::Player(ev) => match ev {
                PlayerEvent::Cleared => self.player_behind = self.player_behind.saturating_sub(1),
                // About the track played before: its end or failure must not move the queue on.
                PlayerEvent::TimePos(_)
                | PlayerEvent::Duration(_)
                | PlayerEvent::Eof
                | PlayerEvent::Error { .. }
                | PlayerEvent::Spectrum(_)
                    if self.player_behind > 0 => {}
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
                PlayerEvent::Error { message, network } => {
                    self.set_error(format!("Ошибка воспроизведения: {message}"));
                    self.on_track_error(network);
                }
                PlayerEvent::OutputFailed { kept, error } => {
                    self.output = Output::from_id(kept.as_deref());
                    self.set_error(format!("Не удалось переключить вывод звука: {error}"));
                }
                PlayerEvent::Died(msg) => {
                    self.exit_error = Some(msg);
                    self.quit = true;
                }
            },
            Msg::Url { generation, result } => {
                if generation != self.generation {
                    return;
                }
                match result {
                    Ok(url) => {
                        let length = self.current().and_then(length);
                        let start = self.resume_at.take().unwrap_or(0.0);
                        self.play(PlayerCmd::Load(TrackFile { url, length, start }));
                        self.load_deadline = Some(Instant::now() + LOAD_TIMEOUT);
                    }
                    Err(e) => {
                        self.set_error(format!("Не удалось получить трек: {e}"));
                        self.on_track_error(e.is_network());
                    }
                }
            }
            Msg::LikedIds(ids) => self.likes.ids_loaded(&ids),
            Msg::Liked(r) => match self.likes.loaded(r) {
                Ok(()) => {
                    if !self.likes.tracks().is_empty() && self.liked_state.selected().is_none() {
                        self.liked_state.select(Some(0));
                    }
                }
                Err(e) => self.set_error(format!("Не удалось загрузить «Мне нравится»: {e}")),
            },
            Msg::Stations(list) => {
                let mut all = default_stations();
                all.retain(|s| s.id == MY_WAVE);
                let mut rest = list;
                let order = ["Жанры", "Настроение", "Занятия", "Эпохи"];
                rest.sort_by_key(|s| order.iter().position(|c| *c == s.category).unwrap_or(order.len()));
                all.extend(rest.into_iter().filter(|s| s.id != MY_WAVE));
                self.set_stations(all);
            }
            Msg::Search { query, page, result } => match self.search.page_loaded(&query, page, result) {
                Page::Stale => {}
                Page::Failed(e) => self.set_error(format!("Ошибка поиска: {e}")),
                Page::Loaded { first, new } => {
                    if first {
                        if new.is_empty() {
                            self.set_status("Ничего не найдено");
                        }
                        self.search_state.select((!new.is_empty()).then_some(0));
                    }
                    if self.search.feeds_queue {
                        self.queue.append(new);
                    }
                }
            },
            Msg::Playlists { req, result } => match self.playlists.loaded(req, result) {
                None => {}
                Some(Ok(())) => {
                    let n = self.playlists.list().len();
                    let sel = self.playlists_state.selected().unwrap_or(0);
                    self.playlists_state.select((n > 0).then(|| sel.min(n - 1)));
                }
                Some(Err(e)) => self.set_error(format!("Не удалось загрузить плейлисты: {e}")),
            },
            Msg::PlaylistOpened { key, req, result } => match self.playlists.open_loaded(&key, req, result) {
                None => {}
                Some(Ok(n)) => {
                    let sel = self.open_state.selected().unwrap_or(0);
                    self.open_state.select((n > 0).then(|| sel.min(n - 1)));
                }
                Some(Err(e)) => self.set_error(format!("Не удалось открыть плейлист: {e}")),
            },
            Msg::PlaylistEdited { key, result } => match result {
                Ok(status) => {
                    self.set_status(status);
                    self.load_playlists();
                    if self.playlists.open.as_ref().is_some_and(|o| o.pl.key() == key) {
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
                    self.playlists.playing = playlist;
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
                        self.queue.replace(Source::Wave, b.tracks);
                        self.search.feeds_queue = false;
                        self.preload = None;
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
                        self.queue.append(b.tracks);
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
            // The main loop takes these itself.
            Msg::Input(_) | Msg::Resized | Msg::Terminate => {}
            Msg::ClipboardLink(link) => {
                if let Some(Modal::OpenLink { input }) = &mut self.modal
                    && input.is_empty()
                {
                    *input = link;
                }
            }
            Msg::Opened { url, ok } => {
                if !ok {
                    crate::clipboard::osc52(&url);
                    self.set_status(format!("Браузер не открылся — откройте ссылку сами (скопирована): {url}"));
                    self.task(Task::CopyWithTool(url));
                }
            }
            Msg::LastfmToken(result) => {
                let Lastfm::Connecting { token: token @ None, poll_at, .. } = &mut self.lastfm else { return };
                match result {
                    Ok(t) => {
                        *poll_at = Instant::now() + LASTFM_POLL;
                        let url = lastfm::auth_url(&t);
                        *token = Some(t);
                        self.set_status("Last.fm: разрешите доступ в открывшемся браузере");
                        self.task(Task::OpenUrl(url));
                    }
                    Err(e) => {
                        self.lastfm = Lastfm::Off;
                        self.set_error(format!("Не удалось подключить Last.fm: {e}"));
                    }
                }
            }
            Msg::LastfmSession { token, result } => self.on_lastfm_session(token, result),
            Msg::LastfmSent { session, tracks, result } => self.on_lastfm_sent(session, tracks, result),
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
            Msg::Preload { generation, idx, id, result } => {
                if generation != self.generation {
                    return;
                }
                self.preload_pending = false;
                if let Ok(url) = result {
                    if self.following(End::Eof) == Some(idx) && self.queue.tracks()[idx].id == id {
                        let length = length(&self.queue.tracks()[idx]);
                        self.play(PlayerCmd::Append(TrackFile { url, length, start: 0.0 }));
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
    /// fresh link): a server that sends the headers and then stalls makes the player reconnect
    /// forever without an error, and switching away and back used to be the only cure. Also
    /// tries a failed track again when its time comes.
    pub fn tick(&mut self) {
        self.tick_spectrum();
        self.poll_lastfm();
        if let Some((at, from)) = self.retry_at
            && Instant::now() >= at
            && let Some(p) = self.queue.pos()
        {
            self.restart(p, from);
            return;
        }
        let Some(deadline) = self.load_deadline else { return };
        if self.paused {
            // A paused track is not expected to start: count from the moment it is resumed.
            self.load_deadline = Some(Instant::now() + LOAD_TIMEOUT);
            return;
        }
        if Instant::now() < deadline {
            return;
        }
        self.set_error("Трек не загружается: сервер не отвечает");
        self.on_track_error(true);
    }

    /// The current track failed. `network`: it did not come (no connection, the server does not
    /// answer): it is tried `NETWORK_TRIES` times, a while apart, before the next one is played;
    /// a track failing otherwise is tried once more (its link could have expired).
    fn on_track_error(&mut self, network: bool) {
        self.load_deadline = None;
        // Without a valid token no track plays: no use trying the next ones.
        if self.unauthorized {
            self.loading_track = false;
            self.play(PlayerCmd::Stop);
            self.stopped = true;
            return;
        }
        self.failures += 1;
        let tries = if network { NETWORK_TRIES } else { 2 };
        if self.queue.pos().is_some() && self.failures < tries {
            // A download broken off midway goes on from there.
            let from = self.time_pos;
            if network {
                // Nothing is heard meanwhile: the stalled download is dropped.
                self.play(PlayerCmd::Stop);
                self.loading_track = true;
                self.retry_at = Some((Instant::now() + RETRY_DELAY, from));
                self.set_error(format!("Нет связи с сервером: повтор {} из {}…", self.failures, tries - 1));
            } else if let Some(p) = self.queue.pos() {
                self.restart(p, from);
            }
            return;
        }
        if network {
            self.set_error(format!("Трек не загрузился за {tries} попытки: нет связи с сервером"));
        }
        self.loading_track = false;
        self.errors_in_row += 1;
        if self.errors_in_row <= 5 {
            self.next(End::Silent);
        } else {
            self.play(PlayerCmd::Stop);
            self.stopped = true;
        }
    }

    /// Plays track `p` again with a fresh link, from `from` seconds; the failures so far count.
    fn restart(&mut self, p: usize, from: f64) {
        let failures = self.failures;
        self.play_index(p);
        self.failures = failures;
        self.resume_at = (from > 1.0).then_some(from);
        // Shown until it goes on, and kept for one more try if this one fails too.
        self.time_pos = from;
    }

    // ------------------------------------------------------------ keys

    /// Text pasted into the terminal: it goes into the open text field as is, and a Yandex Music
    /// link pasted on the main screen opens «Играть по ссылке» instead of acting as hotkeys.
    pub fn on_paste(&mut self, text: &str) {
        let text: String = text.trim().chars().filter(|c| !c.is_control()).collect();
        match &mut self.modal {
            Some(Modal::Rename { input, .. } | Modal::NewPlaylist { input, .. } | Modal::OpenLink { input }) => {
                input.push_str(&text)
            }
            Some(_) => {}
            None if self.search.editing => self.search.input.push_str(&text),
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
        if self.search.editing {
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
        if let Some(action) = keymap::action(key.hotkey.code) {
            self.act(action);
        }
    }

    /// A hotkey of the main screen (see `keymap`); what some of them do depends on the tab.
    pub fn act(&mut self, action: Action) {
        let playlists = self.tab == Tab::Playlists;
        let open = playlists && self.playlists.open.is_some();
        match action {
            Action::Quit => self.quit = true,
            Action::Help => self.show_help = true,
            Action::Settings => self.modal = Some(Modal::Settings { sel: 0 }),
            Action::PlayPause => self.play(PlayerCmd::TogglePause),
            Action::Next => self.next(End::Skip),
            Action::Prev => self.prev(),
            Action::SeekBack => self.play(PlayerCmd::Seek(-10.0)),
            Action::SeekForward => self.play(PlayerCmd::Seek(10.0)),
            Action::VolumeUp => self.change_volume(5.0),
            Action::VolumeDown => self.change_volume(-5.0),
            Action::Mute => self.play(PlayerCmd::ToggleMute),
            Action::Repeat => self.set_repeat(self.queue.repeat().next()),
            Action::LikeCurrent => {
                if let Some(t) = self.current().cloned() {
                    self.toggle_like(t);
                }
            }
            Action::LikeSelected => {
                if let Some(t) = self.selected_track().cloned() {
                    self.toggle_like(t);
                }
            }
            Action::Dislike => self.dislike_current(),
            Action::MyWave => self.start_wave(my_wave()),
            Action::TrackWave => {
                if let Some(t) = self.selected_track().or(self.current()).cloned() {
                    self.start_wave(Station {
                        id: format!("track:{}", t.id),
                        name: format!("Волна по треку «{}»", t.title),
                        category: String::new(),
                        settings: None,
                    });
                }
            }
            Action::Artist => self.play_artist(),
            Action::Album => self.play_album(),
            Action::Tab(i) => self.tab = Tab::ALL[i],
            Action::NextTab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            Action::PrevTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            Action::Down => self.move_sel(1),
            Action::Up => self.move_sel(-1),
            Action::PageDown => self.move_sel(10),
            Action::PageUp => self.move_sel(-10),
            Action::Top => self.move_sel(i32::MIN / 2),
            Action::Bottom => self.move_sel(i32::MAX / 2),
            Action::Activate => self.activate(),
            Action::Shuffle if open => self.play_open(true),
            Action::Shuffle if playlists => self.play_playlist(true),
            Action::Shuffle => self.shuffle_play(),
            Action::CopyLink => self.copy_link(),
            Action::OpenLink => self.ask_link(),
            Action::Search => {
                self.tab = Tab::Search;
                self.search.editing = true;
            }
            Action::EditSearch if self.tab == Tab::Search => self.search.editing = true,
            Action::Reload => {
                self.reload_all();
                self.set_status("Обновление…");
            }
            Action::AddToPlaylist => self.ask_add_to_playlist(),
            Action::Remove if open => self.ask_remove_from_playlist(),
            Action::Remove if playlists => self.ask_delete_playlist(),
            Action::Rename if playlists => self.ask_rename_playlist(),
            Action::NewPlaylist => self.modal = Some(Modal::NewPlaylist { input: String::new(), track: None }),
            Action::Back if playlists => self.playlists.open = None,
            Action::EditSearch | Action::Remove | Action::Rename | Action::Back => {}
        }
    }

    /// Requests the next page of the current search.
    fn load_search_page(&mut self) {
        let (query, page) = self.search.next_page();
        self.task(Task::Search { query, page });
    }

    /// Loads the next search page when the queue played from the search is close to its end.
    fn maybe_extend_search_queue(&mut self) {
        let near_end = self.queue.pos().is_some_and(|p| p + 20 >= self.queue.len());
        if self.search.feeds_queue && near_end && self.search.can_load_more() {
            self.load_search_page();
        }
    }

    /// Plays the loaded search results; later pages are appended to the queue.
    fn play_search(&mut self, name: &str, tracks: Vec<Track>, start: usize, shuffled: bool) {
        let context = PlayContext::new("search", self.search.query());
        if shuffled {
            self.play_shuffled(name, context, tracks);
        } else {
            self.play_list(name, context, tracks, start);
        }
        self.search.feeds_queue = true;
        self.maybe_extend_search_queue();
    }

    /// Loads more search results when the selection gets close to the end of the list.
    fn maybe_load_more_search(&mut self) {
        let sel = self.search_state.selected().unwrap_or(0);
        if sel + 20 >= self.search.results().len() && self.search.can_load_more() {
            self.load_search_page();
        }
    }

    fn on_search_key(&mut self, press: Press) {
        let key = press.typed;
        match key.code {
            KeyCode::Esc => self.search.editing = false,
            KeyCode::Enter => {
                if self.search.submit() {
                    self.load_search_page();
                }
            }
            KeyCode::Backspace => {
                self.search.input.pop();
            }
            _ if is_ctrl(&press.hotkey, 'u') => self.search.input.clear(),
            KeyCode::Char(c) if is_typed(&key) => self.search.input.push(c),
            _ => {}
        }
    }

    pub fn set_eq(&mut self, idx: usize) {
        let idx = idx.min(crate::eq::PRESETS.len() - 1);
        self.eq = idx;
        self.play(PlayerCmd::Eq(&crate::eq::PRESETS[idx]));
    }

    /// Crossfade length in seconds (0 — off).
    pub fn set_crossfade(&mut self, secs: u8) {
        self.crossfade = secs.min(crate::config::MAX_CROSSFADE);
        self.play(PlayerCmd::Fade(f64::from(self.crossfade)));
    }

    pub fn set_icons(&mut self, nerd: bool) {
        self.icons = nerd;
    }

    pub fn set_art(&mut self, art: Art) {
        let was = self.art;
        self.art = art;
        // The analysis filter costs a little CPU: it runs only while the spectrum is shown.
        self.play(PlayerCmd::Spectrum(art == Art::Spectrum));
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
        crate::queue::shuffle(&mut shuffled);
        let finishing = self.finishing_track();
        self.play_list(name, context, shuffled, 0);
        if matches!(self.queue.source(), Source::List(n) if n == name) {
            self.queue.keep_own_order(tracks);
            let when = if finishing { "После этого трека" } else { "Играет" };
            self.set_status(format!("{when}: {}", self.source_name()));
        }
    }

    /// Shuffle turned on or off from the system (MPRIS, Now Playing): see `Queue::set_shuffled`.
    fn set_shuffled(&mut self, on: bool) {
        if !self.queue.set_shuffled(on) {
            return;
        }
        self.queue_state.select(self.queue.pos());
        self.following_changed();
        self.set_status(format!("В случайном порядке: {}", if on { "вкл" } else { "выкл" }));
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.queue.set_repeat(repeat);
        self.following_changed();
        let wave = if repeat == Repeat::All && self.queue.is_wave() { " · волна и так бесконечна" } else { "" };
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
            Setting::Repeat => self.queue.repeat().name().to_string(),
            Setting::Api => if self.api.web { "сайт" } else { "мобильный" }.to_string(),
            Setting::Art => self.art.name().to_string(),
            Setting::Icons => if self.icons { "вкл" } else { "выкл" }.to_string(),
            Setting::Output => self.output.name().to_string(),
            Setting::HideAccount => if self.hide_account { "вкл" } else { "выкл" }.to_string(),
            Setting::RemainingTime => if self.remaining_time { "вкл" } else { "выкл" }.to_string(),
            Setting::Lastfm if !lastfm::available() => "нет ключа".to_string(),
            Setting::Lastfm => match &self.lastfm {
                Lastfm::Off => "выкл".to_string(),
                Lastfm::Connecting { .. } => "подключение…".to_string(),
                Lastfm::On(s) => s.name.clone(),
            },
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
                    self.task(Task::SetLessAi(on));
                }
                None => self.set_status("Настройки аккаунта ещё загружаются"),
            },
            Setting::FinishTrack => self.finish_track = !self.finish_track,
            Setting::Repeat => {
                let i = Repeat::ALL.iter().position(|r| *r == self.queue.repeat()).unwrap_or(0);
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
            Setting::Output => {
                let i = Output::ALL.iter().position(|o| *o == self.output).unwrap_or(0);
                self.output = Output::ALL[cycle(i, Output::ALL.len())];
                self.play(PlayerCmd::Output(self.output.id()));
            }
            Setting::HideAccount => self.hide_account = !self.hide_account,
            Setting::RemainingTime => self.remaining_time = !self.remaining_time,
            Setting::Lastfm if !lastfm::available() => self.set_status("В этой сборке нет ключа Last.fm"),
            Setting::Lastfm => match self.lastfm {
                Lastfm::Off => self.connect_lastfm(),
                Lastfm::Connecting { .. } => {
                    self.lastfm = Lastfm::Off;
                    self.set_status("Подключение Last.fm отменено");
                }
                Lastfm::On(_) => {
                    self.lastfm = Lastfm::Off;
                    self.lastfm_backlog.clear();
                    self.set_status("Last.fm отключён");
                }
            },
            Setting::About | Setting::Logout => {}
        }
    }

    fn change_volume(&mut self, d: f64) {
        self.volume = (self.volume + d).clamp(0.0, MAX_VOLUME);
        self.play(PlayerCmd::Volume(self.volume));
    }

    fn selected_track(&self) -> Option<&Track> {
        match self.tab {
            Tab::Liked => self.liked_state.selected().and_then(|i| self.likes.tracks().get(i)),
            Tab::Search => self.search_state.selected().and_then(|i| self.search.results().get(i)),
            Tab::Wave => self.queue_state.selected().and_then(|i| self.queue.tracks().get(i)),
            Tab::Playlists => {
                let open = self.playlists.open.as_ref()?;
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
            Tab::Liked => self.liked_state.select(step(self.liked_state.selected(), self.likes.tracks().len())),
            Tab::Search => {
                self.search_state.select(step(self.search_state.selected(), self.search.results().len()));
                self.maybe_load_more_search();
            }
            Tab::Playlists => match &self.playlists.open {
                Some(open) => self.open_state.select(step(self.open_state.selected(), open.tracks.len())),
                None => self.playlists_state.select(step(self.playlists_state.selected(), self.playlists.list().len())),
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
                    self.play_list("Мне нравится", self.liked_context(), self.likes.tracks().to_vec(), i);
                }
            }
            Tab::Search => {
                if let Some(i) = self.search_state.selected() {
                    self.play_search("Поиск", self.search.results().to_vec(), i, false);
                }
            }
            Tab::Playlists if self.playlists.open.is_some() => self.play_open(false),
            Tab::Playlists => self.open_selected_playlist(),
        }
    }

    // ------------------------------------------------------------ playlists

    fn selected_playlist(&self) -> Option<&Playlist> {
        self.playlists_state.selected().and_then(|i| self.playlists.list().get(i))
    }

    fn open_selected_playlist(&mut self) {
        let Some(pl) = self.selected_playlist().cloned() else { return };
        self.playlists.open(pl);
        self.open_state = TableState::default();
        self.reload_open_playlist();
    }

    fn reload_open_playlist(&mut self) {
        if let Some((pl, req)) = self.playlists.reload_open() {
            self.task(Task::OpenPlaylist { pl, req });
        }
    }

    /// Plays the opened playlist from the selected track (or all of it shuffled).
    fn play_open(&mut self, shuffle: bool) {
        let Some(open) = &self.playlists.open else { return };
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
        self.playlists.playing = Some(key);
    }

    /// Own playlists (only they can be edited).
    fn own_playlists(&self) -> Vec<Playlist> {
        self.playlists.own(&self.api.uid)
    }

    /// Playlist targeted by rename: the opened one or the one selected in the list.
    fn target_playlist(&self) -> Option<Playlist> {
        match &self.playlists.open {
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
        if choices.is_empty() && self.playlists.loading() {
            self.set_status("Плейлисты ещё загружаются…");
            return;
        }
        self.modal = Some(Modal::AddTo { track, choices, sel: 0 });
    }

    fn ask_remove_from_playlist(&mut self) {
        let Some(open) = &self.playlists.open else { return };
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
            Modal::Rename { .. } | Modal::NewPlaylist { .. } | Modal::OpenLink { .. } => key.code,
            _ => press.hotkey.code,
        };
        let clear = is_ctrl(&press.hotkey, 'u');
        match modal {
            Modal::AddTo { track, choices, mut sel } => match code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Down | KeyCode::Char('j') => {
                    // The row after the playlists is «+ Новый плейлист».
                    sel = (sel + 1).min(choices.len());
                    self.modal = Some(Modal::AddTo { track, choices, sel });
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.saturating_sub(1);
                    self.modal = Some(Modal::AddTo { track, choices, sel });
                }
                KeyCode::Enter if sel == choices.len() => {
                    self.modal = Some(Modal::NewPlaylist { input: String::new(), track: Some(track) });
                }
                KeyCode::Enter => {
                    let pl = choices[sel].clone();
                    self.set_status(format!("Добавление в «{}»…", pl.title));
                    self.task(Task::AddToPlaylist { pl, track });
                }
                _ => self.modal = Some(Modal::AddTo { track, choices, sel }),
            },
            Modal::ConfirmRemove { pl, index, track } => {
                if code == KeyCode::Enter {
                    self.set_status(format!("Удаление из «{}»…", pl.title));
                    self.task(Task::RemoveFromPlaylist { pl, index, track });
                }
            }
            Modal::ConfirmDelete { pl } => {
                if code == KeyCode::Enter {
                    self.set_status(format!("Удаление плейлиста «{}»…", pl.title));
                    self.task(Task::DeletePlaylist(pl));
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
                    self.task(Task::RenamePlaylist { pl, title });
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
            Modal::NewPlaylist { mut input, track } => match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let title = input.trim().to_string();
                    if title.is_empty() {
                        self.modal = Some(Modal::NewPlaylist { input, track });
                        return;
                    }
                    self.set_status(format!("Создание плейлиста «{title}»…"));
                    self.task(Task::CreatePlaylist { title, track });
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::NewPlaylist { input, track });
                }
                _ if clear => self.modal = Some(Modal::NewPlaylist { input: String::new(), track }),
                KeyCode::Char(c) if is_typed(&key) => {
                    input.push(c);
                    self.modal = Some(Modal::NewPlaylist { input, track });
                }
                _ => self.modal = Some(Modal::NewPlaylist { input, track }),
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
        let Some(pl) = self.playlists_state.selected().and_then(|i| self.playlists.list().get(i)).cloned() else {
            return;
        };
        self.set_status(format!("Загрузка плейлиста «{}»…", pl.title));
        let intent = self.new_intent();
        self.task(Task::PlayPlaylist { intent, pl, shuffle });
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
        self.task(Task::CopyWithTool(url));
    }

    /// Asks for a link; a Yandex Music link from the clipboard is filled in once read
    /// (clipboard tools may take a while, so the window opens at once).
    fn ask_link(&mut self) {
        self.modal = Some(Modal::OpenLink { input: String::new() });
        self.task(Task::ReadClipboard);
    }

    /// Plays a track, album or playlist from a Yandex Music link; other links are not opened.
    fn play_link(&mut self, url: &str) {
        let link = match crate::api::parse_link(url) {
            Ok(link) => link,
            Err(e) => return self.set_status(e),
        };
        self.set_status("Загрузка по ссылке…");
        let intent = self.new_intent();
        self.task(Task::Link { intent, link });
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
        self.task(Task::Artist { intent, id, name });
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
        self.task(Task::Album { intent, id, fallback });
    }

    fn shuffle_play(&mut self) {
        if self.tab == Tab::Search {
            self.play_search("Поиск", self.search.results().to_vec(), 0, true);
        } else {
            self.play_shuffled("Мне нравится", self.liked_context(), self.likes.tracks().to_vec());
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
/// The length of `t` the catalogue tells, seconds.
fn length(t: &Track) -> Option<f64> {
    (t.duration_ms > 0).then(|| t.duration_ms as f64 / 1000.0)
}

/// An event of the wave session `w`, for the track batch `batch` (the session's latest if none).
fn feedback(w: &Wave, event: Value, batch: Option<String>) -> Task {
    let batch = batch.unwrap_or_else(|| w.batch.clone());
    Task::WaveFeedback { station: w.station.clone(), session: w.session.clone(), batch, event }
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
        let mut app = App::for_test();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let pl = |k: &str, owner: &str| Playlist { owner: owner.into(), kind: k.into(), title: format!("P{k}"), track_count: 0 };
        app.playlists.set_loaded(vec![pl("1", "1"), pl("2", "999"), pl("3", "1")]);
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
        app.queue.replace(Source::None, vec![t.clone()]);
        app.queue_state.select(Some(0));

        // `a` (and `ф`) offers only own playlists; selection stays in bounds; Esc closes.
        app.on_key(key(KeyCode::Char('ф')));
        let Some(Modal::AddTo { choices, .. }) = &app.modal else { panic!("no picker") };
        assert_eq!(choices.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(), ["1", "3"]);
        for _ in 0..5 {
            app.on_key(key(KeyCode::Char('j')));
        }
        // The last row is «+ Новый плейлист».
        assert!(matches!(app.modal, Some(Modal::AddTo { sel: 2, .. })));
        for _ in 0..3 {
            app.on_key(key(KeyCode::Char('k')));
        }
        assert!(matches!(app.modal, Some(Modal::AddTo { sel: 0, .. })));
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        // A new playlist from that row: an empty name is not accepted, the track goes along.
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(&app.modal, Some(Modal::NewPlaylist { track: Some(t), .. }) if t.id == "7"));
        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_some() && app.take_tasks().is_empty());
        for c in "Новый".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(matches!(&app.take_tasks()[..], [Task::CreatePlaylist { title, track: Some(_) }] if title == "Новый"));
        // `N` (by the physical key, also as `Т`): a playlist without a track.
        app.on_key(key(KeyCode::Char('Т')));
        app.on_paste("Ещё");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(&app.take_tasks()[..], [Task::CreatePlaylist { title, track: None }] if title == "Ещё"));

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
        app.playlists.open = Some(crate::library::OpenPlaylist { pl: app.playlists.list()[0].clone(), tracks: vec![t], loading: false, req: 0 });
        app.open_state.select(Some(0));
        app.on_key(key(KeyCode::Char('x')));
        assert!(matches!(app.modal, Some(Modal::ConfirmRemove { index: 0, .. })));
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.modal.is_none());
        // Esc leaves the opened playlist.
        app.on_key(key(KeyCode::Esc));
        assert!(app.playlists.open.is_none());

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
        // «Вывод звука»: see `output_and_account_settings`.
        app.on_key(key(KeyCode::Char('j')));

        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert!(app.finish_track);
        app.on_key(key(KeyCode::Left));
        assert!(!app.finish_track);

        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.queue.repeat(), Repeat::All);
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.queue.repeat(), Repeat::One, "wraps around");
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
        app.take_player();
        app.on_key(key(KeyCode::Right));
        assert!(app.art == Art::Spectrum && app.clear_images);
        assert_eq!(app.take_player(), ["spectrum true"]);
        app.picker = Some(Picker::from_fontsize((8, 16)));
        app.load_cover(Some("http://x/cover".into()));
        assert!(app.covers_loading.is_empty());
        app.on_key(key(KeyCode::Right));
        assert_eq!((app.art, app.take_player()), (Art::None, vec!["spectrum false".to_string()]));
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
        // «Обратный отсчёт».
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.remaining_time);
        app.on_key(key(KeyCode::Left));
        assert!(!app.remaining_time);
        // «Скрыть аккаунт».
        app.on_key(key(KeyCode::Char('j')));
        // «Last.fm»: without the program's key nothing is asked for (see `lastfm_scrobbles`).
        app.on_key(key(KeyCode::Char('j')));
        if !lastfm::available() {
            app.take_tasks();
            app.on_key(key(KeyCode::Enter));
            assert!(matches!(app.lastfm, Lastfm::Off) && app.take_tasks().is_empty());
            assert_eq!(app.setting_value(Setting::Lastfm).as_deref(), Some("нет ключа"));
        }

        // «О программе» opens from the settings and returns there.
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.show_about && app.modal.is_none());
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.show_about);
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 13 })));

        // Logout asks first; Esc goes back to the settings.
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::ConfirmLogout)));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::Settings { sel: 14 })));
        assert!(!app.logout && !app.quit);
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_key(key(KeyCode::Char('o')));
        for _ in 0..Setting::ALL.len() {
            app.on_key(key(KeyCode::Char('j')));
        }
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        assert!(app.logout && app.quit);
    }

    /// «Доигрывать трек»: a new list goes after the playing track.
    /// Moving to a track without a loaded cover erases the image of the previous one.
    #[test]
    fn cover_cleared() {
        use super::*;
        let mut app = App::for_test();
        app.picker = Some(Picker::from_fontsize((8, 16)));
        let t = |id: &str, cover: Option<&str>| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: cover.map(String::from),
            batch: None,
        };
        app.queue.replace(Source::Wave, vec![t("a", Some("http://x/a")), t("b", None), t("c", Some("http://x/c"))]);
        app.covers.push(("http://x/a".into(), image::DynamicImage::new_rgb8(2, 2)));
        app.queue.select(0);
        assert!(app.current_cover().is_some());
        app.queue.select(1);
        assert!(app.current_cover().is_none() && std::mem::take(&mut app.clear_images));
        // Nothing shown any more: no further erasing.
        assert!(app.current_cover().is_none() && !app.clear_images);
        app.queue.select(0);
        assert!(app.current_cover().is_some());
        // The next cover is still loading.
        app.queue.select(2);
        assert!(app.current_cover().is_none() && app.clear_images);
    }

    /// «Вывод звука» moves the sound at once; an output that fails to open is reported and the
    /// setting shows the one still in use. «Скрыть аккаунт» is a plain switch.
    #[test]
    fn output_and_account_settings() {
        use super::*;
        use ratatui::crossterm::event::KeyEvent;
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let mut app = App::for_test();
        app.modal = Some(Modal::Settings { sel: Setting::Output.index() });
        app.on_key(key(KeyCode::Right));
        assert_eq!((app.output, app.take_player()), (Output::Pulse, vec!["output pulse".to_string()]));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        let last = *Output::ALL.last().unwrap();
        assert_eq!(app.output, last);
        assert_eq!(app.take_player().last().map(String::as_str), Some(format!("output {}", last.id().unwrap()).as_str()));
        app.on_msg(Msg::Player(PlayerEvent::OutputFailed { kept: Some("pulse".into()), error: "занято".into() }));
        assert_eq!(app.output, Output::Pulse);
        assert!(app.status_error);
        app.modal = Some(Modal::Settings { sel: Setting::HideAccount.index() });
        app.on_key(key(KeyCode::Enter));
        assert!(app.hide_account);
        assert_eq!(Output::from_id(Some("jack")), Output::Auto);
    }

    #[test]
    fn finish_track() {
        use super::*;
        let mut app = App::for_test();
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
        let ids = |app: &App| app.queue.tracks().iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        app.finish_track = true;
        app.queue.replace(Source::Wave, vec![t("a"), t("b")]);
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
        app.on_msg(Msg::Preload { generation: app.generation, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        app.take_player();

        // The playing track stays and comes first; the new list starts from the chosen track,
        // without a second copy of the playing one; the player drops the preloaded old next track.
        let generation = app.generation;
        app.play_list("Новый", PlayContext::default(), vec![t("x"), t("y"), t("a"), t("z")], 1);
        assert_eq!(ids(&app), ["a", "y", "z"]);
        assert_eq!((app.queue.pos(), app.source_name()), (Some(0), "Новый".to_string()));
        assert!(app.wave.is_none() && app.preload.is_none());
        assert_eq!(app.take_player(), ["clear"]);
        app.on_msg(Msg::Preload { generation, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert!(app.take_player().is_empty(), "late preload of the old list is dropped");
        assert!(app.queue.on_carried() && app.carried_wave.is_some(), "its end is reported to the old wave");

        // Another list before the track ends replaces the waiting one; the track keeps its wave.
        app.play_list("Ещё", PlayContext::default(), vec![t("p"), t("q")], 0);
        assert_eq!(ids(&app), ["a", "p", "q"]);
        assert!(app.queue.on_carried() && app.carried_wave.is_some());

        // The track ends: the new list plays.
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!((app.queue.pos(), app.loading_track), (Some(1), true));
        app.loading_track = false;

        // Paused: switched at once.
        app.paused = true;
        app.play_list("Сразу", PlayContext::default(), vec![t("m"), t("n")], 1);
        assert_eq!((ids(&app), app.queue.pos()), (vec!["m".to_string(), "n".to_string()], Some(1)));
        assert!(!app.queue.on_carried());
        app.paused = false;
        app.loading_track = false;

        // Off: switched at once.
        app.finish_track = false;
        app.play_list("Выкл", PlayContext::default(), vec![t("k")], 0);
        assert_eq!((ids(&app), app.queue.pos()), (vec!["k".to_string()], Some(0)));

        // Nothing plays after the queue ran out: switched at once.
        app.finish_track = true;
        app.loading_track = false;
        app.next(End::Eof);
        app.play_list("Снова", PlayContext::default(), vec![t("u"), t("v")], 0);
        assert_eq!((ids(&app), app.queue.pos()), (vec!["u".to_string(), "v".to_string()], Some(0)));
    }

    /// Preload state machine.
    /// What is asked of the network: on start, for a track to play and the one after it, on a
    /// dislike in a wave.
    #[test]
    fn tasks() {
        use super::*;
        let mut app = App::for_test();
        app.start();
        let started: Vec<_> = app.take_tasks();
        assert!(matches!(started[..], [Task::Liked, Task::Stations, Task::Playlists { .. }, Task::LessAi]), "{started:?}");

        let t = |id: &str| Track::from_json(&json!({"id": id, "title": id, "durationMs": 200_000, "available": true})).unwrap();
        app.play_list("test", PlayContext::new("playlist", "1:3"), vec![t("1"), t("2")], 0);
        let tasks = app.take_tasks();
        let generation = app.generation;
        assert!(
            matches!(&tasks[..], [Task::SendPlay(r), Task::TrackUrl { generation: g, full_id, quality: "nq" }]
                if r.track.id == "1" && *g == generation && full_id == "1"),
            "{tasks:?}"
        );
        // Once it plays, the next track's link is asked for.
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(0.5)));
        let tasks = app.take_tasks();
        assert!(matches!(&tasks[..], [Task::Preload { idx: 1, track, .. }] if track.id == "2"), "{tasks:?}");

        // A dislike in a wave: the server learns it twice (the dislike and the wave event), and
        // the next track is asked for.
        app.queue.replace(Source::Wave, vec![t("w1"), t("w2")]);
        app.wave = Some(Wave {
            station: "user:onyourwave".into(),
            name: "Моя волна".into(),
            session: Some("s".into()),
            batch: "b".into(),
            fetching: false,
            pending_next: false,
        });
        app.begin_track(0, false);
        app.take_tasks();
        app.on_key(KeyCode::Char('d'));
        let tasks = app.take_tasks();
        assert!(tasks.iter().any(|t| matches!(t, Task::Dislike(track) if track.id == "w1")), "{tasks:?}");
        let events: Vec<&str> = tasks
            .iter()
            .filter_map(|t| if let Task::WaveFeedback { event, .. } = t { event["type"].as_str() } else { None })
            .collect();
        assert_eq!(events, ["dislike", "skip", "trackStarted"]);
        assert!(tasks.iter().any(|t| matches!(t, Task::TrackUrl { full_id, .. } if full_id == "w2")));
    }

    /// A download broken off midway: a fresh link, and the track goes on from where it stopped.
    #[test]
    fn resume_after_failure() {
        use super::*;
        let mut app = App::for_test();
        let t = |id: &str| Track::from_json(&json!({"id": id, "title": id, "durationMs": 200_000, "available": true})).unwrap();
        app.play_list("test", PlayContext::default(), vec![t("1"), t("2")], 0);
        app.on_msg(Msg::Url { generation: app.generation, result: Ok("http://x/1".into()) });
        assert_eq!(app.take_player(), ["stop", "load http://x/1"]);
        app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(40.0)));
        app.take_tasks();
        let error = PlayerEvent::Error { message: "соединение оборвалось".into(), network: true };
        app.on_msg(Msg::Player(error));
        // A moment for the connection to come back, in silence.
        assert!(app.take_tasks().is_empty());
        assert_eq!(app.take_player(), ["stop"]);
        assert!(app.loading_track && app.status.contains("повтор 1 из 2"));
        app.tick();
        assert!(app.take_tasks().is_empty(), "not yet");
        app.retry_at = app.retry_at.map(|(_, from)| (Instant::now(), from));
        app.tick();
        assert!(matches!(&app.take_tasks()[..], [Task::TrackUrl { full_id, .. }] if full_id == "1"));
        assert_eq!(app.take_player(), ["stop"]);
        app.on_msg(Msg::Url { generation: app.generation, result: Ok("http://x/1b".into()) });
        // With the length from the catalogue, for a file that does not tell its own.
        let [Effect::Player(PlayerCmd::Load(file))] = &app.effects[..] else { panic!("no load") };
        assert_eq!((file.start, file.length), (40.0, Some(200.0)));
        assert_eq!(app.take_player(), ["load http://x/1b from 40"]);
        assert_eq!(app.failures, 1);
        // Failing again before it goes on: still from there.
        app.on_msg(Msg::Player(PlayerEvent::Error { message: "нет сети".into(), network: true }));
        assert_eq!(app.retry_at.map(|(_, from)| from), Some(40.0));
        assert_eq!(app.queue.pos(), Some(0));
    }

    /// `n` on a slow connection: the old track stops at once, and its end or failure reported
    /// before the player has dropped it does not move the queue past the chosen track.
    #[test]
    fn switch_while_link_comes() {
        use super::*;
        let mut app = App::for_test();
        let t = |id: &str| Track::from_json(&json!({"id": id, "title": id, "durationMs": 200_000, "available": true})).unwrap();
        app.play_list("test", PlayContext::default(), vec![t("1"), t("2"), t("3")], 0);
        app.on_msg(Msg::Url { generation: app.generation, result: Ok("http://x/1".into()) });
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(199.5)));
        app.on_key(KeyCode::Char('n'));
        assert_eq!(app.take_player(), ["stop"]);
        // Sent by the player before the stop reached it.
        app.player_behind = 1;
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        app.on_msg(Msg::Player(PlayerEvent::Error { message: "обрыв".into(), network: true }));
        assert_eq!((app.queue.pos(), app.failures, app.retry_at.is_none()), (Some(1), 0, true));
        app.on_msg(Msg::Player(PlayerEvent::Cleared));
        app.on_msg(Msg::Url { generation: app.generation, result: Ok("http://x/2".into()) });
        assert_eq!(app.take_player(), ["load http://x/2"]);
        app.on_msg(Msg::Player(PlayerEvent::Cleared));
        // Now the player speaks of «2».
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.queue.pos(), Some(2));
    }

    /// A track that fails not because of the network is tried once more, then skipped.
    #[test]
    fn broken_track() {
        use super::*;
        let mut app = App::for_test();
        let t = |id: &str| Track::from_json(&json!({"id": id, "title": id, "available": true})).unwrap();
        app.play_list("test", PlayContext::default(), vec![t("1"), t("2")], 0);
        app.take_player();
        app.take_tasks();
        let broken = || Msg::Player(PlayerEvent::Error { message: "не тот формат".into(), network: false });
        app.on_msg(broken());
        assert!(app.retry_at.is_none(), "at once");
        assert!(matches!(&app.take_tasks()[..], [Task::TrackUrl { full_id, .. }] if full_id == "1"));
        app.take_player();
        app.on_msg(broken());
        assert_eq!(app.queue.pos(), Some(1));
        // A link that does not come because of the server is waited for.
        let busy = ApiError::new(crate::api::ErrorKind::Http(503), "HTTP 503");
        app.on_msg(Msg::Url { generation: app.generation, result: Err(busy) });
        assert_eq!((app.queue.pos(), app.failures, app.retry_at.is_some()), (Some(1), 1, true));
    }

    /// A refused token stops playback at once instead of trying track after track.
    #[test]
    fn refused_token() {
        use super::*;
        let mut app = App::for_test();
        let t = |id: &str| Track::from_json(&json!({"id": id, "title": id, "available": true})).unwrap();
        app.play_list("test", PlayContext::default(), vec![t("1"), t("2"), t("3")], 0);
        // An ordinary failure: the link is asked for again.
        app.on_msg(Msg::Url { generation: app.generation, result: Err("нет сети".into()) });
        assert!(!app.stopped && app.failures == 1);
        let refused = ApiError::new(crate::api::ErrorKind::Unauthorized, "HTTP 401: токен недействителен, выполните `ymusic login` (x)");
        app.take_player();
        app.on_msg(Msg::Url { generation: app.generation, result: Err(refused) });
        assert!(app.stopped);
        assert_eq!(app.take_player(), ["stop"]);
        assert_eq!(app.queue.pos(), Some(0), "no next track tried");
        assert!(app.status.contains("ymusic login"));
    }

    #[test]
    fn preload_flow() {
        use super::*;
        let mut app = App::for_test();
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
        app.queue.replace(Source::List("test".into()), vec![t("a"), t("b"), t("c")]);
        app.begin_track(0, false);
        app.loading_track = false;
        app.take_player();

        // Stale response (older generation) is ignored.
        app.on_msg(Msg::Preload { generation: app.generation - 1, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert!(app.preload.is_none());
        assert!(app.take_player().is_empty());
        // Fresh response is appended.
        app.on_msg(Msg::Preload { generation: app.generation, idx: 1, id: "b".into(), result: Ok("http://x/b".into()) });
        assert_eq!(app.preload, Some((1, "b".to_string())));
        assert_eq!(app.take_player(), ["append http://x/b"]);

        // End of track: the player moves on by itself, state follows without a new URL request.
        app.take_tasks();
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.queue.pos(), Some(1));
        assert!(app.failures == 0 && app.preload.is_none());
        assert!(app.take_player().is_empty());
        assert!(!app.take_tasks().iter().any(|t| matches!(t, Task::TrackUrl { .. })));

        // Manual skip uses the preloaded entry too.
        app.loading_track = false;
        app.on_msg(Msg::Preload { generation: app.generation, idx: 2, id: "c".into(), result: Ok("http://x/c".into()) });
        app.next(End::Skip);
        assert_eq!(app.queue.pos(), Some(2));
        assert_eq!(app.take_player(), ["append http://x/c", "next"]);

        // Error on a preloaded track retries the same track with a fresh URL.
        app.on_track_error(false);
        assert_eq!(app.queue.pos(), Some(2));
        assert_eq!(app.failures, 1);
    }

    #[test]
    fn shuffle_from_system() {
        use super::*;
        let mut app = App::for_test();
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
        let ids = |app: &App| app.queue.tracks().iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        let own: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        app.play_list("Альбом", PlayContext::default(), own.iter().map(|id| t(id)).collect(), 5);
        app.loading_track = false;
        assert!(!app.queue.shuffled());

        // On: the current track goes first, the rest are shuffled; the name tells it.
        use crate::media::Remote;
        app.on_remote(Remote::Shuffle(true));
        assert!(app.queue.shuffled());
        assert_eq!((app.queue.pos(), app.queue.tracks()[0].id.as_str()), (Some(0), "5"));
        assert_ne!(ids(&app), own);
        let mut sorted = ids(&app);
        sorted.sort_by_key(|id| id.parse::<u32>().unwrap());
        assert_eq!(sorted, own);
        assert_eq!(app.source_name(), "Альбом (в случайном порядке)");
        assert!(app.media_state().0.shuffle);

        // Off: the list's own order, the current track where it belongs.
        app.begin_track(3, false);
        let cur = app.queue.tracks()[3].id.clone();
        app.on_remote(Remote::Shuffle(false));
        assert_eq!(ids(&app), own);
        assert_eq!(app.current().map(|t| t.id.clone()), Some(cur));
        assert_eq!(app.source_name(), "Альбом");

        // `s` plays a list shuffled: shuffle shows as on and turns off to the list's order.
        app.likes.set_loaded(own.iter().map(|id| t(id)).collect());
        app.tab = Tab::Liked;
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.queue.shuffled());
        app.on_remote(Remote::Shuffle(false));
        assert_eq!(ids(&app), own);

        // A wave is not shuffled.
        app.queue.set_source(Source::Wave);
        app.on_remote(Remote::Shuffle(true));
        assert!(!app.queue.shuffled());
    }

    #[test]
    fn repeat_modes() {
        use super::*;
        let mut app = App::for_test();
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
        app.queue.replace(Source::List("test".into()), vec![t("a"), t("b"), t("c")]);
        app.begin_track(0, false);
        app.loading_track = false;
        app.take_player();

        // One track: the ended track plays again, the preload asks for it; `n` still moves on.
        app.set_repeat(Repeat::One);
        assert_eq!(app.following(End::Eof), Some(0));
        assert_eq!(app.following(End::Skip), Some(1));
        assert_eq!(app.take_player(), ["clear"]);
        app.on_msg(Msg::Preload { generation: app.generation, idx: 0, id: "a".into(), result: Ok("http://x/a".into()) });
        assert_eq!(app.preload, Some((0, "a".to_string())));
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.queue.pos(), Some(0));
        assert!(app.play.is_some(), "a new playback is reported");
        // Switching the mode drops the preloaded repeat.
        app.loading_track = false;
        app.on_msg(Msg::Preload { generation: app.generation, idx: 0, id: "a".into(), result: Ok("http://x/a".into()) });
        app.take_player();
        app.set_repeat(Repeat::Off);
        assert!(app.preload.is_none());
        assert_eq!(app.take_player(), ["clear"]);

        // The whole queue: after the last track — the first one.
        app.begin_track(2, false);
        app.loading_track = false;
        assert_eq!(app.following(End::Eof), None);
        app.set_repeat(Repeat::All);
        assert_eq!(app.following(End::Eof), Some(0));
        assert_eq!(app.following(End::Skip), Some(0));
        // A wave has no end to go round.
        app.queue.set_source(Source::Wave);
        assert_eq!(app.following(End::Eof), None);
        app.queue.set_source(Source::List("test".into()));
        app.next(End::Eof);
        assert_eq!(app.queue.pos(), Some(0));
    }

    #[test]
    fn stalled_load() {
        use super::*;
        let mut app = App::for_test();
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
        app.queue.replace(Source::List("test".into()), vec![t("a"), t("b")]);
        app.begin_track(0, false);
        let past = || Some(Instant::now() - Duration::from_secs(1));
        app.on_msg(Msg::Url { generation: app.generation, result: Ok("http://x/a".into()) });
        assert_eq!(app.take_player(), ["load http://x/a"]);

        // Still loading while paused: the timer restarts instead of firing.
        app.load_deadline = past();
        app.paused = true;
        app.tick();
        assert!(app.load_deadline.is_some_and(|d| d > Instant::now()));
        app.paused = false;

        // Two stalls: each time, a while later, the same track is requested again (fresh link).
        for n in 1..=2 {
            let generation = app.generation;
            app.load_deadline = past();
            app.tick();
            assert!(app.status_error && app.load_deadline.is_none() && app.retry_at.is_some());
            app.retry_at = app.retry_at.map(|(_, from)| (Instant::now(), from));
            app.tick();
            assert_eq!((app.queue.pos(), app.generation, app.failures), (Some(0), generation + 1, n));
            app.on_msg(Msg::Url { generation: app.generation, result: Ok(format!("http://x/a{n}")) });
        }

        // The third one: the next track.
        app.load_deadline = past();
        app.tick();
        assert_eq!(app.queue.pos(), Some(1));
        assert!(app.status_error && app.status.contains("3 попытки"));
        assert!(app.failures == 0 && app.retry_at.is_none() && app.load_deadline.is_none());

        // Once playback starts, the watchdog is off.
        app.on_msg(Msg::Url { generation: app.generation, result: Ok("http://x/b".into()) });
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(0.5)));
        assert!(app.load_deadline.is_none());
    }

    #[test]
    fn spectrum_bars() {
        use super::*;
        let mut app = App::for_test();
        app.set_art(Art::Spectrum);
        let n = crate::player::SPECTRUM_BANDS;
        // Nothing plays: nothing is asked for and levels are ignored.
        app.spectrum_asked -= SPECTRUM_PERIOD;
        app.take_player();
        app.tick();
        app.on_msg(Msg::Player(PlayerEvent::Spectrum(vec![0.0; n])));
        assert!(!app.spectrum_moving() && !app.take_player().contains(&"spectrum?".to_string()));

        app.queue.replace(Source::None, vec![Track::from_json(&json!({"id": 1})).unwrap()]);
        app.queue.select(0);
        app.spectrum_asked -= SPECTRUM_PERIOD;
        app.tick();
        assert!(app.take_player().contains(&"spectrum?".to_string()));
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
        let mut app = App::for_test();
        // `o` with Caps Lock in the Russian layout: «Щ», the settings window all the same.
        app.on_key(press('Щ', 'o', KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::Settings { .. })));
        app.on_key(KeyCode::Esc);
        // The search field takes the letters as typed; Ctrl+U (Ctrl+Г) clears it.
        app.tab = Tab::Search;
        app.search.editing = true;
        app.on_key(press('Д', 'l', KeyModifiers::NONE));
        app.on_key(press('а', 'f', KeyModifiers::NONE));
        assert_eq!(app.search.input, "Да");
        app.on_key(press('г', 'u', KeyModifiers::CONTROL));
        assert_eq!(app.search.input, "");
    }

    /// While the link of the next track is resolved the previous one still plays: its positions
    /// are not the new track's, and do not count as its start.
    #[test]
    fn positions_of_the_previous_track_are_ignored() {
        use super::*;
        let mut app = App::for_test();
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
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Duration(180.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(120.0)));
        assert_eq!((app.time_pos, app.loading_track), (120.0, false));
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(120.5)));
        assert_eq!((app.time_pos, app.loading_track), (0.0, true));
        // The old track still plays until the player has dropped it (before the link to «b»
        // comes: `awaiting_start` covers a player that is late with it).
        app.on_msg(Msg::Player(PlayerEvent::Cleared));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(121.0)));
        assert_eq!((app.time_pos, app.loading_track), (0.0, true));
        // «b» starts; its length is unknown to the player, the one from the API stays.
        app.on_msg(Msg::Player(PlayerEvent::Duration(0.0)));
        assert_eq!(app.duration, 200.0);
        app.on_msg(Msg::Player(PlayerEvent::TimePos(0.25)));
        assert_eq!((app.time_pos, app.loading_track), (0.25, false));
    }

    /// Last.fm: connected by a token confirmed in the browser; the track is shown as playing once it
    /// plays and scrobbled after half of it; scrobbles that failed for the network go with the next.
    #[test]
    fn lastfm_scrobbles() {
        use super::*;
        let mut app = App::for_test();
        // Connecting: a token, the browser, then polls until the user has confirmed.
        app.connect_lastfm();
        assert!(matches!(app.take_tasks()[..], [Task::LastfmToken]));
        app.on_msg(Msg::LastfmToken(Ok("tok".into())));
        assert!(matches!(&app.take_tasks()[..], [Task::OpenUrl(url)] if url.ends_with("&token=tok")));
        assert!(matches!(app.lastfm, Lastfm::Connecting { token: Some(_), .. }));
        let Lastfm::Connecting { poll_at, .. } = &mut app.lastfm else { panic!() };
        *poll_at = Instant::now();
        app.tick();
        assert!(matches!(&app.take_tasks()[..], [Task::LastfmSession(t)] if t == "tok"));
        app.tick();
        assert!(app.take_tasks().is_empty(), "one question at a time");
        let pending = lastfm::Error { code: 14, message: "not authorized".into(), blocked: false };
        app.on_msg(Msg::LastfmSession { token: "tok".into(), result: Err(pending) });
        app.tick();
        assert!(app.take_tasks().is_empty(), "asked again a while later");
        app.on_msg(Msg::LastfmSession { token: "tok".into(), result: Ok(("me".into(), "sk".into())) });
        assert!(app.lastfm_session().is_some_and(|s| s.name == "me" && s.key == "sk"));

        let t = |id: &str, secs: u64| Track {
            id: id.to_string(),
            album_id: Some("9".into()),
            title: format!("Трек {id}"),
            artists: "A, B".into(),
            main_artist: Some(("1".into(), "A".into())),
            album: "Альбом".into(),
            duration_ms: secs * 1000,
            available: true,
            cover: None,
            batch: None,
        };
        let pos = |app: &mut App, d: f64, t: f64| {
            app.take_player();
            app.on_msg(Msg::Player(PlayerEvent::Duration(d)));
            app.on_msg(Msg::Player(PlayerEvent::TimePos(t)));
        };
        let lastfm_tasks = |app: &mut App| -> Vec<Task> {
            app.take_tasks()
                .into_iter()
                .filter(|t| matches!(t, Task::LastfmNowPlaying { .. } | Task::LastfmScrobble { .. }))
                .collect()
        };
        app.play_list("Альбом", PlayContext::new("album", "9"), vec![t("a", 100), t("b", 20), t("c", 100)], 0);
        assert!(lastfm_tasks(&mut app).is_empty(), "nothing until it plays");
        pos(&mut app, 100.0, 0.5);
        let now = lastfm_tasks(&mut app);
        let [Task::LastfmNowPlaying { session, track }] = &now[..] else { panic!("{now:?}") };
        assert_eq!((session.as_str(), track.artist.as_str(), track.track.as_str(), track.duration), ("sk", "A", "Трек a", 100));
        for i in 2..=100 {
            pos(&mut app, 100.0, f64::from(i) * 0.5);
        }
        let sent = lastfm_tasks(&mut app);
        let [Task::LastfmScrobble { tracks, .. }] = &sent[..] else { panic!("{sent:?}") };
        assert_eq!(tracks.len(), 1);
        let a = tracks[0].clone();
        // The network failed: it goes again with the next scrobble.
        app.on_msg(Msg::LastfmSent {
            session: "sk".into(),
            tracks: vec![a.clone()],
            result: Err(lastfm::Error { code: 0, message: "нет сети".into(), blocked: false }),
        });
        // A track of 30 s or shorter is only shown as playing.
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        for i in 1..=40 {
            pos(&mut app, 20.0, f64::from(i) * 0.5);
        }
        assert!(matches!(&lastfm_tasks(&mut app)[..], [Task::LastfmNowPlaying { .. }]));
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        for i in 1..=101 {
            pos(&mut app, 100.0, f64::from(i) * 0.5);
        }
        let sent = lastfm_tasks(&mut app);
        let [_, Task::LastfmScrobble { tracks, .. }] = &sent[..] else { panic!("{sent:?}") };
        assert_eq!((tracks.len(), &tracks[0], tracks[1].track.as_str()), (2, &a, "Трек c"));

        // A revoked session disconnects.
        app.on_msg(Msg::LastfmSent {
            session: "sk".into(),
            tracks: Vec::new(),
            result: Err(lastfm::Error { code: 9, message: "Invalid session key".into(), blocked: false }),
        });
        assert!(matches!(app.lastfm, Lastfm::Off) && app.status_error);
    }

    /// The terminal title tells what plays.
    #[test]
    fn window_title() {
        use super::*;
        let mut app = App::for_test();
        assert_eq!(app.window_title(), "ymusic");
        let t = Track {
            id: "1".into(),
            album_id: None,
            title: "Выхода нет".into(),
            artists: "Сплин".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 220_000,
            available: true,
            cover: None,
            batch: None,
        };
        app.play_list("Альбом", PlayContext::new("album", "9"), vec![t], 0);
        assert_eq!(app.window_title(), "Сплин — Выхода нет");
    }

    #[test]
    fn play_reports() {
        use super::*;
        let mut app = App::for_test();
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
            app.take_player();
            app.on_msg(Msg::Player(PlayerEvent::Duration(200.0)));
            app.on_msg(Msg::Player(PlayerEvent::TimePos(t)));
        };
        app.play_list("Альбом", PlayContext::new("album", "9"), vec![t("a"), t("b"), t("c")], 0);
        let start = app.plays_sent().last().unwrap().clone();
        assert_eq!((start.track.id.as_str(), start.reason, start.played), ("a", None, 0.0));
        assert_eq!(start.context, PlayContext::new("album", "9"));

        // 30 seconds of listening are reported once, with the same play id.
        for i in 1..=70 {
            pos(&mut app, f64::from(i) * 0.5);
        }
        assert_eq!(app.plays_sent().len(), 2);
        let partial = &app.plays_sent()[1];
        assert_eq!((partial.play_id.as_str(), partial.reason), (start.play_id.as_str(), Some("in_progress")));
        assert!((30.1..31.0).contains(&partial.played), "{}", partial.played);

        // A seek does not count as listening; a pause is reported.
        pos(&mut app, 150.0);
        app.on_msg(Msg::Player(PlayerEvent::Pause(true)));
        pos(&mut app, 150.0);
        app.on_msg(Msg::Player(PlayerEvent::Pause(false)));
        let paused = app.plays_sent().last().unwrap().clone();
        assert_eq!((paused.reason, paused.pause, paused.seek), (None, true, true));
        assert_eq!(app.plays_sent().len(), 3);

        // `n`: the end of «a» (skip) and the start of «b».
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        let end = &app.plays_sent()[3];
        assert_eq!((end.track.id.as_str(), end.reason, end.end_position), ("a", Some("skip"), 150.0));
        assert!((34.0..36.0).contains(&end.played), "{}", end.played);
        let b = app.plays_sent()[4].clone();
        assert_eq!((b.track.id.as_str(), b.reason), ("b", None));
        assert_ne!(b.play_id, start.play_id);

        // A restart of the same track after an error goes on with its playback.
        pos(&mut app, 1.0);
        app.on_track_error(false);
        assert_eq!(app.plays_sent().len(), 5);

        // The end of the file, then back to the previous track.
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert_eq!(app.plays_sent()[5].reason, Some("finish"));
        assert_eq!(app.plays_sent()[6].track.id, "c");
        pos(&mut app, 1.0);
        app.on_key(KeyEvent::from(KeyCode::Char('b')));
        assert_eq!((app.plays_sent()[7].track.id.as_str(), app.plays_sent()[7].reason), ("c", Some("back_skip")));

        // Another list ends the track as `other_track`; quitting gives the last report to send.
        app.play_list("Мне нравится", app.liked_context(), vec![t("x")], 0);
        let other = &app.plays_sent()[9];
        assert_eq!((other.track.id.as_str(), other.reason), ("b", Some("other_track")));
        assert_eq!(app.plays_sent()[10].context, PlayContext::new("playlist", "1:3"));
        let quit = app.quit_play().unwrap();
        assert_eq!((quit.track.id.as_str(), quit.reason, quit.pause), ("x", None, true));
        assert!(app.quit_play().is_none());
    }

    #[test]
    fn search_paging() {
        use super::*;
        let mut app = App::for_test();
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
        let search = |app: &mut App, q: &str| {
            app.search.input = q.into();
            app.on_search_key(KeyCode::Enter.into());
        };
        app.tab = Tab::Search;
        search(&mut app, "q");
        assert!(app.search.loading());
        app.on_msg(Msg::Search { query: "q".into(), page: 0, result: Ok((page(0..100), Some(250))) });
        assert_eq!((app.search.results().len(), app.search.total(), app.search.loading()), (100, Some(250), false));

        // Far from the end: nothing more is requested.
        app.move_sel(10);
        assert!(!app.search.loading());
        // Close to the end: the next page is requested.
        app.move_sel(75);
        assert!(app.search.loading());

        // Replies to another query or page are ignored.
        app.on_msg(Msg::Search { query: "old".into(), page: 1, result: Ok((page(500..600), Some(900))) });
        app.on_msg(Msg::Search { query: "q".into(), page: 0, result: Ok((page(500..600), Some(900))) });
        assert_eq!(app.search.results().len(), 100);

        // Overlapping tracks are not duplicated; selection stays in place.
        app.on_msg(Msg::Search { query: "q".into(), page: 1, result: Ok((page(90..190), Some(250))) });
        assert_eq!(app.search.results().len(), 190);
        assert_eq!(app.search_state.selected(), Some(85));

        // Playing from the search: later pages go to the queue, playing near its end loads more.
        app.search_state.select(Some(0));
        app.activate();
        assert_eq!((app.queue.len(), app.queue.pos(), app.search.loading()), (190, Some(0), false));
        app.begin_track(175, false);
        assert!(app.search.loading());
        app.on_msg(Msg::Search { query: "q".into(), page: 2, result: Ok((page(180..290), Some(400))) });
        assert_eq!((app.search.results().len(), app.queue.len()), (290, 290));
        assert_eq!(app.queue.tracks()[289].id, "289");
        // A new search does not touch the playing queue.
        search(&mut app, "new");
        app.on_msg(Msg::Search { query: "new".into(), page: 0, result: Ok((page(1000..1100), Some(900))) });
        assert_eq!(app.queue.len(), 290);

        // A page without new tracks ends the paging.
        app.move_sel(1000);
        assert!(app.search.loading());
        app.on_msg(Msg::Search { query: "new".into(), page: 1, result: Ok((page(1000..1100), Some(900))) });
        assert_eq!(app.search.total(), Some(100));
        app.move_sel(1000);
        assert!(!app.search.loading());

        // The website API does not count the results: pages are loaded until the last one.
        search(&mut app, "web");
        app.on_msg(Msg::Search { query: "web".into(), page: 0, result: Ok((page(0..100), None)) });
        assert_eq!(app.search.total(), None);
        app.move_sel(1000);
        assert!(app.search.loading());
        app.on_msg(Msg::Search { query: "web".into(), page: 1, result: Ok((page(100..130), Some(130))) });
        assert_eq!((app.search.results().len(), app.search.total()), (130, Some(130)));
        app.move_sel(1000);
        assert!(!app.search.loading());
    }

    /// A like set while «Мне нравится» is loading survives the server's (older) answer;
    /// an older playlists reply does not replace a newer one.
    #[test]
    fn replies_during_reload() {
        use super::*;
        let mut app = App::for_test();
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
        let ids = |app: &App| app.likes.tracks().iter().map(|t| t.id.clone()).collect::<Vec<_>>();

        // Liked before the first load finished: the server's list does not have it yet.
        app.start();
        assert!(app.likes.loading());
        app.toggle_like(t("5"));
        app.on_msg(Msg::LikedIds(vec!["1:10".into(), "2:20".into()]));
        assert!(app.likes.contains("5") && app.likes.contains("1"));
        app.on_msg(Msg::Liked(Ok(vec![t("1"), t("2")])));
        assert_eq!(ids(&app), ["5", "1", "2"]);
        assert!(!app.likes.loading());

        // `R`, then a like is removed while the old list is still coming.
        app.load_liked();
        app.load_liked();
        app.toggle_like(t("1"));
        app.on_msg(Msg::LikedIds(vec!["5:1".into(), "1:10".into(), "2:20".into()]));
        assert!(!app.likes.contains("1"));
        app.on_msg(Msg::Liked(Ok(vec![t("5"), t("1"), t("2")])));
        assert_eq!(ids(&app), ["5", "2"]);

        // After the load, likes apply directly.
        app.toggle_like(t("2"));
        assert_eq!(ids(&app), ["5"]);

        let pl = |k: &str| Playlist { owner: "1".into(), kind: k.into(), title: k.into(), track_count: 0 };
        app.take_tasks();
        app.load_playlists();
        let [Task::Playlists { req }] = app.take_tasks()[..] else { panic!("no request") };
        app.on_msg(Msg::Playlists { req: req - 1, result: Ok(vec![pl("old")]) });
        assert!(app.playlists.list().is_empty() && app.playlists.loading());
        app.on_msg(Msg::Playlists { req, result: Ok(vec![pl("new")]) });
        assert_eq!(app.playlists.list()[0].kind, "new");
    }

    /// Errors are marked by a flag, not by the wording; copying reports how the link was copied.
    #[test]
    fn status_kinds() {
        use super::*;
        let mut app = App::for_test();
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
        let mut app = App::for_test();
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

        app.search.editing = true;
        app.on_paste("рок\tq");
        assert_eq!(app.search.input, "рокq");
    }

    /// Replies to an older play request do not replace what the user started later.
    #[test]
    fn stale_play_replies() {
        use super::*;
        let mut app = App::for_test();
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
        assert!(*app.queue.source() == Source::List("Альбом".into()));

        // A list started by hand drops a link that was still loading.
        let link = app.new_intent();
        app.play_list("Мне нравится", PlayContext::default(), vec![t("c")], 0);
        app.on_msg(list(link, "Трек по ссылке", vec![t("d")]));
        assert_eq!(app.queue.tracks()[0].id, "c");
    }
}
