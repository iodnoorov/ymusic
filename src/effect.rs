//! What the app's logic asks of the world outside: commands to the player and background tasks
//! (requests to Yandex and Last.fm, the clipboard). The logic only lists them (`App::take_effects`); the main
//! loop carries them out (`Runtime`). So every request the program makes is listed here, and
//! tests see what was asked for without a network or sound.

use crate::api::lastfm::{self, Scrobble};
use crate::api::{Api, ApiError, Link, PlayContext, PlayReport, Playlist, Track};
use crate::app::{Msg, PodcastMsg, PodcastTask};
use crate::player::{Player, PlayerCmd};
use crate::Lock;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub enum Effect {
    Player(PlayerCmd),
    /// Boxed: some tasks carry a whole track or report.
    Task(Box<Task>),
}

/// Background work; its results come back as messages.
#[derive(Debug)]
pub enum Task {
    /// «Мне нравится»: the ids first (`LikedIds`), then the tracks.
    Liked,
    Podcast(PodcastTask),
    Playlists { req: u64 },
    Stations,
    /// «Меньше ИИ-музыки» of the account.
    LessAi,
    SetLessAi(bool),
    /// A record for the listening history (nothing comes back).
    SendPlay(PlayReport),
    /// An event of a wave session (nothing comes back).
    WaveFeedback { station: String, session: Option<String>, batch: String, event: Value },
    WaveStart { intent: u64, station: String, name: String, settings: Option<(&'static str, &'static str)> },
    WaveMore { wave: u64, station: String, session: Option<String>, recent: Vec<String> },
    /// The link to the current track.
    TrackUrl { generation: u64, full_id: String, quality: &'static str },
    /// The link to the next track, played after the current one.
    Preload { generation: u64, idx: usize, track: Track, quality: &'static str },
    Cover(String),
    SetLike { track: Track, like: bool, req: u64 },
    Dislike(Track),
    Search { generation: u64, query: String, page: usize },
    /// The tracks of the playlist opened in «Плейлисты».
    OpenPlaylist { pl: Playlist, req: u64 },
    /// A playlist to play.
    PlayPlaylist { intent: u64, pl: Playlist, shuffle: bool },
    AddToPlaylist { pl: Playlist, track: Track },
    RemoveFromPlaylist { pl: Playlist, index: usize, track: Track },
    DeletePlaylist(Playlist),
    RenamePlaylist { pl: Playlist, title: String },
    /// A new playlist, with `track` added to it.
    CreatePlaylist { title: String, track: Option<Track> },
    Artist { intent: u64, id: String, name: String },
    /// An album; `fallback` names it if the server gives no title.
    Album { intent: u64, id: String, fallback: String },
    Link { intent: u64, link: Link },
    /// Copies a link with a clipboard tool (OSC 52 is sent at once by the app).
    CopyWithTool(String),
    /// Looks for a Yandex Music link in the clipboard.
    ReadClipboard,
    /// Opens a link in the browser.
    OpenUrl(String),
    LastfmToken { req: u64 },
    /// Whether the user has confirmed the token yet.
    LastfmSession(String),
    LastfmNowPlaying { session: String, track: Scrobble },
    LastfmScrobble { session: String, req: u64, tracks: Vec<Scrobble> },
}

fn text(e: anyhow::Error) -> ApiError {
    ApiError::from_anyhow(&e)
}

impl Task {
    /// Does the work; `send` takes what comes back.
    pub fn run(self, api: &Api, send: impl Fn(Msg)) {
        let msg = match self {
            Task::Podcast(task) => Msg::Podcasts(task.run(api)),
            Task::Liked => {
                let ids = match api.liked() {
                    Ok(r) => r,
                    Err(e) => return send(Msg::Liked(Err(text(e)))),
                };
                send(Msg::LikedIds(ids.clone()));
                let tracks = api.tracks(&ids);
                Msg::Liked(tracks.map_err(text))
            }
            Task::Playlists { req } => Msg::Playlists { req, result: api.playlists().map_err(text) },
            Task::Stations => match api.stations() {
                Ok(s) if !s.is_empty() => Msg::Stations(s),
                _ => return,
            },
            Task::LessAi => Msg::LessAi(api.ai_reduction().map_err(text)),
            Task::SetLessAi(on) => Msg::LessAiSet { on, result: api.set_ai_reduction(on).map_err(text) },
            Task::SendPlay(report) => {
                let _ = api.send_play(&report);
                return;
            }
            Task::WaveFeedback { station, session, batch, event } => {
                let _ = api.wave_feedback(&station, session.as_deref(), &batch, event);
                return;
            }
            Task::WaveStart { intent, station, name, settings } => {
                let result = api.wave_start(&station, settings).map_err(text);
                Msg::WaveStarted { intent, station, name, result }
            }
            Task::WaveMore { wave, station, session, recent } => {
                Msg::WaveMore { wave, result: api.wave_more(&station, session.as_deref(), &recent).map_err(text) }
            }
            Task::TrackUrl { generation, full_id, quality } => {
                Msg::Url { generation, result: api.track_url(&full_id, quality).map_err(text) }
            }
            Task::Preload { generation, idx, track, quality } => {
                let result = api.track_url(&track.full_id(), quality).map_err(text);
                Msg::Preload { generation, idx, id: track.id, result }
            }
            Task::Cover(url) => {
                let result = api.fetch_image(&url).map_err(text);
                Msg::Cover { url, result }
            }
            Task::SetLike { track, like, req } => {
                let result = api.set_like(&track, like).map_err(text);
                Msg::LikeSet { id: track.id, req, result }
            }
            Task::Dislike(track) => match api.set_dislike(&track, true) {
                Ok(()) => return,
                Err(e) => Msg::Error(format!("Ошибка дизлайка: {e:#}")),
            },
            Task::Search { generation, query, page } => {
                let result = api.search(&query, page).map_err(text);
                Msg::Search { generation, query, page, result }
            }
            Task::OpenPlaylist { pl, req } => {
                let result = api.playlist_tracks(&pl).map_err(text);
                Msg::PlaylistOpened { key: pl.key(), req, result }
            }
            Task::PlayPlaylist { intent, pl, shuffle } => {
                let result = api.playlist_tracks(&pl).map_err(text);
                let (name, context) = (format!("Плейлист: {}", pl.title), PlayContext::new("playlist", pl.key()));
                Msg::ListLoaded { intent, name, context, shuffle, playlist: Some(pl.key()), result }
            }
            Task::AddToPlaylist { pl, track } => {
                let result = api
                    .add_to_playlist(&pl, &track)
                    .map(|()| format!("Добавлено в «{}»: {}", pl.title, track.title))
                    .map_err(text);
                Msg::PlaylistEdited { key: pl.key(), result }
            }
            Task::RemoveFromPlaylist { pl, index, track } => {
                let result = api
                    .remove_from_playlist(&pl, index, &track.id)
                    .map(|()| format!("Удалено из «{}»: {}", pl.title, track.title))
                    .map_err(text);
                Msg::PlaylistEdited { key: pl.key(), result }
            }
            Task::DeletePlaylist(pl) => {
                let result = api.delete_playlist(&pl).map(|()| format!("Плейлист удалён: «{}»", pl.title)).map_err(text);
                Msg::PlaylistEdited { key: pl.key(), result }
            }
            Task::RenamePlaylist { pl, title } => {
                let result = api.rename_playlist(&pl, &title).map(|()| format!("Плейлист переименован: «{title}»")).map_err(text);
                Msg::PlaylistEdited { key: pl.key(), result }
            }
            Task::CreatePlaylist { title, track } => match api.create_playlist(&title) {
                Ok(pl) => {
                    let result = match &track {
                        Some(t) => api
                            .add_to_playlist(&pl, t)
                            .map(|()| format!("Создан плейлист «{title}», добавлено: {}", t.title))
                            .map_err(|e| text(e.context(format!("плейлист «{title}» создан, но трек не добавлен")))),
                        None => Ok(format!("Создан плейлист «{title}»")),
                    };
                    Msg::PlaylistEdited { key: pl.key(), result }
                }
                Err(e) => Msg::PlaylistEdited { key: String::new(), result: Err(text(e)) },
            },
            Task::Artist { intent, id, name } => {
                let result = api.artist_tracks(&id).map_err(text);
                let context = PlayContext::new("artist", id);
                Msg::ListLoaded { intent, name: format!("Исполнитель: {name}"), context, shuffle: false, playlist: None, result }
            }
            Task::Album { intent, id, fallback } => {
                let result = api.album_tracks(&id).map_err(text);
                let name = match &result {
                    Ok((title, _)) if !title.is_empty() => format!("Альбом: {title}"),
                    _ => format!("Альбом: {fallback}"),
                };
                let context = PlayContext::new("album", id);
                Msg::ListLoaded { intent, name, context, shuffle: false, playlist: None, result: result.map(|(_, t)| t) }
            }
            Task::Link { intent, link } => linked(api, intent, link),
            Task::CopyWithTool(url) => Msg::Copied { by_tool: crate::clipboard::copy_with_tool(&url), url },
            Task::ReadClipboard => {
                match crate::clipboard::paste().map(|s| s.trim().to_string()).filter(|s| crate::api::parse_link(s).is_ok()) {
                    Some(link) => Msg::ClipboardLink(link),
                    None => return,
                }
            }
            Task::OpenUrl(url) => Msg::Opened { ok: open_url(&url), url },
            Task::LastfmToken { req } => Msg::LastfmToken { req, result: lastfm::token() },
            Task::LastfmSession(token) => Msg::LastfmSession { result: lastfm::session(&token), token },
            Task::LastfmNowPlaying { session, track } => {
                let result = lastfm::now_playing(&session, &track);
                Msg::LastfmSent { session, req: None, result: result.map(|()| 0) }
            }
            Task::LastfmScrobble { session, req, tracks } => {
                let result = lastfm::scrobble(&session, &tracks);
                Msg::LastfmSent { session, req: Some(req), result }
            }
        };
        send(msg);
    }
}

/// Opens `url` with the desktop's browser; false if that cannot be done. The opener usually
/// returns at once; one that keeps running (it may wait for the browser) is left to it.
fn open_url(url: &str) -> bool {
    use std::process::{Command, Stdio};
    let bin = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let Ok(mut child) = Command::new(bin).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    else {
        return false;
    };
    for _ in 0..30 {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(_) => return false,
        }
    }
    std::thread::spawn(move || child.wait());
    true
}

/// A track, album or playlist from a Yandex Music link, to be played.
fn linked(api: &Api, intent: u64, link: Link) -> Msg {
    let titled = |kind: &str, r: anyhow::Result<(String, Vec<Track>)>| match r {
        Ok((title, tracks)) => {
            let name = if title.is_empty() { format!("{kind} по ссылке") } else { format!("{kind}: {title}") };
            (name, Ok(tracks))
        }
        Err(e) => (format!("{kind} по ссылке"), Err(text(e))),
    };
    let (name, result) = match &link {
        Link::Track(id) => {
            let tracks = api.tracks(std::slice::from_ref(id)).map_err(text);
            if let Ok(list) = &tracks
                && list.first().is_some_and(Track::is_podcast) {
                return Msg::Podcasts(PodcastMsg::Linked {
                    intent, id: id.split(':').next().unwrap_or(id).to_string(),
                });
            }
            ("Трек по ссылке".to_string(), tracks)
        }
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
    Msg::ListLoaded { intent, name, context, shuffle: false, playlist: None, result }
}

impl Task {
    /// A saturated queue must complete the protocol too: no spinner or optimistic write may
    /// remain waiting forever for a task that was not accepted.
    fn rejected(&self) -> Msg {
        let error = || ApiError::from_anyhow(&anyhow::anyhow!("очередь фоновых задач заполнена, повторите действие"));
        let lastfm_error =
            || lastfm::Error {
                code: 0, message: "очередь фоновых задач заполнена".into(), blocked: false
            };
        match self {
            Task::Podcast(task) => Msg::Podcasts(task.rejected(error())),
            Task::Liked => Msg::Liked(Err(error())),
            Task::Playlists { req } => Msg::Playlists { req: *req, result: Err(error()) },
            Task::LessAi => Msg::LessAi(Err(error())),
            Task::SetLessAi(on) => Msg::LessAiSet { on: *on, result: Err(error()) },
            Task::WaveStart { intent, station, name, .. } => {
                Msg::WaveStarted { intent: *intent, station: station.clone(), name: name.clone(), result: Err(error()) }
            }
            Task::WaveMore { wave, .. } => Msg::WaveMore { wave: *wave, result: Err(error()) },
            Task::TrackUrl { generation, .. } => Msg::Url { generation: *generation, result: Err(error()) },
            Task::Preload { generation, idx, track, .. } => {
                Msg::Preload { generation: *generation, idx: *idx, id: track.id.clone(), result: Err(error()) }
            }
            Task::Cover(url) => Msg::Cover { url: url.clone(), result: Err(error()) },
            Task::SetLike { track, req, .. } => Msg::LikeSet { id: track.id.clone(), req: *req, result: Err(error()) },
            Task::Search { generation, query, page } => {
                Msg::Search { generation: *generation, query: query.clone(), page: *page, result: Err(error()) }
            }
            Task::OpenPlaylist { pl, req } => Msg::PlaylistOpened { key: pl.key(), req: *req, result: Err(error()) },
            Task::LastfmToken { req } => Msg::LastfmToken { req: *req, result: Err(lastfm_error()) },
            Task::LastfmSession(token) => Msg::LastfmSession { token: token.clone(), result: Err(lastfm_error()) },
            Task::LastfmNowPlaying { session, .. } => {
                Msg::LastfmSent { session: session.clone(), req: None, result: Err(lastfm_error()) }
            }
            Task::LastfmScrobble { session, req, .. } => {
                Msg::LastfmSent { session: session.clone(), req: Some(*req), result: Err(lastfm_error()) }
            }
            _ => Msg::Error(error().to_string()),
        }
    }

    /// The user waits for it: something is to play. Others (covers, reports, lists loading in the
    /// background) never hold it up — see `Pool`.
    fn urgent(&self) -> bool {
        matches!(
            self,
            Task::TrackUrl { .. }
                | Task::Preload { .. }
                | Task::WaveStart { .. }
                | Task::WaveMore { .. }
                | Task::Search { .. }
                | Task::PlayPlaylist { .. }
                | Task::Artist { .. }
                | Task::Album { .. }
                | Task::Link { .. }
        ) || matches!(self, Task::Podcast(task) if task.urgent())
    }

    /// A link asked for a track the user has already moved away from (`generation` is the app's
    /// current one): not worth a request, which on a slow connection would hold up the new one.
    fn stale(&self, generation: u64) -> bool {
        matches!(self, Task::TrackUrl { generation: g, .. } | Task::Preload { generation: g, .. } if *g != generation)
    }
}

/// Carries out the effects: player commands at once, tasks in the `Pool`.
pub struct Runtime {
    pub player: Player,
    tx: Sender<Msg>,
    pool: Pool,
    /// The app's track generation, as of the last `run`.
    generation: Arc<AtomicU64>,
}

impl Runtime {
    pub fn new(player: Player, tx: Sender<Msg>) -> Runtime {
        Runtime { player, tx, pool: Pool::new(WORKERS, WORKERS - 1), generation: Arc::default() }
    }

    /// `api`: as the tasks are to use it (the app's settings applied); `generation`: the app's
    /// current track generation (`App::generation`).
    pub fn run(&mut self, effects: Vec<Effect>, api: &Api, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
        for effect in effects {
            match effect {
                Effect::Player(cmd) => self.player.apply(cmd),
                Effect::Task(task) => {
                    let (api, tx) = (api.clone(), self.tx.clone());
                    let (urgent, current) = (task.urgent(), self.generation.clone());
                    let rejected = task.rejected();
                    let accepted = self.pool.run(urgent, move || {
                        if task.stale(current.load(Ordering::Relaxed)) {
                            return;
                        }
                        task.run(&api, |m| {
                            let _ = tx.send(m);
                        });
                    });
                    if !accepted { let _ = self.tx.send(rejected); }
                }
            }
        }
    }
}

/// Threads for the tasks: at most so many requests go to the server at once (it answers 429
/// «Concurrency limit exceeded» to more).
const WORKERS: usize = 4;
const MAX_QUEUED: usize = 256;

type Job = Box<dyn FnOnce() + Send>;

/// A fixed set of threads taking jobs from two queues: urgent ones first, and the others on at
/// most `max_background` threads, so that one is always free for an urgent job.
struct Pool {
    shared: std::sync::Arc<PoolShared>,
}

struct PoolShared {
    queues: std::sync::Mutex<Queues>,
    changed: std::sync::Condvar,
    max_background: usize,
}

#[derive(Default)]
struct Queues {
    stopped: bool,
    urgent: std::collections::VecDeque<Job>,
    background: std::collections::VecDeque<Job>,
    /// Background jobs running now.
    running: usize,
}

impl Pool {
    fn new(threads: usize, max_background: usize) -> Pool {
        let shared = std::sync::Arc::new(PoolShared {
            queues: std::sync::Mutex::default(),
            changed: std::sync::Condvar::new(),
            max_background,
        });
        for n in 0..threads {
            let shared = shared.clone();
            // Threads end with the program.
            let _ = std::thread::Builder::new().name(format!("task-{n}")).spawn(move || shared.work());
        }
        Pool { shared }
    }

    fn run(&self, urgent: bool, job: impl FnOnce() + Send + 'static) -> bool {
        let mut q = self.shared.queues.locked();
        if q.stopped || q.urgent.len() + q.background.len() >= MAX_QUEUED { return false; }
        if urgent {
            q.urgent.push_back(Box::new(job));
        } else {
            q.background.push_back(Box::new(job));
        }
        self.shared.changed.notify_one();
        true
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let mut q = self.shared.queues.locked();
        q.stopped = true;
        q.urgent.clear();
        q.background.clear();
        self.shared.changed.notify_all();
    }
}

impl PoolShared {
    fn work(&self) {
        loop {
            let (job, background) = {
                let mut q = self.queues.locked();
                loop {
                    if q.stopped { return; }
                    if let Some(job) = q.urgent.pop_front() {
                        break (job, false);
                    }
                    if q.running < self.max_background
                        && let Some(job) = q.background.pop_front()
                    {
                        q.running += 1;
                        break (job, true);
                    }
                    q = self.changed.wait(q).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            // A job that panics takes only itself down, not the thread.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            if background {
                self.queues.locked().running -= 1;
                // A background job may be waiting for this slot.
                self.changed.notify_all();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pool;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Links to tracks the user has moved away from are not asked for.
    #[test]
    fn stale_links() {
        use super::Task;
        let url = Task::TrackUrl { generation: 2, full_id: "1".into(), quality: "nq" };
        assert!(url.stale(3) && !url.stale(2));
        assert!(!Task::Stations.stale(3));
    }

    /// Slow background jobs never take the last thread: an urgent one gets through, and a fourth
    /// background job waits for a slot.
    #[test]
    fn urgent_jobs_get_through() {
        let pool = Pool::new(4, 3);
        let (done, finished) = mpsc::channel();
        let mut release = Vec::new();
        for n in 0..4 {
            let (go, wait) = mpsc::channel::<()>();
            release.push(go);
            let done = done.clone();
            pool.run(false, move || {
                let _ = wait.recv();
                let _ = done.send(format!("background {n}"));
            });
        }
        let urgent = done.clone();
        pool.run(true, move || {
            let _ = urgent.send("urgent".into());
        });
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)).unwrap(), "urgent");
        // The fourth one has not started: releasing it alone does nothing yet.
        release[3].send(()).unwrap();
        assert!(finished.recv_timeout(Duration::from_millis(200)).is_err());
        release[0].send(()).unwrap();
        let mut rest: Vec<String> = (0..2).map(|_| finished.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        rest.sort();
        assert_eq!(rest, ["background 0", "background 3"]);
        // A job that panics leaves its thread working.
        pool.run(true, || panic!("job"));
        let after = done.clone();
        pool.run(true, move || {
            let _ = after.send("after".into());
        });
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)).unwrap(), "after");
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    #[test]
    fn saturation_rejects_work_and_drop_releases_queued_captures() {
        let pool = Pool::new(0, 0);
        let guard = Arc::new(());
        for _ in 0..MAX_QUEUED {
            let g = guard.clone();
            assert!(pool.run(false, move || drop(g)));
        }
        assert!(!pool.run(true, || {}));
        assert_eq!(Arc::strong_count(&guard), MAX_QUEUED + 1);
        drop(pool);
        assert_eq!(Arc::strong_count(&guard), 1);
        let task = Task::TrackUrl { generation: 42, full_id: "1".into(), quality: "nq" };
        assert!(matches!(task.rejected(), Msg::Url { generation: 42, result: Err(_) }));
    }
}
