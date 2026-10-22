//! What the app's logic asks of the world outside: commands to the player and background tasks
//! (requests to Yandex, the clipboard). The logic only lists them (`App::take_effects`); the main
//! loop carries them out (`Runtime`). So every request the program makes is listed here, and
//! tests see what was asked for without a network or sound.

use crate::api::{Api, ApiError, Link, PlayContext, PlayReport, Playlist, Track};
use crate::app::Msg;
use crate::player::{Player, PlayerCmd};
use crate::Lock;
use serde_json::Value;
use std::sync::mpsc::Sender;

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
    WaveMore { station: String, session: Option<String>, recent: Vec<String> },
    /// The link to the current track.
    TrackUrl { generation: u64, full_id: String, quality: &'static str },
    /// The link to the next track, played after the current one.
    Preload { generation: u64, idx: usize, track: Track, quality: &'static str },
    Cover(String),
    SetLike { track: Track, like: bool },
    Dislike(Track),
    Search { query: String, page: usize },
    /// The tracks of the playlist opened in «Плейлисты».
    OpenPlaylist { pl: Playlist, req: u64 },
    /// A playlist to play.
    PlayPlaylist { intent: u64, pl: Playlist, shuffle: bool },
    AddToPlaylist { pl: Playlist, track: Track },
    RemoveFromPlaylist { pl: Playlist, index: usize, track: Track },
    DeletePlaylist(Playlist),
    RenamePlaylist { pl: Playlist, title: String },
    Artist { intent: u64, id: String, name: String },
    /// An album; `fallback` names it if the server gives no title.
    Album { intent: u64, id: String, fallback: String },
    Link { intent: u64, link: Link },
    /// Copies a link with a clipboard tool (OSC 52 is sent at once by the app).
    CopyWithTool(String),
    /// Looks for a Yandex Music link in the clipboard.
    ReadClipboard,
}

fn text(e: anyhow::Error) -> ApiError {
    ApiError::from_anyhow(&e)
}

impl Task {
    /// Does the work; `send` takes what comes back.
    pub fn run(self, api: &Api, send: impl Fn(Msg)) {
        let msg = match self {
            Task::Liked => {
                let (ids, tracks) = match api.liked() {
                    Ok(r) => r,
                    Err(e) => return send(Msg::Liked(Err(text(e)))),
                };
                send(Msg::LikedIds(ids.clone()));
                let tracks = match tracks {
                    Some(t) => Ok(t),
                    None => api.tracks(&ids),
                };
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
            Task::WaveMore { station, session, recent } => {
                Msg::WaveMore(api.wave_more(&station, session.as_deref(), &recent).map_err(text))
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
            Task::SetLike { track, like } => match api.set_like(&track, like) {
                Ok(()) => return,
                Err(e) => Msg::LikeFailed { track, like, err: text(e) },
            },
            Task::Dislike(track) => match api.set_dislike(&track, true) {
                Ok(()) => return,
                Err(e) => Msg::Error(format!("Ошибка дизлайка: {e:#}")),
            },
            Task::Search { query, page } => {
                let result = api.search(&query, page).map_err(text);
                Msg::Search { query, page, result }
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
        };
        send(msg);
    }
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
        Link::Track(id) => ("Трек по ссылке".to_string(), api.tracks(std::slice::from_ref(id)).map_err(text)),
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
        )
    }
}

/// Carries out the effects: player commands at once, tasks in the `Pool`.
pub struct Runtime {
    pub player: Player,
    tx: Sender<Msg>,
    pool: Pool,
}

impl Runtime {
    pub fn new(player: Player, tx: Sender<Msg>) -> Runtime {
        Runtime { player, tx, pool: Pool::new(WORKERS, WORKERS - 1) }
    }

    /// `api`: as the tasks are to use it (the app's settings applied).
    pub fn run(&mut self, effects: Vec<Effect>, api: &Api) {
        for effect in effects {
            match effect {
                Effect::Player(cmd) => self.player.apply(cmd),
                Effect::Task(task) => {
                    let (api, tx) = (api.clone(), self.tx.clone());
                    let urgent = task.urgent();
                    self.pool.run(urgent, move || {
                        task.run(&api, |m| {
                            let _ = tx.send(m);
                        });
                    });
                }
            }
        }
    }
}

/// Threads for the tasks: at most so many requests go to the server at once (it answers 429
/// «Concurrency limit exceeded» to more).
const WORKERS: usize = 4;

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

    fn run(&self, urgent: bool, job: impl FnOnce() + Send + 'static) {
        let mut q = self.shared.queues.locked();
        if urgent {
            q.urgent.push_back(Box::new(job));
        } else {
            q.background.push_back(Box::new(job));
        }
        self.shared.changed.notify_one();
    }
}

impl PoolShared {
    fn work(&self) {
        loop {
            let (job, background) = {
                let mut q = self.queues.locked();
                loop {
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
