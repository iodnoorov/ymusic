use super::*;
use crate::ynison::{Desired, Event, Restored};

#[derive(Default)]
pub struct Sync {
    pub enabled: bool,
    pub autoplay: bool,
    pub epoch: u64,
    pub owned: bool,
    pub claim: u64,
    pub restore_allowed: bool,
    pub restored_position: Option<f64>,
    template: Option<Value>,
}
impl App {
    pub fn set_ynison(&mut self, enabled: bool) {
        self.ynison.enabled = enabled;
        self.ynison.epoch = self.ynison.epoch.wrapping_add(1);
        self.ynison.restore_allowed = enabled && self.queue.is_empty();
        self.ynison.owned = enabled && self.current().is_some() && !self.stopped;
        self.ynison.claim = self.ynison.claim.wrapping_add(1);
    }
    pub(super) fn toggle_playback(&mut self) {
        if let Some(position) = self.ynison.restored_position.take() {
            if let Some(i) = self.queue.pos() {
                self.play_index(i);
                self.resume_at = Some(position);
                self.time_pos = position;
            }
        } else {
            if self.paused && !self.ynison.owned {
                self.ynison.claim = self.ynison.claim.wrapping_add(1);
                self.ynison.owned = true;
            }
            self.play(PlayerCmd::TogglePause);
        }
    }
    pub(super) fn on_ynison(&mut self, epoch: u64, event: Event) {
        if !self.ynison.enabled || epoch != self.ynison.epoch {
            return;
        }
        match event {
            Event::Error => {
                self.set_error("Ynison: нет соединения; воспроизведение доступно локально")
            }
            Event::Relinquished { claim } => {
                if !self.ynison.owned || self.ynison.claim != claim {
                    return;
                }
                self.ynison.owned = false;
                if self.loading_track || self.player_behind > 0 {
                    let position = self
                        .confirmed_time_pos
                        .or(self.resume_at)
                        .unwrap_or(self.time_pos);
                    self.generation += 1;
                    self.play(PlayerCmd::Stop);
                    self.loading_track = false;
                    self.awaiting_start = false;
                    self.load_deadline = None;
                    self.retry_at = None;
                    self.preload = None;
                    self.preload_pending = false;
                    self.stopped = true;
                    self.paused = true;
                    self.ynison.restored_position = Some(position);
                } else if !self.paused && !self.stopped {
                    self.play(PlayerCmd::TogglePause);
                    self.paused = true;
                    self.pause_play();
                }
                self.set_status("Воспроизведение продолжено на другом устройстве");
            }
            Event::Restore(r) if self.ynison.restore_allowed => self.restore_ynison(r),
            Event::Restore(_) => {}
        }
    }
    fn restore_ynison(&mut self, mut r: Restored) {
        self.ynison.restore_allowed = false;
        if r.index >= r.tracks.len() {
            return;
        }
        let kind = r.queue["entity_type"].as_str().unwrap_or("VARIOUS");
        let item = r.queue["entity_id"].as_str().unwrap_or("").to_owned();
        let context = match kind {
            "RADIO" => "radio",
            "PLAYLIST" => "playlist",
            "ALBUM" => "album",
            "ARTIST" => "artist",
            _ => "various",
        };
        self.context = PlayContext::new(context, &item);
        let mut own_order = None;
        if kind != "RADIO"
            && let Some(indices) = r.queue["shuffle_optional"]["playable_indices"].as_array()
        {
            let order: Option<Vec<usize>> = indices
                .iter()
                .map(|i| {
                    i.as_u64()
                        .or_else(|| i.as_str()?.parse().ok())
                        .and_then(|i| usize::try_from(i).ok())
                })
                .collect();
            if let Some(order) = order.filter(|o| {
                o.len() == r.tracks.len()
                    && o.iter().copied().collect::<HashSet<_>>().len() == r.tracks.len()
                    && o.iter().all(|&i| i < r.tracks.len())
            }) {
                r.index = order.iter().position(|&i| i == r.index).unwrap();
                let original = r.tracks;
                r.tracks = order.iter().map(|&i| original[i].clone()).collect();
                own_order = Some(original);
            }
        }

        // A restored wave keeps its radio session and can request the next batch.
        let source = if kind == "RADIO" && !item.is_empty() {
            self.wave = Some(Wave {
                id: self.intent,
                station: item.clone(),
                name: if item == MY_WAVE {
                    "Моя волна".into()
                } else {
                    item.clone()
                },
                session: r.queue["adding_options_optional"]["radio_options"]["session_id"]
                    .as_str()
                    .map(str::to_owned),
                batch: r.tracks[r.index].batch.clone().unwrap_or_default(),
                fetching: false,
                pending_next: false,
            });
            Source::Wave
        } else {
            self.wave = None;
            Source::List("Из Ynison".into())
        };
        self.queue.replace(source, r.tracks);
        if let Some(own) = own_order {
            self.queue.keep_own_order(own);
        }
        self.queue
            .set_repeat(match r.queue["options"]["repeat_mode"].as_str() {
                Some("ONE") => Repeat::One,
                Some("ALL") => Repeat::All,
                _ => Repeat::Off,
            });
        self.queue.select(r.index);
        self.queue_state.select(Some(r.index));
        self.search.feeds_queue = false;
        self.ynison.template = Some(r.queue);
        let track = self.current().unwrap();
        let duration = track.duration_ms as f64 / 1000.0;
        let cover = track.cover.clone();
        self.duration = duration;
        let position = r.position.max(0.0).min(self.duration);
        self.time_pos = position;
        self.confirmed_time_pos = Some(position);
        self.paused = true;
        self.stopped = true;
        self.ynison.restored_position = Some(position);
        self.load_cover(cover);
        self.set_status("Очередь восстановлена из Ynison");
        if self.ynison.autoplay && self.current().is_some_and(|t| t.available) {
            self.toggle_playback();
        }
    }
    pub fn ynison_desired(&self) -> Desired {
        let player = (self.ynison.owned && self.ynison.enabled).then(|| {
            let (entity_type, entity_id) = if let Some(w) = &self.wave { ("RADIO",w.station.as_str()) } else {
                (match self.context.kind { "playlist" => "PLAYLIST", "album" => "ALBUM", "artist" => "ARTIST", _ => "VARIOUS" }, self.context.item.as_str())
            };
            let mut queue = json!({"entity_type":entity_type,"entity_id":entity_id,"entity_context":"BASED_ON_ENTITY_BY_DEFAULT",
                "current_playable_index":self.queue.pos().map(|i| i as i64).unwrap_or(-1),
                "playable_list":self.queue.tracks().iter().map(|t| json!({"playable_id":t.id,"album_id_optional":t.album_id,"playable_type":"TRACK","title":t.title,"cover_url_optional":t.cover,"from":"ymusic","track_info":{"batch_id_optional":t.batch,"track_source_key":0}})).collect::<Vec<_>>(),
                "options":{"repeat_mode":match self.queue.repeat() { Repeat::Off => "NONE", Repeat::All => "ALL", Repeat::One => "ONE" }}});
            if let Some(w) = &self.wave {
                if let Some(session) = &w.session { queue["adding_options_optional"] = json!({"radio_options":{"session_id":session}}); }
                if let Some(template) = &self.ynison.template {
                    // Keep wave navigation metadata only while using the restored radio session.
                    if template["adding_options_optional"] == queue["adding_options_optional"] {
                        queue["queue"] = template["queue"].clone();
                        if queue["queue"]["wave_queue"].is_object() {
                            queue["queue"]["wave_queue"]["live_playable_index"] = queue["current_playable_index"].clone();
                        }
                    }
                }
            }
            let position = if self.seeking || self.buffering || self.loading_track {
                self.confirmed_time_pos.or(self.resume_at).unwrap_or(0.0)
            } else { self.time_pos };
            json!({"player_queue":queue,"status":{"progress_ms":(position.max(0.0)*1000.0) as u64,"duration_ms":(self.duration.max(0.0)*1000.0) as u64,"paused":self.paused || self.stopped,"playback_speed":1}})
        });
        Desired {
            enabled: self.ynison.enabled,
            epoch: self.ynison.epoch,
            claim: self.ynison.claim,
            player,
            volume: self.save_volume() as f64 / 100.0,
            shutdown: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn restored() -> Restored {
        let tracks = ["a", "b"]
            .into_iter()
            .map(|id| Track {
                id: id.into(),
                album_id: None,
                title: id.into(),
                artists: String::new(),
                main_artist: None,
                album: String::new(),
                duration_ms: 120000,
                available: true,
                cover: None,
                batch: None,
                kind: crate::api::TrackKind::Music,
                progress: None,
                published: None,
            })
            .collect();
        Restored {
            queue: json!({"entity_type":"PLAYLIST","entity_id":"1:3","options":{"repeat_mode":"ALL"}}),
            tracks,
            index: 1,
            position: 42.5,
        }
    }
    fn deliver(app: &mut App) {
        app.on_msg(Msg::Ynison {
            epoch: app.ynison.epoch,
            event: Event::Restore(restored()),
        });
    }
    #[test]
    fn restore_waits_for_play_and_resumes_from_position() {
        let mut a = App::for_test();
        a.set_ynison(true);
        deliver(&mut a);
        assert_eq!(a.queue.len(), 2);
        assert_eq!(a.current().unwrap().id, "b");
        assert_eq!(a.queue.repeat(), Repeat::All);
        assert_eq!(a.time_pos, 42.5);
        assert!(a.paused && a.stopped);
        assert!(a.take_player().is_empty());
        assert!(
            a.ynison_desired().player.is_none(),
            "passive restoration must not overwrite the phone"
        );
        a.toggle_playback();
        assert_eq!(a.resume_at, Some(42.5));
        assert!(a.ynison.owned && !a.stopped);
        a.on_msg(Msg::Url {
            generation: a.generation,
            result: Ok(crate::api::FileInfo {
                url: "https://test".into(),
                ..Default::default()
            }),
        });
        let effects = a.take_effects();
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Player(PlayerCmd::Load(t)) if t.start == 42.5))
        );
    }
    #[test]
    fn autoplay_and_stale_restoration() {
        let mut a = App::for_test();
        a.ynison.autoplay = true;
        a.set_ynison(true);
        deliver(&mut a);
        assert!(a.ynison.owned && !a.stopped);
        assert_eq!(a.resume_at, Some(42.5));
        let mut a = App::for_test();
        a.set_ynison(true);
        a.new_intent();
        deliver(&mut a);
        assert!(
            a.queue.is_empty(),
            "a local selection wins over late startup state"
        );
        a.set_ynison(false);
        a.set_ynison(true);
        a.on_msg(Msg::Ynison {
            epoch: a.ynison.epoch - 2,
            event: Event::Restore(restored()),
        });
        assert!(
            a.queue.is_empty(),
            "a disabled session cannot restore later"
        );
    }
    #[test]
    fn handoff_pauses_and_requires_a_new_claim() {
        let mut a = App::for_test();
        a.ynison.autoplay = true;
        a.set_ynison(true);
        deliver(&mut a);
        a.paused = false;
        a.loading_track = false;
        a.take_player();
        let claim = a.ynison.claim;
        a.on_ynison(a.ynison.epoch, Event::Relinquished { claim });
        assert!(a.paused && !a.ynison.owned);
        assert!(a.ynison_desired().player.is_none());
        assert_eq!(a.take_player(), ["pause"]);
        a.toggle_playback();
        assert!(a.ynison.owned && a.ynison.claim > claim);
        a.on_ynison(a.ynison.epoch, Event::Relinquished { claim });
        assert!(
            a.ynison.owned,
            "old handoff cannot cancel the new play command"
        );
    }
    #[test]
    fn autoplay_depends_on_ynison() {
        let mut a = App::for_test();
        a.change_setting(Setting::Autoplay, 1);
        assert!(!a.ynison.autoplay);
        a.change_setting(Setting::Ynison, 1);
        a.change_setting(Setting::Autoplay, 1);
        assert!(a.ynison.autoplay);
        a.change_setting(Setting::Ynison, 1);
        assert_eq!(a.setting_value(Setting::Autoplay).as_deref(), Some("выкл"));
        assert!(!a.ynison_desired().enabled);
    }
    #[test]
    fn restored_shuffle_keeps_current_track_and_original_order() {
        let mut a = App::for_test();
        a.set_ynison(true);
        let mut r = restored();
        r.queue["shuffle_optional"] = json!({"playable_indices":[1,0]});
        a.on_ynison(a.ynison.epoch, Event::Restore(r));
        assert!(a.queue.shuffled());
        assert_eq!(a.queue.pos(), Some(0));
        assert_eq!(a.current().unwrap().id, "b");
        a.queue.set_shuffled(false);
        assert_eq!(a.queue.pos(), Some(1));
        assert_eq!(a.current().unwrap().id, "b");
    }
    #[test]
    fn takeover_cancels_a_pending_load() {
        let mut a = App::for_test();
        a.ynison.autoplay = true;
        a.set_ynison(true);
        deliver(&mut a);
        let old = a.generation;
        a.on_ynison(
            a.ynison.epoch,
            Event::Relinquished {
                claim: a.ynison.claim,
            },
        );
        assert!(a.stopped && a.paused && !a.ynison.owned);
        a.take_player();
        a.on_msg(Msg::Url {
            generation: old,
            result: Ok(crate::api::FileInfo {
                url: "https://stale".into(),
                ..Default::default()
            }),
        });
        assert!(a.take_player().is_empty());
        assert_eq!(a.ynison.restored_position, Some(42.5));
    }
}
