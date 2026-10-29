//! Wave session lifecycle; every continuation belongs to a specific accepted start.
use super::*;

pub struct Wave {
    /// The accepted start intent; unique even when restarting the same station.
    pub id: u64,
    pub station: String,
    pub name: String,
    pub session: Option<String>,
    pub batch: String,
    pub fetching: bool,
    pub pending_next: bool,
}

impl App {
    pub(super) fn start_wave(&mut self, station: Station) {
        let intent = self.new_intent();
        let name = station.name.clone();
        self.set_status(format!("Запуск: {name}…"));
        let settings = station.settings;
        let id = station.id.clone();
        self.task(Task::WaveStart { intent, station: id, name, settings });
    }

    pub(super) fn fetch_more_wave(&mut self) {
        let Some(w) = &mut self.wave else { return };
        if w.fetching {
            return;
        }
        w.fetching = true;
        let (wave, st, sess) = (w.id, w.station.clone(), w.session.clone());
        let recent: Vec<String> = self.queue.tracks().iter().rev().take(10).rev().map(Track::full_id).collect();
        self.task(Task::WaveMore { wave, station: st, session: sess, recent });
    }

    pub(super) fn on_wave_started(
        &mut self,
        intent: u64,
        station: String,
        name: String,
        result: Result<WaveBatch, ApiError>,
    ) {
        if intent != self.intent {
            return;
        }
        match result {
            Ok(b) => {
                let from = station.replace(':', "-");
                let wave = Wave {
                    id: intent,
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
        }
    }
    pub(super) fn on_wave_more(&mut self, wave: u64, result: Result<WaveBatch, ApiError>) {
        let Some(w) = self.wave.as_mut().filter(|w| w.id == wave && w.fetching) else { return };
        w.fetching = false;
        match result {
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
}
