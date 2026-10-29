//! Last.fm authorization, bounded durable outbox and submission acknowledgements.
use super::*;

pub const MAX_PENDING: usize = 4096;
const RETRY: Duration = Duration::from_secs(60);

/// Only account name and pending tracks; no session secret is duplicated here.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingScrobbles {
    pub account: String,
    pub tracks: Vec<Scrobble>,
}

#[derive(Default)]
pub struct ScrobbleQueue {
    pending: PendingScrobbles,
    flight: Option<(u64, usize)>,
    req: u64,
    revision: u64,
    retry_at: Option<Instant>,
}

impl ScrobbleQueue {
    pub fn restore(&mut self, mut pending: PendingScrobbles) {
        pending.tracks.truncate(MAX_PENDING);
        self.pending = pending;
        self.flight = None;
        self.revision += 1;
    }
    pub fn snapshot(&self) -> PendingScrobbles {
        self.pending.clone()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub(super) fn in_flight(&self) -> Option<u64> {
        self.flight.map(|(req, _)| req)
    }
    fn for_account(&mut self, account: &str) {
        if self.pending.account != account {
            self.clear();
            self.pending.account = account.to_string();
        }
    }
    pub(super) fn clear(&mut self) {
        self.pending = PendingScrobbles::default();
        self.flight = None;
        self.retry_at = None;
        self.revision += 1;
    }
    fn push(&mut self, item: Scrobble) -> bool {
        if self.pending.tracks.len() >= MAX_PENDING {
            return false;
        }
        self.pending.tracks.push(item);
        self.revision += 1;
        true
    }
    fn batch(&mut self, now: Instant, new_track: bool) -> Option<(u64, Vec<Scrobble>)> {
        if self.flight.is_some()
            || self.pending.tracks.is_empty()
            || (!new_track && self.retry_at.is_some_and(|at| now < at))
        {
            return None;
        }
        self.req += 1;
        let count = self.pending.tracks.len().min(lastfm::MAX_BATCH);
        self.flight = Some((self.req, count));
        Some((self.req, self.pending.tracks[..count].to_vec()))
    }
    fn finish(&mut self, remove: bool, now: Instant) {
        let Some((_, count)) = self.flight.take() else { return };
        if remove {
            self.pending.tracks.drain(..count);
            self.revision += 1;
        }
        self.retry_at = Some(now + RETRY);
    }
}

impl App {
    pub fn restore_scrobbles(&mut self, pending: PendingScrobbles) {
        self.scrobbles.restore(pending);
        // Keep an outbox belonging to a revoked session until that account reconnects.
        if let Lastfm::On(s) = &self.lastfm {
            self.scrobbles.for_account(&s.name);
        }
    }
    pub fn pending_scrobbles(&self) -> PendingScrobbles {
        self.scrobbles.snapshot()
    }
    pub fn scrobbles_revision(&self) -> u64 {
        self.scrobbles.revision()
    }

    /// Last.fm learns the track plays once it does, and gets a scrobble once it is listened long
    /// enough (see `lastfm::scrobble_after`).
    pub(super) fn lastfm_play(&mut self) {
        let Lastfm::On(s) = &self.lastfm else { return };
        let session = s.key.clone();
        self.scrobbles.for_account(&s.name);
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
            if !self.scrobbles.push(item) {
                self.set_error("Last.fm: очередь из 4096 скробблов заполнена, новый скроббл не сохранён");
            }
            self.send_scrobbles(true);
        }
    }

    /// Starts connecting Last.fm: a token is asked for, then confirmed by the user in the browser.
    pub(super) fn connect_lastfm(&mut self) {
        let now = Instant::now();
        self.lastfm_auth += 1;
        self.lastfm = Lastfm::Connecting {
            req: self.lastfm_auth,
            token: None,
            until: now + LASTFM_CONFIRM,
            poll_at: now,
            polling: false,
        };
        self.set_status("Last.fm: подключение…");
        self.task(Task::LastfmToken { req: self.lastfm_auth });
    }

    /// While connecting: asks now and then whether the access has been confirmed.
    pub(super) fn poll_lastfm(&mut self) {
        let now = Instant::now();
        self.send_scrobbles(false);
        let Lastfm::Connecting { token, until, poll_at, polling, .. } = &mut self.lastfm else { return };
        if now >= *until {
            self.lastfm = Lastfm::Off;
            self.set_error("Last.fm: доступ не подтверждён — подключите снова в настройках");
            return;
        }
        if *polling || now < *poll_at {
            return;
        }
        let Some(token) = token.clone() else { return };
        *polling = true;
        self.task(Task::LastfmSession(token));
    }

    pub(super) fn on_lastfm_session(&mut self, token: String, result: Result<(String, String), lastfm::Error>) {
        let Lastfm::Connecting { token: Some(t), polling, poll_at, .. } = &mut self.lastfm else { return };
        if *t != token {
            return;
        }
        match result {
            Ok((name, key)) => {
                self.set_status(format!("Last.fm подключён: {name}"));
                self.scrobbles.for_account(&name);
                self.lastfm = Lastfm::On(LastfmSession { name, key });
                self.send_scrobbles(true);
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

    pub(super) fn on_lastfm_token(&mut self, req: u64, result: Result<String, lastfm::Error>) {
        let Lastfm::Connecting { req: current, token: token @ None, poll_at, .. } = &mut self.lastfm else { return };
        if req != *current {
            return;
        }
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

    pub(super) fn send_scrobbles(&mut self, new_track: bool) {
        let Lastfm::On(s) = &self.lastfm else { return };
        self.scrobbles.for_account(&s.name);
        if let Some((req, tracks)) = self.scrobbles.batch(Instant::now(), new_track) {
            self.task(Task::LastfmScrobble { session: s.key.clone(), req, tracks });
        }
    }

    pub(super) fn on_lastfm_sent(&mut self, session: String, req: Option<u64>, result: Result<usize, lastfm::Error>) {
        if !matches!(&self.lastfm, Lastfm::On(s) if s.key == session) {
            return;
        }
        if let Some(req) = req {
            if self.scrobbles.in_flight() != Some(req) {
                return;
            }
            // Keep a failed batch in the durable queue. A permanent malformed batch is removed
            // with an error, so it cannot prevent later valid scrobbles from being submitted.
            let remove = result.is_ok() || result.as_ref().is_err_and(|e| !e.is_temporary() && !e.is_session());
            self.scrobbles.finish(remove, Instant::now());
        }
        match result {
            Ok(ignored) => {
                if ignored > 0 {
                    self.set_error(format!("Last.fm отклонил скробблы: {ignored}"));
                }
                if req.is_some() {
                    self.send_scrobbles(true);
                }
            }
            Err(e) if e.is_session() => {
                self.scrobbles.flight = None;
                self.lastfm = Lastfm::Off;
                self.set_error("Last.fm: доступ отозван — подключите снова в настройках");
            }
            Err(e) if req.is_some() => self.set_error(format!("Last.fm: {e}")),
            Err(_) => {}
        }
    }

    pub fn lastfm_session(&self) -> Option<&LastfmSession> {
        if let Lastfm::On(s) = &self.lastfm { Some(s) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(n: u64) -> Scrobble {
        Scrobble { artist: "a".into(), track: n.to_string(), album: String::new(), duration: 100, timestamp: n }
    }
    #[test]
    fn full_batch_acknowledges_only_sent_tracks_and_survives_restart() {
        let now = Instant::now();
        let mut q = ScrobbleQueue::default();
        q.for_account("me");
        for n in 0..51 {
            assert!(q.push(item(n)));
        }
        let (req, batch) = q.batch(now, true).unwrap();
        assert_eq!(batch.len(), 50);
        assert!(q.batch(now, true).is_none());
        let snapshot = q.snapshot();
        assert_eq!(snapshot.tracks.len(), 51, "in-flight entries remain durable");
        let mut restarted = ScrobbleQueue::default();
        restarted.restore(snapshot);
        assert_eq!(restarted.batch(now, true).unwrap().1.len(), 50);
        assert_eq!(q.in_flight(), Some(req));
        q.finish(true, now);
        assert_eq!(q.snapshot().tracks, vec![item(50)]);
        assert_eq!(q.batch(now, true).unwrap().1, vec![item(50)]);
        q.finish(false, now);
        assert!(q.batch(now, false).is_none());
        assert_eq!(q.batch(now + RETRY, false).unwrap().1, vec![item(50)]);
        q.for_account("someone-else");
        assert!(q.snapshot().tracks.is_empty());
    }
    #[test]
    fn duplicate_ack_does_not_remove_a_later_batch() {
        let mut app = App::for_test();
        app.lastfm = Lastfm::On(LastfmSession { name: "me".into(), key: "sk".into() });
        app.scrobbles.for_account("me");
        for n in 0..51 {
            app.scrobbles.push(item(n));
        }
        app.send_scrobbles(true);
        let first = app.scrobbles.in_flight().unwrap();
        app.on_lastfm_sent("sk".into(), Some(first), Ok(0));
        let second = app.scrobbles.in_flight().unwrap();
        assert_ne!(first, second);
        app.on_lastfm_sent("sk".into(), Some(first), Ok(0));
        assert_eq!(app.scrobbles.snapshot().tracks, vec![item(50)]);
        app.on_lastfm_sent("sk".into(), Some(second), Ok(0));
        assert!(app.scrobbles.snapshot().tracks.is_empty());
    }
}
