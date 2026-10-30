use super::*;
use crate::api::{Podcast, PodcastProgress, StreamProgress};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PodcastSection {
    #[default]
    Mine,
    New,
    Search,
}

impl PodcastSection {
    fn moved(self, direction: i32) -> Self {
        let all = [Self::Mine, Self::New, Self::Search];
        let index = all.iter().position(|s| *s == self).unwrap() as i32;
        all[(index + direction).rem_euclid(all.len() as i32) as usize]
    }
}

pub struct OpenPodcast {
    pub show: Podcast,
    pub tracks: Vec<Track>,
    pub loading: bool,
    pub total: usize,
    req: u64,
    pages: usize,
}

#[derive(Default)]
pub struct Podcasts {
    pub section: PodcastSection,
    pub shows: Vec<Podcast>,
    pub loading: bool,
    pub input: String,
    pub editing: bool,
    pub query: String,
    pub total: Option<usize>,
    pub open: Option<OpenPodcast>,
    pub liked: HashSet<String>,
    pub state: TableState,
    pub episode_state: TableState,
    pub description: Option<(String, String)>,
    pub description_scroll: u16,
    generation: u64,
    pages: usize,
    open_req: u64,
    description_req: u64,
    favorites_req: u64,
    favorites_loading: bool,
    pub(super) favorites_ready: bool,
    favorites: Vec<Podcast>,
    favorite_pending: Option<String>,
    favorite_after_load: Option<Podcast>,
    checkpoint_at: Option<Instant>,
}

#[derive(Debug)]
pub enum PodcastTask {
    Favorites { req: u64 },
    Shows { generation: u64, section: PodcastSection, query: String, page: usize },
    Episodes { req: u64, id: String, page: usize },
    Play { intent: u64, id: String, from_start: bool, checkpoint: Option<PodcastProgress> },
    Description { req: u64, id: String, title: String },
    Favorite { show: Podcast, like: bool },
    Save(PodcastProgress),
}

pub enum PodcastMsg {
    Favorites { req: u64, result: Result<Vec<Podcast>, ApiError> },
    Shows { generation: u64, page: usize, result: Result<(Vec<Podcast>, Option<usize>), ApiError> },
    Episodes { req: u64, id: String, page: usize, result: Result<(Podcast, Vec<Track>, usize), ApiError> },
    Play { intent: u64, from_start: bool, checkpoint: Option<PodcastProgress>, result: Result<Vec<Track>, ApiError> },
    Linked { intent: u64, id: String },
    Description { req: u64, title: String, result: Result<String, ApiError> },
    Favorite { show: Podcast, like: bool, result: Result<(), ApiError> },
    Saved { report: PodcastProgress, result: Result<StreamProgress, ApiError> },
}

impl PodcastMsg {
    pub(super) fn error(&self) -> Option<&ApiError> {
        match self {
            Self::Favorites { result: Err(e), .. }
            | Self::Shows { result: Err(e), .. }
            | Self::Episodes { result: Err(e), .. }
            | Self::Play { result: Err(e), .. }
            | Self::Description { result: Err(e), .. }
            | Self::Favorite { result: Err(e), .. }
            | Self::Saved { result: Err(e), .. } => Some(e),
            _ => None,
        }
    }
}

impl PodcastTask {
    pub(crate) fn urgent(&self) -> bool {
        matches!(self, Self::Play { .. } | Self::Shows { section: PodcastSection::Search, .. })
    }

    pub(crate) fn run(self, api: &Api) -> PodcastMsg {
        let error = |e: anyhow::Error| ApiError::from_anyhow(&e);
        match self {
            Self::Favorites { req } => PodcastMsg::Favorites { req, result: api.podcast_liked().map_err(error) },
            Self::Shows { generation, section, query, page } => {
                let result = match section {
                    PodcastSection::New => api.podcast_new().map(|shows| { let n = shows.len(); (shows, Some(n)) }),
                    PodcastSection::Search => api.podcast_search(&query, page),
                    PodcastSection::Mine => api.podcast_liked().map(|shows| { let n = shows.len(); (shows, Some(n)) }),
                };
                PodcastMsg::Shows { generation, page, result: result.map_err(error) }
            }
            Self::Episodes { req, id, page } => {
                let result = api.podcast_episodes(&id, page).map_err(error);
                PodcastMsg::Episodes { req, id, page, result }
            }
            Self::Play { intent, id, from_start, checkpoint } => {
                let result = (|| {
                    if let Some(report) = &checkpoint { api.podcast_save_progress(report)?; }
                    api.podcast_episode(&id).map(|track| vec![track])
                })().map_err(error);
                PodcastMsg::Play { intent, from_start, checkpoint, result }
            }
            Self::Description { req, id, title } => {
                PodcastMsg::Description { req, title, result: api.podcast_description(&id).map_err(error) }
            }
            Self::Favorite { show, like } => {
                let result = api.podcast_set_liked(&show.id, like).map_err(error);
                PodcastMsg::Favorite { show, like, result }
            }
            Self::Save(report) => {
                let result = api.podcast_save_progress(&report).map_err(error);
                PodcastMsg::Saved { report, result }
            }
        }
    }

    pub(crate) fn rejected(&self, error: ApiError) -> PodcastMsg {
        match self {
            Self::Favorites { req } => PodcastMsg::Favorites { req: *req, result: Err(error) },
            Self::Shows { generation, page, .. } => PodcastMsg::Shows { generation: *generation, page: *page, result: Err(error) },
            Self::Episodes { req, id, page } => PodcastMsg::Episodes { req: *req, id: id.clone(), page: *page, result: Err(error) },
            Self::Play { intent, from_start, checkpoint, .. } => PodcastMsg::Play {
                intent: *intent, from_start: *from_start, checkpoint: checkpoint.clone(), result: Err(error),
            },
            Self::Description { req, title, .. } => PodcastMsg::Description { req: *req, title: title.clone(), result: Err(error) },
            Self::Favorite { show, like } => PodcastMsg::Favorite { show: show.clone(), like: *like, result: Err(error) },
            Self::Save(report) => PodcastMsg::Saved { report: report.clone(), result: Err(error) },
        }
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingPodcastProgress {
    pub account: String,
    pub items: Vec<PodcastProgress>,
}

#[derive(Default)]
pub(super) struct ProgressOutbox {
    pending: PendingPodcastProgress,
    in_flight: Option<PodcastProgress>,
    retry_at: Option<Instant>,
    revision: u64,
}

impl ProgressOutbox {
    fn push(&mut self, report: PodcastProgress) {
        match self.pending.items.iter_mut().find(|p| p.track_id == report.track_id) {
            Some(old) if old.position == report.position && old.finished == report.finished => return,
            Some(old) => *old = report,
            None => self.pending.items.push(report),
        }
        self.revision = self.revision.wrapping_add(1);
    }

    fn next(&mut self) -> Option<PodcastProgress> {
        if self.in_flight.is_some() || self.retry_at.is_some_and(|at| Instant::now() < at) { return None; }
        let report = self.pending.items.first()?.clone();
        self.in_flight = Some(report.clone());
        Some(report)
    }

    fn complete(&mut self, report: &PodcastProgress, ok: bool) {
        if self.in_flight.as_ref() == Some(report) { self.in_flight = None; }
        if ok {
            if let Some(index) = self.pending.items.iter().position(|p| p == report) {
                self.pending.items.remove(index);
                self.revision = self.revision.wrapping_add(1);
            }
            self.retry_at = None;
        } else {
            self.retry_at = Some(Instant::now() + Duration::from_secs(60));
        }
    }
}

impl App {
    pub(super) fn ensure_podcasts(&mut self) {
        if !self.podcasts.favorites_ready && !self.podcasts.favorites_loading { self.reload_podcast_favorites(); }
        if self.podcasts.section == PodcastSection::Mine {
            self.podcasts.loading = self.podcasts.favorites_loading;
        }
    }

    fn reload_podcast_favorites(&mut self) {
        self.podcasts.favorites_req += 1;
        self.podcasts.favorites_loading = true;
        if self.podcasts.section == PodcastSection::Mine { self.podcasts.loading = true; }
        self.task(Task::Podcast(PodcastTask::Favorites { req: self.podcasts.favorites_req }));
    }

    pub(super) fn podcast_section(&mut self, direction: i32) {
        self.podcasts.section = self.podcasts.section.moved(direction);
        self.podcasts.open = None;
        self.podcasts.description = None;
        self.podcasts.editing = false;
        self.podcasts.state = TableState::default();
        self.reload_podcasts();
    }

    pub(super) fn reload_podcasts(&mut self) {
        if let Some(open) = &mut self.podcasts.open {
            open.pages = 0;
            open.total = usize::MAX;
            open.loading = false;
            self.load_podcast_episode_page();
            return;
        }
        self.podcasts.generation += 1;
        self.podcasts.pages = 0;
        self.podcasts.total = None;
        self.podcasts.loading = false;
        match self.podcasts.section {
            PodcastSection::Mine => {
                self.podcasts.shows = self.podcasts.favorites.clone();
                self.reload_podcast_favorites();
            }
            PodcastSection::New => self.load_podcast_show_page(),
            PodcastSection::Search if !self.podcasts.query.is_empty() => self.load_podcast_show_page(),
            PodcastSection::Search => { self.podcasts.shows.clear(); self.podcasts.editing = true; }
        }
    }

    fn load_podcast_show_page(&mut self) {
        if self.podcasts.loading { return; }
        self.podcasts.loading = true;
        self.task(Task::Podcast(PodcastTask::Shows {
            generation: self.podcasts.generation, section: self.podcasts.section,
            query: self.podcasts.query.clone(), page: self.podcasts.pages,
        }));
    }

    fn load_podcast_episode_page(&mut self) {
        let Some(open) = &mut self.podcasts.open else { return };
        if open.loading { return; }
        self.podcasts.open_req += 1;
        open.req = self.podcasts.open_req;
        open.loading = true;
        let task = PodcastTask::Episodes { req: open.req, id: open.show.id.clone(), page: open.pages };
        self.task(Task::Podcast(task));
    }

    pub(super) fn podcast_selected_track(&self) -> Option<&Track> {
        self.podcasts.episode_state.selected().and_then(|i| self.podcasts.open.as_ref()?.tracks.get(i))
    }

    fn selected_podcast_show(&self) -> Option<&Podcast> {
        self.podcasts.open.as_ref().map(|o| &o.show)
            .or_else(|| self.podcasts.state.selected().and_then(|i| self.podcasts.shows.get(i)))
    }

    pub(super) fn activate_podcast(&mut self, from_start: bool) {
        if let Some(track) = self.podcast_selected_track().cloned() {
            self.request_podcast_play(&track.id, from_start);
        } else if let Some(show) = self.selected_podcast_show().cloned() {
            self.podcasts.open = Some(OpenPodcast { show, tracks: Vec::new(), loading: false, total: usize::MAX, req: 0, pages: 0 });
            self.podcasts.episode_state = TableState::default();
            self.load_podcast_episode_page();
        }
    }

    pub(super) fn request_podcast_play(&mut self, id: &str, from_start: bool) {
        let intent = self.new_intent();
        self.set_status("Чтение позиции подкаста…");
        self.checkpoint_podcast(false);
        let checkpoint = self.podcast_progress.pending.items.iter().find(|p| p.track_id == id).cloned();
        self.task(Task::Podcast(PodcastTask::Play { intent, id: id.to_string(), from_start, checkpoint }));
    }

    pub(super) fn move_podcast_selection(&mut self, direction: i32) {
        let step = |current: Option<usize>, len: usize| {
            (len > 0).then(|| (current.unwrap_or(0) as i64 + i64::from(direction)).clamp(0, len as i64 - 1) as usize)
        };
        if let Some(open) = &self.podcasts.open {
            self.podcasts.episode_state.select(step(self.podcasts.episode_state.selected(), open.tracks.len()));
            if self.podcasts.episode_state.selected().unwrap_or(0).saturating_add(10) >= open.tracks.len()
                && open.tracks.len() < open.total && !open.loading {
                self.load_podcast_episode_page();
            }
        } else {
            self.podcasts.state.select(step(self.podcasts.state.selected(), self.podcasts.shows.len()));
            if self.podcasts.section == PodcastSection::Search && self.podcasts.pages > 0
                && self.podcasts.state.selected().unwrap_or(0).saturating_add(10) >= self.podcasts.shows.len()
                && self.podcasts.total.is_none_or(|n| self.podcasts.shows.len() < n) {
                self.load_podcast_show_page();
            }
        }
    }

    pub(super) fn edit_podcast_search(&mut self) {
        if self.podcasts.section != PodcastSection::Search {
            self.podcasts.section = PodcastSection::Search;
            self.podcasts.generation += 1;
            self.podcasts.loading = false;
            self.podcasts.shows.clear();
            self.podcasts.pages = 0;
            self.podcasts.total = None;
        }
        self.podcasts.open = None;
        self.podcasts.editing = true;
    }

    pub(super) fn on_podcast_key(&mut self, press: &Press) -> bool {
        if self.podcasts.description.is_some() {
            match press.hotkey.code {
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('I') => {
                    self.podcasts.description = None;
                    self.podcasts.description_req += 1;
                }
                KeyCode::Down | KeyCode::Char('j') => self.podcasts.description_scroll = self.podcasts.description_scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.podcasts.description_scroll = self.podcasts.description_scroll.saturating_sub(1),
                KeyCode::PageDown => self.podcasts.description_scroll = self.podcasts.description_scroll.saturating_add(10),
                KeyCode::PageUp => self.podcasts.description_scroll = self.podcasts.description_scroll.saturating_sub(10),
                KeyCode::Home | KeyCode::Char('g') => self.podcasts.description_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.podcasts.description_scroll = u16::MAX,
                _ => {}
            }
            return true;
        }
        if !self.podcasts.editing { return false; }
        let key = press.typed;
        match key.code {
            KeyCode::Esc => self.podcasts.editing = false,
            KeyCode::Enter => {
                let query = self.podcasts.input.trim().to_string();
                if !query.is_empty() {
                    self.podcasts.query = query;
                    self.podcasts.editing = false;
                    self.podcasts.state = TableState::default();
                    self.reload_podcasts();
                }
            }
            KeyCode::Backspace => { self.podcasts.input.pop(); }
            _ if is_ctrl(&press.hotkey, 'u') => self.podcasts.input.clear(),
            KeyCode::Char(c) if is_typed(&key) => self.podcasts.input.push(c),
            _ => {}
        }
        true
    }

    pub(super) fn podcast_description(&mut self) {
        self.podcasts.description_scroll = 0;
        if let Some(track) = self.podcast_selected_track().cloned() {
            self.podcasts.description_req += 1;
            self.podcasts.description = Some((track.title.clone(), "Загрузка описания…".into()));
            self.task(Task::Podcast(PodcastTask::Description {
                req: self.podcasts.description_req, id: track.id, title: track.title,
            }));
        } else if let Some(show) = self.selected_podcast_show() {
            self.podcasts.description = Some((show.title.clone(), show.description.clone()));
        }
    }

    pub(super) fn toggle_podcast_favorite(&mut self) {
        let Some(show) = self.selected_podcast_show().cloned() else { return };
        if !self.podcasts.favorites_ready {
            self.podcasts.favorite_after_load = Some(show);
            self.ensure_podcasts();
            return;
        }
        self.set_podcast_favorite(show);
    }

    fn set_podcast_favorite(&mut self, show: Podcast) {
        if self.podcasts.favorite_pending.is_some() { return; }
        let like = !self.podcasts.liked.contains(&show.id);
        self.podcasts.favorite_pending = Some(show.id.clone());
        self.task(Task::Podcast(PodcastTask::Favorite { show, like }));
    }

    pub(super) fn on_podcast_msg(&mut self, msg: PodcastMsg) {
        match msg {
            PodcastMsg::Favorites { req, result } => {
                if req != self.podcasts.favorites_req { return; }
                self.podcasts.favorites_loading = false;
                if self.podcasts.section == PodcastSection::Mine { self.podcasts.loading = false; }
                match result {
                    Ok(shows) => {
                        self.podcasts.liked = shows.iter().map(|s| s.id.clone()).collect();
                        self.podcasts.favorites = shows;
                        self.podcasts.favorites_ready = true;
                        if self.podcasts.section == PodcastSection::Mine {
                            self.podcasts.shows = self.podcasts.favorites.clone();
                            let n = self.podcasts.shows.len();
                            let selected = self.podcasts.state.selected().unwrap_or(0);
                            self.podcasts.state.select((n > 0).then(|| selected.min(n - 1)));
                        }
                        if let Some(show) = self.podcasts.favorite_after_load.take() { self.set_podcast_favorite(show); }
                    }
                    Err(e) => { self.podcasts.favorite_after_load = None; self.set_error(format!("Не удалось загрузить подкасты: {e}")); }
                }
            }
            PodcastMsg::Shows { generation, page, result } => {
                if generation != self.podcasts.generation || page != self.podcasts.pages { return; }
                self.podcasts.loading = false;
                match result {
                    Ok((shows, total)) => {
                        if page == 0 { self.podcasts.shows.clear(); }
                        let old = self.podcasts.shows.len();
                        let seen: HashSet<_> = self.podcasts.shows.iter().map(|s| s.id.as_str()).collect();
                        let added: Vec<_> = shows.into_iter().filter(|s| !seen.contains(s.id.as_str())).collect();
                        self.podcasts.shows.extend(added);
                        self.podcasts.total = if page > 0 && old == self.podcasts.shows.len() { Some(old) } else { total };
                        self.podcasts.pages += 1;
                        if page == 0 { self.podcasts.state.select((!self.podcasts.shows.is_empty()).then_some(0)); }
                    }
                    Err(e) => self.set_error(format!("Не удалось загрузить подкасты: {e}")),
                }
            }
            PodcastMsg::Episodes { req, id, page, result } => {
                let Some(open) = self.podcasts.open.as_mut().filter(|o| o.req == req && o.show.id == id && o.pages == page) else { return };
                open.loading = false;
                match result {
                    Ok((show, tracks, total)) => {
                        open.show = show;
                        if page == 0 { open.tracks.clear(); }
                        let old = open.tracks.len();
                        let seen: HashSet<_> = open.tracks.iter().map(|t| t.id.as_str()).collect();
                        let added: Vec<_> = tracks.into_iter().filter(|t| !seen.contains(t.id.as_str())).collect();
                        open.tracks.extend(added);
                        open.total = if page > 0 && old == open.tracks.len() { old } else { total };
                        open.pages += 1;
                        let selected = self.podcasts.episode_state.selected().unwrap_or(0);
                        self.podcasts.episode_state.select((!open.tracks.is_empty()).then(|| selected.min(open.tracks.len() - 1)));
                    }
                    Err(e) => self.set_error(format!("Не удалось открыть подкаст: {e}")),
                }
            }
            PodcastMsg::Linked { intent, id } => {
                if intent == self.intent { self.request_podcast_play(&id, false); }
            }
            PodcastMsg::Play { intent, from_start, checkpoint, result } => {
                if result.is_ok() && let Some(report) = &checkpoint {
                    self.podcast_progress.complete(report, true);
                }
                if intent != self.intent { return; }
                match result {
                    Ok(mut tracks) => {
                        let Some(track) = tracks.first_mut().filter(|t| t.is_podcast()) else {
                            self.set_error("Это не выпуск подкаста"); return;
                        };
                        if from_start { track.progress = Some(StreamProgress { position: 0.0, finished: false }); }
                        let name = format!("Подкаст: {}", track.album);
                        let context = PlayContext::new("album", track.album_id.clone().unwrap_or_default());
                        self.play_list(&name, context, tracks, 0);
                    }
                    Err(e) => self.set_error(format!("Не удалось прочитать выпуск: {e}")),
                }
            }
            PodcastMsg::Description { req, title, result } => {
                if req != self.podcasts.description_req || self.podcasts.description.is_none() { return; }
                match result {
                    Ok(text) => self.podcasts.description = Some((title, text)),
                    Err(e) => { self.podcasts.description = None; self.set_error(format!("Не удалось загрузить описание: {e}")); }
                }
            }
            PodcastMsg::Favorite { show, like, result } => {
                if self.podcasts.favorite_pending.as_deref() != Some(show.id.as_str()) { return; }
                self.podcasts.favorite_pending = None;
                match result {
                    Ok(()) => {
                        if like {
                            self.podcasts.liked.insert(show.id.clone());
                            if !self.podcasts.favorites.iter().any(|s| s.id == show.id) {
                                self.podcasts.favorites.push(show.clone());
                            }
                        } else {
                            self.podcasts.liked.remove(&show.id);
                            self.podcasts.favorites.retain(|s| s.id != show.id);
                        }
                        if self.podcasts.section == PodcastSection::Mine {
                            self.podcasts.shows = self.podcasts.favorites.clone();
                            let n = self.podcasts.shows.len();
                            let selected = self.podcasts.state.selected().unwrap_or(0);
                            self.podcasts.state.select((n > 0).then(|| selected.min(n - 1)));
                        }
                        self.set_status(format!("{}: {}", if like { "Подкаст добавлен в избранное" } else { "Подкаст удалён из избранного" }, show.title));
                        self.reload_podcast_favorites();
                    }
                    Err(e) => self.set_error(format!("Не удалось изменить избранное: {e}")),
                }
            }
            PodcastMsg::Saved { report, result } => self.podcast_progress_saved(&report, result),
        }
    }

    fn update_podcast_progress(&mut self, id: &str, progress: StreamProgress) {
        self.queue.set_stream_progress(id, progress.clone());
        if let Some(open) = &mut self.podcasts.open
            && let Some(track) = open.tracks.iter_mut().find(|t| t.id == id) {
            track.progress = Some(progress);
        }
    }

    pub(super) fn checkpoint_podcast(&mut self, finished: bool) {
        if !finished && (self.awaiting_start || self.stopped) { return; }
        let Some(track) = self.current().filter(|t| t.is_podcast()) else { return };
        let length = track.duration_ms as f64 / 1000.0;
        if length <= 0.0 { return; }
        let position = if finished { length } else {
            let Some(position) = self.confirmed_time_pos else { return };
            position.clamp(0.0, length)
        };
        let id = track.id.clone();
        let report = PodcastProgress { track_id: id.clone(), position, length, finished, timestamp: iso_now() };
        self.podcast_progress.pending.account = self.account.uid.clone();
        self.podcast_progress.push(report);
        self.podcasts.checkpoint_at = Some(Instant::now());
        self.update_podcast_progress(&id, StreamProgress { position: if finished { 0.0 } else { position }, finished });
    }

    pub(super) fn tick_podcast_progress(&mut self) {
        if self.current().is_some_and(Track::is_podcast)
            && !self.paused && self.podcasts.checkpoint_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
            self.checkpoint_podcast(false);
        }
        if !self.unauthorized && let Some(report) = self.podcast_progress.next() {
            self.task(Task::Podcast(PodcastTask::Save(report)));
        }
    }

    pub(crate) fn podcast_progress_saved(&mut self, report: &PodcastProgress, result: Result<StreamProgress, ApiError>) {
        self.podcast_progress.complete(report, result.is_ok());
        match result {
            Ok(progress) => {
                if !self.podcast_progress.pending.items.iter().any(|p| p.track_id == report.track_id) {
                    self.update_podcast_progress(&report.track_id, progress);
                }
            }
            Err(e) => self.set_error(format!("Позиция подкаста сохранена локально; отправка будет повторена: {e}")),
        }
    }

    pub fn restore_podcast_progress(&mut self, mut pending: PendingPodcastProgress) {
        if pending.account != self.account.uid { return; }
        pending.items.retain(|p| !p.track_id.is_empty() && p.length.is_finite() && p.length > 0.0
            && p.position.is_finite() && p.position >= 0.0 && p.position <= p.length && !p.timestamp.is_empty());
        self.podcast_progress.pending = pending;
    }

    pub fn pending_podcast_progress(&self) -> PendingPodcastProgress { self.podcast_progress.pending.clone() }
    pub fn podcast_progress_revision(&self) -> u64 { self.podcast_progress.revision }
    pub fn capture_podcast_progress(&mut self) { self.checkpoint_podcast(false); }

    pub(super) fn podcast_track_started(&mut self) {
        self.podcasts.checkpoint_at = Some(Instant::now());
    }
}

pub(super) fn resume_position(track: &Track) -> f64 {
    track.progress.as_ref().filter(|p| track.is_podcast() && !p.finished && p.position.is_finite())
        .map_or(0.0, |p| p.position.clamp(0.0, track.duration_ms as f64 / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(id: &str, position: f64) -> Track {
        Track::from_json(&json!({
            "id": id, "type": "podcast-episode", "title": id, "durationMs": 600000,
            "albums": [{"id": "show", "type": "podcast", "title": "Show"}],
            "streamProgress": {"endPositionSec": position, "everFinished": false},
        })).unwrap()
    }

    fn show(id: &str) -> Podcast {
        Podcast { id: id.into(), title: id.into(), description: String::new(), cover: None, episodes: 1 }
    }

    #[test]
    fn episode_resumes_and_stops_at_eof_even_with_music_repeat() {
        let mut app = App::for_test();
        app.tab = Tab::Podcasts;
        let intent = app.new_intent();
        app.on_msg(Msg::Podcasts(PodcastMsg::Play { intent, from_start: false, checkpoint: None, result: Ok(vec![episode("e", 240.5)]) }));
        assert!(app.tab == Tab::Podcasts);
        let generation = app.generation;
        app.take_player();
        app.on_msg(Msg::Url { generation, result: Ok("audio".into()) });
        assert!(app.take_player().iter().any(|c| c == "load audio from 240.5"));
        app.on_msg(Msg::Player(PlayerEvent::Duration(600.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(241.0)));
        app.set_repeat(Repeat::One);
        app.take_player();
        app.take_tasks();
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        assert!(app.current().is_none());
        assert_eq!(app.queue.repeat(), Repeat::One, "music preference is retained");
        let pending = app.pending_podcast_progress();
        assert_eq!(pending.items.len(), 1);
        assert!(pending.items[0].finished);
        assert_eq!(pending.items[0].position, 600.0);
        assert!(!app.take_player().iter().any(|c| c.starts_with("append ")));
    }

    #[test]
    fn closed_show_cannot_receive_an_old_episode_page() {
        let mut app = App::for_test();
        app.tab = Tab::Podcasts;
        app.podcasts.shows = vec![show("a"), show("b")];
        app.podcasts.state.select(Some(0));
        app.activate_podcast(false);
        let old_req = app.podcasts.open.as_ref().unwrap().req;
        app.act(Action::Back);
        app.podcasts.state.select(Some(1));
        app.activate_podcast(false);
        app.on_msg(Msg::Podcasts(PodcastMsg::Episodes {
            req: old_req, id: "a".into(), page: 0, result: Ok((show("a"), vec![episode("old", 0.0)], 1)),
        }));
        let open = app.podcasts.open.as_ref().unwrap();
        assert_eq!(open.show.id, "b");
        assert!(open.loading && open.tracks.is_empty());
    }

    #[test]
    fn progress_ack_does_not_delete_a_newer_checkpoint_and_retry_keeps_time() {
        let mut outbox = ProgressOutbox::default();
        let first = PodcastProgress {
            track_id: "e".into(), position: 10.0, length: 600.0, finished: false,
            timestamp: "2026-10-30T10:00:00.123Z".into(),
        };
        let later = PodcastProgress { position: 20.0, timestamp: "2026-10-30T10:00:01.123Z".into(), ..first.clone() };
        outbox.push(first.clone());
        assert_eq!(outbox.next(), Some(first.clone()));
        outbox.push(later.clone());
        outbox.complete(&first, true);
        assert_eq!(outbox.next(), Some(later.clone()));
        outbox.complete(&later, false);
        assert!(outbox.next().is_none());
        outbox.retry_at = None;
        assert_eq!(outbox.next(), Some(later));
    }

    #[test]
    fn podcast_actions_do_not_start_music_and_finished_episodes_restart_at_zero() {
        let mut app = App::for_test();
        app.tab = Tab::Podcasts;
        for action in [Action::Shuffle, Action::Dislike, Action::TrackWave, Action::AddToPlaylist] {
            app.act(action);
        }
        assert!(app.take_tasks().is_empty() && app.take_player().is_empty());
        let mut track = episode("done", 599.0);
        track.progress.as_mut().unwrap().finished = true;
        assert_eq!(resume_position(&track), 0.0);
        let intent = app.new_intent();
        app.on_msg(Msg::Podcasts(PodcastMsg::Play { intent, from_start: true, checkpoint: None, result: Ok(vec![episode("e", 240.0)]) }));
        assert_eq!(app.time_pos, 0.0);
        app.restore_podcast_progress(PendingPodcastProgress { account: "someone-else".into(), items: vec![PodcastProgress {
            track_id: "private".into(), position: 10.0, length: 600.0, finished: false, timestamp: iso_now(),
        }] });
        assert!(app.pending_podcast_progress().items.is_empty());
    }

    #[test]
    fn transport_regression_buffering_keeps_music_preload_and_success_accounting() {
        for initially_buffering in [false, true] {
            let mut app = App::for_test();
            let music = |id: &str| Track::from_json(&json!({
                "id": id, "title": id, "type": "music", "durationMs": 120000,
            })).unwrap();
            app.play_list("Music", PlayContext::default(), vec![music("a"), music("b")], 0);
            let generation = app.generation;
            app.take_player();
            app.on_msg(Msg::Url { generation, result: Ok("audio".into()) });
            app.take_player();
            app.take_tasks();
            app.errors_in_row = 3;
            app.on_msg(Msg::Player(PlayerEvent::Duration(120.0)));
            app.on_msg(Msg::Player(PlayerEvent::Buffering(initially_buffering)));
            app.on_msg(Msg::Player(PlayerEvent::Buffering(false)));
            app.on_msg(Msg::Player(PlayerEvent::TimePos(0.5)));
            assert_eq!(app.errors_in_row, 0);
            assert!(app.take_tasks().iter().any(|task| matches!(task, Task::Preload { idx: 1, .. })));
        }
    }
    fn playing_episode() -> App {
        let mut app = App::for_test();
        app.play_list("Podcast", PlayContext::default(), vec![episode("e", 240.0)], 0);
        let generation = app.generation;
        app.take_player();
        app.on_msg(Msg::Url { generation, result: Ok("audio".into()) });
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Duration(600.0)));
        app.on_msg(Msg::Player(PlayerEvent::Buffering(false)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(241.0)));
        app.take_tasks();
        app
    }

    #[test]
    fn unfinished_seek_checkpoints_the_last_confirmed_position() {
        let mut app = playing_episode();
        app.on_msg(Msg::Player(PlayerEvent::Seeking(true)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(500.0)));
        app.on_msg(Msg::Player(PlayerEvent::Pause(true)));
        assert_eq!(app.pending_podcast_progress().items[0].position, 241.0);
        app.capture_podcast_progress();
        assert_eq!(app.pending_podcast_progress().items[0].position, 241.0);
        app.on_msg(Msg::Player(PlayerEvent::Seeking(false)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(500.0)));
        app.capture_podcast_progress();
        assert_eq!(app.pending_podcast_progress().items[0].position, 500.0);
    }

    #[test]
    fn eof_updates_the_queue_and_its_original_order_before_restart() {
        let mut app = playing_episode();
        app.queue.keep_own_order(app.queue.tracks().to_vec());
        app.on_msg(Msg::Player(PlayerEvent::Eof));
        app.take_player();
        app.next(End::Skip);
        assert_eq!(app.time_pos, 0.0, "completed episode restarts from zero");
        assert!(app.queue.set_shuffled(false));
        assert!(app.current().unwrap().progress.as_ref().unwrap().finished);
    }

    #[test]
    fn carried_podcast_stops_at_eof_without_music_repeat_or_wave_continuation() {
        for source in [Source::List("next".into()), Source::Wave] {
            let mut app = playing_episode();
            app.queue.set_repeat(Repeat::One);
            let music = Track::from_json(&json!({"id":"music", "title":"Music", "type":"music"})).unwrap();
            app.queue.after_current(source, vec![music]);
            app.on_msg(Msg::Player(PlayerEvent::Eof));
            assert!(app.current().is_none());
            assert_eq!(app.queue.repeat(), Repeat::One);
            assert!(!app.take_tasks().iter().any(|t| matches!(t, Task::Preload { .. } | Task::WaveMore { .. })));
        }
    }

    #[test]
    fn successful_favorite_toggle_can_reverse_before_refresh_returns() {
        let mut app = App::for_test();
        app.podcasts.favorites_ready = true;
        app.set_podcast_favorite(show("s"));
        app.take_tasks();
        app.on_msg(Msg::Podcasts(PodcastMsg::Favorite { show: show("s"), like: true, result: Ok(()) }));
        app.set_podcast_favorite(show("s"));
        assert!(app.take_tasks().iter().any(|t| matches!(
            t, Task::Podcast(PodcastTask::Favorite { show, like: false }) if show.id == "s"
        )));
    }

    #[test]
    fn linked_episode_reconciles_pending_progress_before_server_resume() {
        let mut app = playing_episode();
        let intent = app.new_intent();
        app.on_msg(Msg::Podcasts(PodcastMsg::Linked { intent, id: "e".into() }));
        assert!(app.take_tasks().iter().any(|t| matches!(
            t, Task::Podcast(PodcastTask::Play { checkpoint: Some(report), .. })
                if report.track_id == "e" && report.position == 241.0
        )));
    }

    #[test]
    fn enabled_lastfm_excludes_podcasts_but_still_reports_music() {
        let mut app = playing_episode();
        app.lastfm = Lastfm::On(LastfmSession { name: "test".into(), key: "test-session".into() });
        for position in 242..487 {
            app.on_msg(Msg::Player(PlayerEvent::TimePos(position as f64)));
        }
        assert!(app.play.as_ref().unwrap().report.played >= 240.0);
        assert!(!app.take_tasks().iter().any(|task| matches!(
            task, Task::LastfmNowPlaying { .. } | Task::LastfmScrobble { .. }
        )));
        let music = Track::from_json(&json!({
            "id":"music", "type":"music", "title":"Music", "durationMs":600000,
            "artists":[{"id":"artist", "name":"Artist"}],
        })).unwrap();
        app.play_list("Music", PlayContext::default(), vec![music], 0);
        let generation = app.generation;
        app.take_player();
        app.on_msg(Msg::Url { generation, result: Ok("audio".into()) });
        app.take_player();
        app.on_msg(Msg::Player(PlayerEvent::Duration(600.0)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(1.0)));
        assert!(app.take_tasks().iter().any(|task| matches!(
            task, Task::LastfmNowPlaying { track, .. } if track.track == "Music"
        )));
    }

    #[test]
    fn episode_likes_target_selection_without_entering_the_music_collection() {
        let mut app = playing_episode();
        app.tab = Tab::Podcasts;
        app.queue.set_source(Source::Wave);
        let selected = episode("selected", 0.0);
        app.podcasts.open = Some(OpenPodcast {
            show: show("show"), tracks: vec![selected.clone()], loading: false, total: 1, req: 0, pages: 1,
        });
        app.podcasts.episode_state.select(Some(0));
        app.act(Action::LikeSelected);
        assert!(app.is_liked(&selected));
        assert!(!app.is_liked(app.current().unwrap()), "L must not rate the playing episode");
        assert!(app.likes.tracks().is_empty(), "podcast must not enter the music list");
        let tasks = app.take_tasks();
        let [Task::SetLike { track, like: true, req }] = tasks.as_slice() else { panic!("{tasks:?}") };
        assert_eq!(track.id, selected.id);
        let selected_req = *req;
        app.act(Action::LikeCurrent);
        assert!(app.is_liked(app.current().unwrap()));
        let tasks = app.take_tasks();
        assert!(!tasks.iter().any(|t| matches!(t, Task::WaveFeedback { .. })));
        assert!(tasks.iter().any(|t| matches!(t, Task::SetLike { track, like: true, .. } if track.id == "e")));
        app.on_msg(Msg::LikeSet {
            id: selected.id.clone(), req: selected_req,
            result: Err(ApiError::from_anyhow(&anyhow::anyhow!("write failed"))),
        });
        assert!(!app.is_liked(&selected), "failed write restores the selected episode's mark");
        assert!(app.is_liked(app.current().unwrap()), "other episode's pending like is unchanged");
        assert!(app.likes.tracks().is_empty());
        assert!(app.take_player().is_empty(), "liking must not start or move playback");
    }

    #[test]
    fn episode_like_survives_reload_and_a_pending_unlike_overlays_old_server_ids() {
        let mut app = playing_episode();
        let track = app.current().unwrap().clone();
        app.on_msg(Msg::LikedIds(vec![track.full_id()]));
        app.on_msg(Msg::Liked(Ok(vec![track.clone()])));
        assert!(app.is_liked(&track));
        assert!(app.likes.tracks().is_empty());
        app.tab = Tab::Podcasts;
        app.act(Action::LikeCurrent);
        assert!(!app.is_liked(&track));
        let req = app.like_edits[&track.id].req;
        app.load_liked();
        app.on_msg(Msg::LikedIds(vec![track.full_id()]));
        app.on_msg(Msg::Liked(Ok(vec![track.clone()])));
        assert!(!app.is_liked(&track), "stale reload cannot undo a pending unlike");
        app.on_msg(Msg::LikeSet { id: track.id.clone(), req, result: Ok(()) });
        assert!(!app.is_liked(&track));
        assert!(app.likes.tracks().is_empty());
    }

}
