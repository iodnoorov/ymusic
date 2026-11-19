use super::*;
use crate::ynison::{Command, Control, Desired, Device, Event, Restored};

#[derive(Default)]
pub struct Sync {
    pub enabled: bool,
    pub autoplay: bool,
    pub epoch: u64,
    pub owned: bool,
    pub claim: u64,
    pub restore_allowed: bool,
    pub restored_position: Option<f64>,
    pub takeover: Option<u64>,
    takeover_id: u64,
    pub phones: Vec<Device>,
    pub active: Option<String>,
    pub local: String,
    pub commands: Vec<Command>,
    template: Option<Value>,
}
impl App {
    pub fn set_ynison(&mut self, enabled: bool) {
        self.ynison.takeover = None;
        self.ynison.phones.clear();
        self.ynison.active = None;
        self.ynison.commands.clear();
        self.ynison.enabled = enabled;
        self.ynison.epoch = self.ynison.epoch.wrapping_add(1);
        self.ynison.restore_allowed = enabled && self.queue.is_empty();
        self.ynison.owned = enabled && self.current().is_some() && !self.stopped;
        self.ynison.claim = self.ynison.claim.wrapping_add(1);
    }
    pub(super) fn open_ynison_devices(&mut self) {
        if self.ynison.enabled {
            let sel = self
                .ynison
                .phones
                .iter()
                .position(|p| self.ynison.active.as_ref() == Some(&p.id))
                .map_or(0, |index| index + 1);
            self.modal = Some(Modal::YnisonDevices { sel });
        } else {
            self.set_status("Включите Ynison в настройках");
        }
    }

    pub(super) fn on_ynison_device_key(&mut self, mut sel: usize, code: KeyCode) {
        let len = self.ynison.phones.len();
        sel = sel.min(len);
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('V') => return,
            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(len),
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            KeyCode::Enter if sel == 0 => self.take_over_ynison(),
            _ => {
                let control = match (code, keymap::action(code)) {
                    (KeyCode::Enter, _) => Some(Control::Transfer),
                    (_, Some(Action::PlayPause)) => Some(Control::Pause),
                    (_, Some(Action::Next)) => Some(Control::Track(1)),
                    (_, Some(Action::Prev)) => Some(Control::Track(-1)),
                    (_, Some(Action::SeekBack)) => Some(Control::Seek(-10_000)),
                    (_, Some(Action::SeekForward)) => Some(Control::Seek(10_000)),
                    (_, Some(Action::VolumeDown)) => Some(Control::Volume(-0.05)),
                    (_, Some(Action::VolumeUp)) => Some(Control::Volume(0.05)),
                    (_, Some(Action::Repeat)) => Some(Control::Repeat),
                    (_, Some(Action::Shuffle)) => Some(Control::Shuffle),
                    _ => None,
                };
                if let Some(control) = control
                    && let Some(phone) = sel.checked_sub(1).and_then(|i| self.ynison.phones.get(i))
                {
                    self.ynison.commands.push(Command {
                        epoch: self.ynison.epoch,
                        target: phone.id.clone(),
                        control,
                        created: Instant::now(),
                    });
                }
            }
        }
        self.modal = Some(Modal::YnisonDevices { sel });
    }

    pub(super) fn take_over_ynison(&mut self) {
        if !self.ynison.enabled || self.ynison.owned {
            return;
        }
        self.ynison.takeover_id = self.ynison.takeover_id.wrapping_add(1);
        self.ynison.takeover = Some(self.ynison.takeover_id);
        self.set_status("Получаем текущее воспроизведение из Ynison");
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
                self.ynison.takeover = None;
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
            Event::Devices {
                phones,
                active,
                local,
            } => {
                if let Some(Modal::YnisonDevices { sel }) = self.modal.as_mut() {
                    *sel = sel
                        .checked_sub(1)
                        .and_then(|i| self.ynison.phones.get(i))
                        .and_then(|previous| phones.iter().position(|p| p.id == previous.id))
                        .map_or(0, |i| i + 1);
                }
                self.ynison.phones = phones;
                self.ynison.active = active;
                self.ynison.local = local;
            }
            Event::ControlUnavailable => {
                self.set_status("Команда Ynison недоступна для выбранного устройства")
            }

            Event::RemotePlayback {
                claim,
                track_id,
                paused,
                position,
            } if self.ynison.owned
                && self.ynison.claim == claim
                && self.current().is_some_and(|track| track.id == track_id)
                && (self.stopped || !self.awaiting_start) =>
            {
                if self.stopped && self.ynison.restored_position.is_some() && !paused {
                    self.ynison.restored_position = Some(position.min(self.duration));
                    self.toggle_playback();
                } else if !self.stopped {
                    self.apply_ynison_playback(paused, position);
                }
            }
            Event::RemoteQueue {
                claim,
                track_ids,
                shuffle,
                index,
                repeat,
                position,
                paused,
            } if self.ynison.owned
                && self.ynison.claim == claim
                && self
                    .queue
                    .original_tracks()
                    .iter()
                    .map(|t| &t.id)
                    .eq(&track_ids) =>
            {
                let index = if self.queue.is_list() {
                    if self.queue.shuffle_indices() != shuffle
                        && self.queue.apply_shuffle_order(shuffle.as_deref())
                    {
                        self.queue_state.select(self.queue.pos());
                        self.following_changed();
                    }
                    shuffle
                        .as_ref()
                        .and_then(|indices| indices.iter().position(|&i| i == index))
                        .unwrap_or(index)
                } else {
                    index
                };
                if self.queue.repeat() != repeat {
                    self.set_repeat(repeat);
                }
                if self.queue.pos() == Some(index) && !self.awaiting_start && !self.stopped {
                    self.apply_ynison_playback(paused, position);
                } else if self.queue.tracks().get(index).is_some_and(|t| t.available)
                    && (self.queue.pos() != Some(index) || self.stopped)
                {
                    if paused {
                        self.park_ynison_track(index, position);
                    } else {
                        self.play_index(index);
                        self.time_pos = position.min(self.duration);
                        self.resume_at = Some(self.time_pos);
                    }
                }
            }
            Event::RemoteVolume(volume) => {
                self.on_remote(crate::media::Remote::Volume(volume * 100.0));
            }
            Event::RemoteReplacement {
                claim,
                restored,
                paused,
            } if self.ynison.owned && self.ynison.claim == claim => {
                self.stop_ynison_track();
                self.restore_ynison(restored, !paused);
            }
            Event::RemotePlayback { .. }
            | Event::RemoteQueue { .. }
            | Event::RemoteReplacement { .. } => {}
            Event::Error => {
                self.ynison.phones.clear();
                self.ynison.active = None;
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
            Event::Takeover { request, restored } if self.ynison.takeover == Some(request) => {
                self.ynison.takeover = None;
                if let Some(restored) = restored.filter(|r| r.tracks[r.index].available) {
                    self.stop_ynison_track();
                    self.restore_ynison(restored, true);
                } else {
                    self.set_error("Ynison: нет доступного трека на другом устройстве");
                }
            }
            Event::Takeover { .. } => {}
            Event::SelectedLocally { claim, restored }
                if !self.ynison.owned
                    && self.ynison.claim == claim
                    && self.ynison.takeover.is_none() =>
            {
                // The source can report paused when it hands off its audio output.
                self.stop_ynison_track();
                self.restore_ynison(restored, true);
            }
            Event::SelectedLocally { .. } => {}
            Event::Restore(r) if self.ynison.restore_allowed && self.ynison.takeover.is_none() => {
                self.restore_ynison(r, self.ynison.autoplay);
            }
            Event::Restore(_) => {}
        }
    }
    fn stop_ynison_track(&mut self) {
        if !self.stopped {
            self.end_play(Some("skip"));
            self.generation += 1;
            self.play(PlayerCmd::Stop);
        }
        self.loading_track = false;
        self.awaiting_start = false;
        self.load_deadline = None;
        self.retry_at = None;
        self.preload = None;
        self.preload_pending = false;
        self.audio_info = None;
        self.seeking = false;
        self.buffering = false;
    }

    fn park_ynison_track(&mut self, index: usize, position: f64) {
        self.stop_ynison_track();
        self.queue.select(index);
        self.queue_state.select(Some(index));
        let track = &self.queue.tracks()[index];
        self.duration = track.duration_ms as f64 / 1000.0;
        self.time_pos = position.min(self.duration);
        self.confirmed_time_pos = Some(self.time_pos);
        self.ynison.restored_position = Some(self.time_pos);
        self.stopped = true;
        self.paused = true;
        self.load_cover(track.cover.clone());
    }

    fn apply_ynison_playback(&mut self, paused: bool, position: f64) {
        use crate::media::Remote;
        if paused != self.paused {
            // A pause/resume includes the controller's last sampled position. Seeking there
            // would discard buffered audio and can start a new range request while paused.
            self.on_remote(if paused { Remote::Pause } else { Remote::Play });
        } else if !self.loading_track && !self.seeking && (self.time_pos - position).abs() >= 0.5 {
            self.on_remote(Remote::SeekTo(position.clamp(0.0, self.duration)));
        }
    }
    fn restore_ynison(&mut self, mut r: Restored, play: bool) {
        self.ynison.restore_allowed = false;
        if r.index >= r.tracks.len() {
            return;
        }
        self.intent = self.intent.wrapping_add(1);
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
            && let Some(order) = crate::ynison::order(&r.queue, r.tracks.len())
        {
            r.index = order.iter().position(|&i| i == r.index).unwrap();
            let original = r.tracks;
            r.tracks = order.iter().map(|&i| original[i].clone()).collect();
            own_order = Some(original);
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
        if play && self.current().is_some_and(|t| t.available) {
            self.toggle_playback();
        }
    }
    pub fn ynison_desired(&self) -> Desired {
        let player = (self.ynison.owned && self.ynison.enabled).then(|| {
            let (entity_type, entity_id) = if let Some(w) = &self.wave { ("RADIO",w.station.as_str()) } else {
                (match self.context.kind { "playlist" => "PLAYLIST", "album" => "ALBUM", "artist" => "ARTIST", _ => "VARIOUS" }, self.context.item.as_str())
            };
            let shuffle = self.queue.shuffle_indices();
            let index = self.queue.pos().map(|i| shuffle.as_ref().map_or(i, |indices| indices[i]));
            let mut queue = json!({"entity_type":entity_type,"entity_id":entity_id,"entity_context":"BASED_ON_ENTITY_BY_DEFAULT",
                "current_playable_index":index.map(|i| i as i64).unwrap_or(-1),
                "playable_list":self.queue.original_tracks().iter().map(|t| json!({"playable_id":t.id,"album_id_optional":t.album_id,"playable_type":"TRACK","title":t.title,"cover_url_optional":t.cover,"from":"ymusic","track_info":{"batch_id_optional":t.batch,"track_source_key":0}})).collect::<Vec<_>>(),
                "options":{"repeat_mode":match self.queue.repeat() { Repeat::Off => "NONE", Repeat::All => "ALL", Repeat::One => "ONE" }}});
            if let Some(indices) = shuffle { queue["shuffle_optional"] = json!({"playable_indices":indices}); }
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
                self.confirmed_time_pos.or(self.resume_at).unwrap_or(self.time_pos)
            } else { self.time_pos };
            json!({"player_queue":queue,"status":{"progress_ms":(position.max(0.0)*1000.0) as u64,"duration_ms":(self.duration.max(0.0)*1000.0) as u64,"paused":self.paused || self.stopped,"playback_speed":1}})
        });
        Desired {
            enabled: self.ynison.enabled,
            epoch: self.ynison.epoch,
            claim: self.ynison.claim,
            player,
            volume: self.volume.clamp(0.0, 100.0) / 100.0,
            shutdown: false,
            takeover: self.ynison.takeover,
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
    fn remote_shuffle_keeps_the_current_track_and_publishes_the_permutation() {
        let mut app = App::for_test();
        app.ynison.autoplay = true;
        app.set_ynison(true);
        deliver(&mut app);
        app.on_ynison(
            app.ynison.epoch,
            Event::RemoteQueue {
                claim: app.ynison.claim,
                track_ids: vec!["a".into(), "b".into()],
                shuffle: Some(vec![1, 0]),
                index: 1,
                repeat: Repeat::All,
                position: 42.5,
                paused: false,
            },
        );
        assert_eq!(app.current().unwrap().id, "b");
        assert_eq!(app.queue.pos(), Some(0));
        let desired = app.ynison_desired().player.unwrap();
        let queue = &desired["player_queue"];
        assert_eq!(queue["current_playable_index"], 1);
        assert_eq!(queue["playable_list"][0]["playable_id"], "a");
        assert_eq!(queue["shuffle_optional"]["playable_indices"], json!([1, 0]));
    }

    #[test]
    fn explicit_takeover_plays_even_without_autoplay_and_ignores_superseded_results() {
        let mut app = App::for_test();
        app.set_ynison(true);
        deliver(&mut app);
        app.take_over_ynison();
        let stale = app.ynison.takeover.unwrap();
        app.take_over_ynison();
        let current = app.ynison.takeover.unwrap();
        app.on_ynison(
            app.ynison.epoch,
            Event::Takeover {
                request: stale,
                restored: Some(restored()),
            },
        );
        assert!(app.stopped);
        app.on_ynison(
            app.ynison.epoch,
            Event::Takeover {
                request: current,
                restored: Some(restored()),
            },
        );
        assert!(app.ynison.owned && !app.stopped);
        assert_eq!(app.resume_at, Some(42.5));
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
    fn selecting_ymusic_from_phone_starts_the_restored_track() {
        let mut app = App::for_test();
        app.set_ynison(true);
        deliver(&mut app);
        assert!(app.paused && app.stopped);
        app.on_ynison(
            app.ynison.epoch,
            Event::SelectedLocally {
                claim: app.ynison.claim,
                restored: restored(),
            },
        );
        assert!(app.ynison.owned && !app.stopped && app.loading_track);
        assert!(app.take_tasks().iter().any(|task| matches!(
            task,
            Task::TrackUrl { full_id, .. } if full_id == "b"
        )));
        app.on_msg(Msg::Url {
            generation: app.generation,
            result: Ok(crate::api::FileInfo {
                url: "https://test".into(),
                ..Default::default()
            }),
        });
        assert!(app.take_effects().iter().any(|effect| matches!(
            effect,
            Effect::Player(PlayerCmd::Load(track)) if track.start == 42.5
        )));
        assert_eq!(
            app.ynison_desired().player.unwrap()["status"]["progress_ms"],
            42500
        );
    }

    #[test]
    fn phone_can_resume_while_the_last_seek_is_waiting_for_audio() {
        let mut app = App::for_test();
        app.ynison.autoplay = true;
        app.set_ynison(true);
        deliver(&mut app);
        app.on_msg(Msg::Player(PlayerEvent::Cleared));
        app.on_msg(Msg::Player(PlayerEvent::Duration(120.0)));
        app.on_msg(Msg::Player(PlayerEvent::Pause(true)));
        app.on_msg(Msg::Player(PlayerEvent::Seeking(true)));
        assert!(app.paused && app.loading_track && !app.awaiting_start);
        app.take_player();
        app.on_ynison(
            app.ynison.epoch,
            Event::RemotePlayback {
                claim: app.ynison.claim,
                track_id: "b".into(),
                paused: false,
                position: 45.0,
            },
        );
        assert_eq!(app.take_player(), ["pause"]);
    }

    #[test]
    fn phone_pause_does_not_seek_to_its_older_sampled_position() {
        let mut app = App::for_test();
        app.ynison.autoplay = true;
        app.set_ynison(true);
        deliver(&mut app);
        app.on_msg(Msg::Player(PlayerEvent::Cleared));
        app.on_msg(Msg::Player(PlayerEvent::Duration(120.0)));
        app.on_msg(Msg::Player(PlayerEvent::Pause(false)));
        app.on_msg(Msg::Player(PlayerEvent::TimePos(45.0)));
        app.take_player();
        app.on_ynison(
            app.ynison.epoch,
            Event::RemotePlayback {
                claim: app.ynison.claim,
                track_id: "b".into(),
                paused: true,
                position: 42.5,
            },
        );
        assert_eq!(app.take_player(), ["pause"]);
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
