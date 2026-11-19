//! Ynison transport. A single background worker owns the socket; the UI shares only its latest
//! desired state, so a slow connection cannot accumulate a backlog of old positions.
use crate::{
    Lock,
    api::{Api, Track},
    app::Msg,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    net::{TcpStream, ToSocketAddrs},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{
    Message, WebSocket, client::IntoClientRequest, http::HeaderValue, stream::MaybeTlsStream,
};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;
const REDIRECTOR: &str = "ynison.music.yandex.ru";

pub struct Restored {
    pub queue: Value,
    pub tracks: Vec<Track>,
    pub index: usize,
    pub position: f64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub title: String,
    pub volume_steps: u64,
}

#[derive(Clone, Debug)]
pub enum Control {
    Transfer,
    Pause,
    Track(i64),
    Seek(i64),
    Volume(f64),
    Repeat,
    Shuffle,
}

pub struct Command {
    pub epoch: u64,
    pub target: String,
    pub control: Control,
    pub created: Instant,
}

pub enum Event {
    Restore(Restored),
    Devices {
        phones: Vec<Device>,
        active: Option<String>,
        local: String,
    },
    ControlUnavailable,
    RemoteReplacement {
        claim: u64,
        restored: Restored,
        paused: bool,
    },
    SelectedLocally {
        claim: u64,
        restored: Restored,
        paused: bool,
    },
    Takeover {
        request: u64,
        restored: Option<Restored>,
    },
    Relinquished {
        claim: u64,
    },
    RemotePlayback {
        claim: u64,
        track_id: String,
        paused: bool,
        position: f64,
    },
    RemoteQueue {
        claim: u64,
        track_ids: Vec<String>,
        shuffle: Option<Vec<usize>>,
        index: usize,
        repeat: crate::queue::Repeat,
        position: f64,
        paused: bool,
    },
    RemoteVolume(f64),
    Error,
}
#[derive(Clone, Default)]
pub struct Desired {
    pub enabled: bool,
    pub epoch: u64,
    pub claim: u64,
    pub player: Option<Value>,
    pub volume: f64,
    pub shutdown: bool,
    pub takeover: Option<u64>,
}
pub struct Client {
    desired: Arc<Mutex<Desired>>,
    done: mpsc::Receiver<()>,
    commands: mpsc::SyncSender<Command>,
    last: Instant,
}
impl Client {
    pub fn new(
        token: String,
        uid: String,
        device: String,
        api: Api,
        tx: mpsc::Sender<Msg>,
    ) -> Result<Self> {
        let desired = Arc::new(Mutex::new(Desired::default()));
        let shared = desired.clone();
        let (done_tx, done) = mpsc::channel();
        let (commands, pending) = mpsc::sync_channel(16);
        std::thread::Builder::new()
            .name("ynison".into())
            .spawn(move || {
                worker(Identity { token, uid, device }, api, tx, shared, pending);
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            desired,
            done,
            commands,
            last: Instant::now() - Duration::from_secs(2),
        })
    }
    pub fn command(&self, command: Command) -> bool {
        self.commands.try_send(command).is_ok()
    }
    pub fn due(&self, enabled: bool, epoch: u64) -> bool {
        let d = self.desired.locked();
        d.enabled != enabled || d.epoch != epoch || self.last.elapsed() >= Duration::from_secs(1)
    }
    pub fn update(&mut self, desired: Desired) {
        *self.desired.locked() = desired;
        self.last = Instant::now();
    }
    pub fn finish(&mut self, mut desired: Desired) {
        desired.shutdown = true;
        if let Some(p) = desired.player.as_mut() {
            p["status"]["paused"] = json!(true);
        }
        self.update(desired);
        let _ = self.done.recv_timeout(Duration::from_secs(3));
    }
}
struct Identity {
    token: String,
    uid: String,
    device: String,
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn version(device: &str) -> Value {
    json!({"device_id":device,"version":crate::api::random_u64() & (i64::MAX as u64),"timestamp_ms":now_ms()})
}
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
fn open(host: &str, path: &str, headers: &Value) -> Result<Socket> {
    // Do not accept a redirect to a different service with the account's credentials.
    if !(host == REDIRECTOR || host.ends_with(".music.yandex.ru")) {
        bail!("unexpected Ynison host");
    }
    let mut req = format!("wss://{host}{path}").into_client_request()?;
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("Bearer, v2, {}", encode(&headers.to_string())))?,
    );
    req.headers_mut().insert(
        "Origin",
        HeaderValue::from_static("https://music.yandex.ru"),
    );
    let mut stream = None;
    for addr in (host, 443).to_socket_addrs()? {
        if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            stream = Some(s);
            break;
        }
    }
    let stream = stream.context("Ynison connection failed")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let (ws, _) = tungstenite::client_tls_with_config(
        req,
        stream,
        None,
        Some(tungstenite::Connector::Rustls(crate::api::tls_config())),
    )
    .map_err(|_| anyhow!("Ynison handshake failed"))?;
    Ok(ws)
}
fn recv(ws: &mut Socket) -> Result<Option<Value>> {
    match ws.read() {
        Ok(Message::Text(s)) => Ok(Some(serde_json::from_str(&s)?)),
        Ok(Message::Close(_)) => bail!("Ynison closed"),
        Ok(_) => {
            ws.flush()?;
            Ok(None)
        }
        Err(tungstenite::Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}
pub fn device_title() -> &'static str {
    static TITLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TITLE.get_or_init(|| {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is writable and its length is passed alongside its pointer.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        let name = (result == 0)
            .then(|| buffer.split(|&byte| byte == 0).next())
            .flatten()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|hostname| hostname.split('.').next())
            .map(str::trim)
            .filter(|name| !name.is_empty());
        format!("ynison-{}", name.unwrap_or("localhost"))
    })
}

fn connect(id: &Identity, volume: f64) -> Result<Socket> {
    let mut headers = json!({"Ynison-Device-Id":id.device,"Ynison-Device-Info":json!({"app_name":"ymusic","app_version":env!("CARGO_PKG_VERSION"),"type":1}).to_string(),"authorization":format!("OAuth {}",id.token),"X-Yandex-Music-Multi-Auth-User-Id":id.uid});
    let mut redirector = open(
        REDIRECTOR,
        "/redirector.YnisonRedirectService/GetRedirectToYnison",
        &headers,
    )?;
    let r = recv(&mut redirector)?.context("Ynison redirect timeout")?;
    let host = r["host"].as_str().context("Ynison redirect failed")?;
    headers["Ynison-Redirect-Ticket"] = r["redirect_ticket"].clone();
    headers["Ynison-Session-Id"] = r["session_id"].clone();
    let _ = redirector.close(None);
    let mut ws = open(
        host,
        "/ynison_state.YnisonStateService/PutYnisonState",
        &headers,
    )?;
    let empty = json!({"device_id":id.device,"version":0,"timestamp_ms":0});
    let state = json!({"player_queue":{"current_playable_index":-1,"entity_id":"","entity_type":"VARIOUS","playable_list":[],"options":{"repeat_mode":"NONE"},"entity_context":"BASED_ON_ENTITY_BY_DEFAULT","version":empty},"status":{"duration_ms":0,"paused":true,"playback_speed":1,"progress_ms":0,"version":empty}});
    let full = json!({"player_state":state,"device":{"volume":volume,"capabilities":{"can_be_player":true,"can_be_remote_controller":true,"volume_granularity":100},"info":{"device_id":id.device,"type":"WEB","title":device_title(),"app_name":"ymusic","app_version":env!("CARGO_PKG_VERSION")},"volume_info":{"volume":volume,"version":null},"is_shadow":false},"is_currently_active":false});
    ws.send(Message::Text(json!({"update_full_state":full,"rid":crate::api::random_uuid(),"player_action_timestamp_ms":0,"activity_interception_type":"DO_NOT_INTERCEPT_BY_DEFAULT"}).to_string()))?;
    match ws.get_ref() {
        MaybeTlsStream::Rustls(t) => t
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(250)))?,
        _ => bail!("Ynison requires TLS"),
    }
    Ok(ws)
}
fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}
fn current_id(queue: &Value) -> Option<&str> {
    let index = usize::try_from(number(&queue["current_playable_index"])?).ok()?;
    queue["playable_list"].get(index)?["playable_id"].as_str()
}

#[derive(Default)]
struct RemoteVersions {
    status: u64,
    queue: u64,
    volume: u64,
}

impl RemoteVersions {
    fn accept(version: &Value, device: &str, previous: &mut u64) -> bool {
        let (Some(author), Some(timestamp)) = (
            version["device_id"].as_str(),
            number(&version["timestamp_ms"]),
        ) else {
            return false;
        };
        if timestamp <= *previous {
            return false;
        }
        *previous = timestamp;
        !author.is_empty() && author != device
    }

    fn volume_event(&mut self, frame: &Value, device: &str) -> Option<Event> {
        let target = frame["devices"]
            .as_array()?
            .iter()
            .find(|d| d["info"]["device_id"] == device)?;
        let info = &target["volume_info"];
        if !Self::accept(&info["version"], device, &mut self.volume) {
            return None;
        }
        let volume = info["volume"]
            .as_f64()
            .filter(|v| (0.0..=1.0).contains(v))?;
        Some(Event::RemoteVolume(volume))
    }

    fn events(&mut self, frame: &Value, device: &str, desired: &Desired, api: &Api) -> Vec<Event> {
        let Some(local) = desired.player.as_ref() else {
            return Vec::new();
        };
        if frame["active_device_id_optional"].as_str() != Some(device) {
            return Vec::new();
        }
        let mut events = Vec::new();
        let state = &frame["player_state"];
        let status = &state["status"];
        let queue = &state["player_queue"];
        let position = number(&status["progress_ms"]).map(|p| p as f64 / 1000.0);
        let paused = status["paused"].as_bool();
        let status_changed = Self::accept(&status["version"], device, &mut self.status);
        let queue_changed = Self::accept(&queue["version"], device, &mut self.queue);
        if queue_changed
            && let Some(items) = queue["playable_list"].as_array()
            && let Some(local_items) = local["player_queue"]["playable_list"].as_array()
            && items.len() == local_items.len()
            && items
                .iter()
                .zip(local_items)
                .all(|(a, b)| a["playable_id"] == b["playable_id"])
            && let Some(track_ids) = items
                .iter()
                .map(|p| p["playable_id"].as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
            && let Some(index) = number(&queue["current_playable_index"])
                .and_then(|i| usize::try_from(i).ok())
                .filter(|&i| i < items.len())
            && let (Some(position), Some(paused)) = (position, paused)
        {
            use crate::queue::Repeat;
            let repeat = match queue["options"]["repeat_mode"].as_str() {
                Some("ONE") => Repeat::One,
                Some("ALL") => Repeat::All,
                _ => Repeat::Off,
            };
            events.push(Event::RemoteQueue {
                claim: desired.claim,
                track_ids,
                shuffle: order(queue, items.len()),
                index,
                repeat,
                position,
                paused,
            });
        } else if queue_changed {
            if let Some(paused) = paused
                && let Ok(Some(restored)) = restore(frame, api)
            {
                events.push(Event::RemoteReplacement {
                    claim: desired.claim,
                    restored,
                    paused,
                });
            }
        } else if status_changed
            && let Some(id) = current_id(&state["player_queue"])
            && current_id(&local["player_queue"]) == Some(id)
            && let (Some(paused), Some(progress)) =
                (status["paused"].as_bool(), number(&status["progress_ms"]))
        {
            events.push(Event::RemotePlayback {
                claim: desired.claim,
                track_id: id.to_owned(),
                paused,
                position: progress as f64 / 1000.0,
            });
        }
        events
    }
}
fn live_position(status: &Value) -> Option<u64> {
    let position = number(&status["progress_ms"])?;
    let elapsed = match (
        status["paused"].as_bool(),
        number(&status["version"]["timestamp_ms"]),
    ) {
        (Some(false), Some(timestamp)) if timestamp > 0 => now_ms().saturating_sub(timestamp),
        _ => 0,
    };
    let speed = status["playback_speed"]
        .as_f64()
        .filter(|v| (0.1..=10.0).contains(v))
        .unwrap_or(1.0);
    Some(position.saturating_add((elapsed as f64 * speed) as u64))
}

fn restore_live(frame: &Value, api: &Api) -> Option<Restored> {
    let mut restored = restore(frame, api).ok().flatten()?;
    if let Some(position) = live_position(&frame["player_state"]["status"]) {
        restored.position = position as f64 / 1000.0;
    }
    Some(restored)
}

fn restore(v: &Value, api: &Api) -> Result<Option<Restored>> {
    let q = &v["player_state"]["player_queue"];
    let Some(items) = q["playable_list"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 10000)
    else {
        return Ok(None);
    };
    let index = number(&q["current_playable_index"])
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&i| i < items.len())
        .context("invalid Ynison index")?;
    if items
        .iter()
        .any(|p| p["playable_type"] != "TRACK" || p["playable_id"].as_str().is_none())
    {
        bail!("unsupported Ynison queue");
    }
    let ids: Vec<_> = items
        .iter()
        .map(|p| p["playable_id"].as_str().unwrap().to_owned())
        .collect();
    let tracks = api.tracks(&ids)?;
    let by_id: std::collections::HashMap<_, _> =
        tracks.into_iter().map(|t| (t.id.clone(), t)).collect();
    let tracks = ids
        .iter()
        .zip(items)
        .map(|(id, item)| {
            let mut t = by_id.get(id).cloned().context("Ynison track missing")?;
            t.batch = item["track_info"]["batch_id_optional"]
                .as_str()
                .map(str::to_owned);
            Ok(t)
        })
        .collect::<Result<Vec<_>>>()?;
    let position = number(&v["player_state"]["status"]["progress_ms"]).unwrap_or(0) as f64 / 1000.0;
    Ok(Some(Restored {
        queue: q.clone(),
        tracks,
        index,
        position,
    }))
}
fn envelope(key: &str, payload: Value, claim: bool) -> Value {
    let mut v = json!({"rid":crate::api::random_uuid(),"player_action_timestamp_ms":now_ms(),"activity_interception_type":if claim {"INTERCEPT_EAGER"} else {"DO_NOT_INTERCEPT_BY_DEFAULT"}});
    v[key] = payload;
    v
}
fn publish(
    ws: &mut Socket,
    id: &Identity,
    state: &Value,
    previous: Option<&Value>,
    claim: bool,
) -> Result<()> {
    let mut state = state.clone();
    let full = claim || previous.is_none_or(|p| p["player_queue"] != state["player_queue"]);
    state["status"]["version"] = version(&id.device);
    let msg = if full {
        state["player_queue"]["version"] = version(&id.device);
        envelope("update_player_state", json!({"player_state":state}), claim)
    } else {
        envelope(
            "update_playing_status",
            json!({"playing_status":state["status"]}),
            false,
        )
    };
    ws.send(Message::Text(msg.to_string()))?;
    Ok(())
}
fn devices(frame: &Value) -> Vec<Device> {
    let mut phones: Vec<_> = frame["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| {
            matches!(d["info"]["type"].as_str(), Some("ANDROID" | "IOS"))
                && d["is_offline"] != true
                && d["capabilities"]["can_be_player"] == true
        })
        .filter_map(|d| {
            Some(Device {
                id: d["info"]["device_id"].as_str()?.to_owned(),
                title: d["info"]["title"]
                    .as_str()
                    .or_else(|| d["info"]["app_name"].as_str())
                    .unwrap_or("Телефон")
                    .to_owned(),
                volume_steps: number(&d["capabilities"]["volume_granularity"]).unwrap_or(0),
            })
        })
        .collect();
    phones.sort_by(|a, b| a.id.cmp(&b.id));
    phones.dedup_by(|a, b| a.id == b.id);
    phones
}

pub fn order(queue: &Value, count: usize) -> Option<Vec<usize>> {
    let entries = queue["shuffle_optional"]["playable_indices"].as_array()?;
    if entries.len() != count {
        return None;
    }
    let mut seen = vec![false; count];
    entries
        .iter()
        .map(|entry| {
            let index = usize::try_from(number(entry)?).ok()?;
            (!std::mem::replace(seen.get_mut(index)?, true)).then_some(index)
        })
        .collect()
}

fn control_message(frame: &Value, local: &str, phone: &Device, control: &Control) -> Option<Value> {
    let active = frame["active_device_id_optional"].as_str()?;
    if matches!(control, Control::Transfer) {
        return (active == local).then(|| {
            envelope(
                "update_active_device",
                json!({"device_id_optional":phone.id}),
                false,
            )
        });
    }
    if active != phone.id {
        return None;
    }
    if let Control::Volume(delta) = control {
        if phone.volume_steps == 0 {
            return None;
        }
        let d = frame["devices"]
            .as_array()?
            .iter()
            .find(|d| d["info"]["device_id"] == phone.id)?;
        let volume = d["volume_info"]["volume"]
            .as_f64()
            .or_else(|| d["volume"].as_f64())?;
        let steps = phone.volume_steps as f64;
        let volume = (((volume + delta.signum() * delta.abs().max(1.0 / steps)).clamp(0.0, 1.0)
            * steps)
            .round()
            / steps)
            .clamp(0.0, 1.0);
        return Some(envelope(
            "update_volume_info",
            json!({"device_id":phone.id,"volume_info":{"volume":volume,"version":version(local)}}),
            false,
        ));
    }
    let mut state = frame.get("player_state")?.clone();
    let queue = &mut state["player_queue"];
    let len = queue["playable_list"].as_array()?.len();
    let index = usize::try_from(number(&queue["current_playable_index"])?).ok()?;
    if index >= len {
        return None;
    }
    let mut queue_changed = true;
    let mut track_changed = false;
    match control {
        Control::Transfer | Control::Volume(_) => return None,
        Control::Pause | Control::Seek(_) => queue_changed = false,
        Control::Repeat => {
            queue["options"]["repeat_mode"] =
                json!(match queue["options"]["repeat_mode"].as_str() {
                    Some("ALL") => "ONE",
                    Some("ONE") => "NONE",
                    _ => "ALL",
                });
        }
        Control::Shuffle => {
            if queue["entity_type"] == "RADIO" {
                return None;
            }
            queue["shuffle_optional"] = if order(queue, len).is_some() {
                Value::Null
            } else {
                let mut indices: Vec<_> = (0..len).filter(|&i| i != index).collect();
                crate::queue::shuffle(&mut indices);
                indices.insert(0, index);
                json!({"playable_indices":indices})
            };
        }
        Control::Track(delta) => {
            let indices = order(queue, len).unwrap_or_else(|| (0..len).collect());
            let current = indices.iter().position(|&i| i == index)? as i64;
            let mut next = current + delta;
            if queue["options"]["repeat_mode"] == "ALL" {
                next = next.rem_euclid(len as i64);
            }
            let next = *indices.get(usize::try_from(next).ok()?)?;
            queue["current_playable_index"] = json!(next);
            if queue["queue"]["wave_queue"].is_object() {
                queue["queue"]["wave_queue"]["live_playable_index"] = json!(next);
            }
            track_changed = true;
        }
    }
    let status = &mut state["status"];
    let paused = status["paused"].as_bool()?;
    let duration = number(&status["duration_ms"]).unwrap_or(0);
    let mut position = live_position(status)?;
    if let Control::Seek(delta) = control {
        position = position.saturating_add_signed(*delta);
    }
    status["progress_ms"] = json!(if track_changed {
        0
    } else if duration > 0 {
        position.min(duration)
    } else {
        position
    });
    if track_changed {
        status["duration_ms"] = json!(0);
    }
    if matches!(control, Control::Pause) {
        status["paused"] = json!(!paused);
    }
    status["version"] = version(local);
    Some(if queue_changed {
        state["player_queue"]["version"] = version(local);
        envelope("update_player_state", json!({"player_state":state}), false)
    } else {
        envelope(
            "update_playing_status",
            json!({"playing_status":status}),
            false,
        )
    })
}

fn worker(
    id: Identity,
    api: Api,
    tx: mpsc::Sender<Msg>,
    desired: Arc<Mutex<Desired>>,
    commands: mpsc::Receiver<Command>,
) {
    let mut retry = Instant::now();
    let mut failed = false;
    let mut forbidden_epoch = None;
    let mut last_claim = None;
    let mut blocked_claim = None;
    let mut handled_takeover = None;
    loop {
        let d = desired.locked().clone();
        if d.shutdown {
            return;
        }
        if !d.enabled || forbidden_epoch == Some(d.epoch) || Instant::now() < retry {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let result = (|| -> Result<()> {
            let mut ws = connect(&id, d.volume)?;
            let mut previous = None;
            let mut restored = false;
            let mut claimed = None;
            let mut blocked = blocked_claim
                .filter(|(epoch, _)| *epoch == d.epoch)
                .map(|(_, claim)| claim);
            let mut volume = d.volume;
            let mut received = false;
            let mut confirmed = false;
            let mut remote_versions = RemoteVersions::default();
            let mut snapshot: Option<Value> = None;
            let mut visible = None;
            let mut phones = Vec::new();
            let mut command_ack: Option<(String, Instant)> = None;
            let mut switched_here = false;
            let connected_at = Instant::now();
            loop {
                let current = desired.locked().clone();
                if !current.enabled || current.epoch != d.epoch {
                    let _ = ws.close(None);
                    return Ok(());
                }
                if let Some(state) = current.player.as_ref().filter(|_| received)
                    && blocked.is_none_or(|c| current.claim > c)
                    && (claimed != Some(current.claim)
                        || previous.as_ref() != Some(state)
                        || current.shutdown)
                {
                    let claim = claimed != Some(current.claim);
                    publish(&mut ws, &id, state, previous.as_ref(), claim)?;
                    previous = Some(state.clone());
                    claimed = Some(current.claim);
                    last_claim = Some((d.epoch, current.claim));
                }
                if received && current.volume != volume {
                    let msg = envelope(
                        "update_volume_info",
                        json!({"device_id":id.device,"volume_info":{"volume":current.volume,"version":version(&id.device)}}),
                        false,
                    );
                    ws.send(Message::Text(msg.to_string()))?;
                    volume = current.volume;
                }
                if current.shutdown {
                    let _ = ws.close(None);
                    return Ok(());
                }
                if command_ack
                    .as_ref()
                    .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
                {
                    command_ack = None;
                    let _ = tx.send(Msg::Ynison {
                        epoch: d.epoch,
                        event: Event::ControlUnavailable,
                    });
                }
                // Wait for the server echo before calculating the next relative command.
                if command_ack.is_none()
                    && let Some(frame) = snapshot.as_ref()
                    && let Ok(command) = commands.try_recv()
                    && command.epoch == d.epoch
                    && command.created >= connected_at
                    && command.created.elapsed() < Duration::from_secs(3)
                {
                    let message = phones
                        .iter()
                        .find(|phone: &&Device| phone.id == command.target)
                        .and_then(|phone| {
                            control_message(frame, &id.device, phone, &command.control)
                        });
                    if let Some(message) = message {
                        ws.send(Message::Text(message.to_string()))?;
                        command_ack = Some((
                            message["rid"].as_str().unwrap().to_owned(),
                            Instant::now() + Duration::from_secs(2),
                        ));
                    } else {
                        let _ = tx.send(Msg::Ynison {
                            epoch: d.epoch,
                            event: Event::ControlUnavailable,
                        });
                    }
                }
                if let Some(request) = current.takeover
                    && handled_takeover != Some((d.epoch, request))
                    && let Some(frame) = snapshot.as_ref()
                {
                    handled_takeover = Some((d.epoch, request));
                    let restored = frame["active_device_id_optional"]
                        .as_str()
                        .filter(|active| *active != id.device)
                        .and_then(|_| restore_live(frame, &api));
                    let _ = tx.send(Msg::Ynison {
                        epoch: d.epoch,
                        event: Event::Takeover { request, restored },
                    });
                }
                if let Some(v) = recv(&mut ws)? {
                    if command_ack
                        .as_ref()
                        .is_some_and(|(rid, _)| v["rid"] == rid.as_str())
                    {
                        command_ack = None;
                    }
                    if let Some(e) = v.get("error") {
                        if number(&e["grpc_code"]) == Some(16)
                            || number(&e["http_code"]) == Some(401)
                        {
                            forbidden_epoch = Some(d.epoch);
                        }
                        let seconds = number(&e["details"]["ynison-go-away-for-seconds"])
                            .unwrap_or(2)
                            .clamp(2, 3600);
                        retry = Instant::now() + Duration::from_secs(seconds);
                        bail!("Ynison service unavailable");
                    }
                    let reconnecting = !received && last_claim == Some((d.epoch, current.claim));
                    received |= v.get("player_state").is_some();
                    failed = false;
                    if v["active_device_id_optional"].as_str() == Some(id.device.as_str()) {
                        confirmed = true;
                    }
                    if let Some(event) = remote_versions.volume_event(&v, &id.device) {
                        let _ = tx.send(Msg::Ynison {
                            epoch: d.epoch,
                            event,
                        });
                    }
                    if claimed == Some(current.claim) {
                        for event in remote_versions.events(&v, &id.device, &current, &api) {
                            let _ = tx.send(Msg::Ynison {
                                epoch: d.epoch,
                                event,
                            });
                        }
                    }
                    if (confirmed && claimed.is_some() || reconnecting)
                        && v["active_device_id_optional"]
                            .as_str()
                            .is_some_and(|a| a != id.device)
                    {
                        let claim = claimed.take().unwrap_or(current.claim);
                        blocked = Some(claim);
                        blocked_claim = Some((d.epoch, claim));
                        confirmed = false;
                        let _ = tx.send(Msg::Ynison {
                            epoch: d.epoch,
                            event: Event::Relinquished { claim },
                        });
                    }
                    let active_here =
                        v["active_device_id_optional"].as_str() == Some(id.device.as_str());
                    let status = &v["player_state"]["status"];
                    switched_here = active_here
                        && (switched_here
                            || snapshot.as_ref().is_some_and(|previous| {
                                previous["active_device_id_optional"]
                                    .as_str()
                                    .is_some_and(|prior| prior != id.device)
                            }));
                    if switched_here
                        && current.player.is_none()
                        && current.takeover.is_none()
                        && let Some(paused) = status["paused"].as_bool()
                    {
                        switched_here = false;
                        if let Some(loaded) = restore_live(&v, &api) {
                            restored = true;
                            let _ = tx.send(Msg::Ynison {
                                epoch: d.epoch,
                                event: Event::SelectedLocally {
                                    claim: current.claim,
                                    restored: loaded,
                                    paused,
                                },
                            });
                        }
                    }
                    if !restored && current.player.is_none() && current.takeover.is_none() {
                        match restore(&v, &api) {
                            Ok(Some(r)) => {
                                restored = true;
                                let _ = tx.send(Msg::Ynison {
                                    epoch: d.epoch,
                                    event: Event::Restore(r),
                                });
                            }
                            Ok(None) => {}
                            Err(_) => restored = true,
                        }
                    }
                    if v.get("devices").is_some() {
                        phones = devices(&v);
                        let active = v["active_device_id_optional"].as_str().map(str::to_owned);
                        let view = (phones.clone(), active);
                        if visible.as_ref() != Some(&view) {
                            let _ = tx.send(Msg::Ynison {
                                epoch: d.epoch,
                                event: Event::Devices {
                                    phones: view.0.clone(),
                                    active: view.1.clone(),
                                    local: id.device.clone(),
                                },
                            });
                            visible = Some(view);
                        }
                    }
                    if v.get("player_state").is_some() {
                        snapshot = Some(v);
                    }
                }
            }
        })();
        if result.is_err() {
            if !failed {
                let _ = tx.send(Msg::Ynison {
                    epoch: d.epoch,
                    event: Event::Error,
                });
            }
            failed = true;
            retry = retry.max(Instant::now() + Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phone_controls_follow_shuffle_and_keep_the_active_device() {
        let phone = Device {
            id: "phone".into(),
            title: "Android".into(),
            volume_steps: 10,
        };
        let mut frame = json!({
            "active_device_id_optional":"phone",
            "devices":[{"info":{"device_id":"phone"},"volume_info":{"volume":0.5}}],
            "player_state":{
                "player_queue":{
                    "current_playable_index":2,
                    "playable_list":[{"playable_id":"a"},{"playable_id":"b"},{"playable_id":"c"}],
                    "shuffle_optional":{"playable_indices":[2,0,1]},
                    "options":{"repeat_mode":"ALL"}
                },
                "status":{"paused":true,"progress_ms":12000,"duration_ms":60000}
            }
        });
        let control = |command| control_message(&frame, "local", &phone, &command).unwrap();
        let next = control(Control::Track(1));
        let state = &next["update_player_state"]["player_state"];
        assert_eq!(state["player_queue"]["current_playable_index"], 0);
        assert_eq!(state["status"]["progress_ms"], 0);
        assert_eq!(
            next["activity_interception_type"],
            "DO_NOT_INTERCEPT_BY_DEFAULT"
        );
        let prev = control(Control::Track(-1));
        assert_eq!(
            prev["update_player_state"]["player_state"]["player_queue"]["current_playable_index"],
            1
        );
        assert_eq!(
            control(Control::Seek(-10000))["update_playing_status"]["playing_status"]["progress_ms"],
            2000
        );
        assert_eq!(
            control(Control::Pause)["update_playing_status"]["playing_status"]["paused"],
            false
        );
        assert_eq!(
            control(Control::Volume(0.05))["update_volume_info"]["volume_info"]["volume"],
            0.6
        );
        assert!(control(Control::Shuffle)["update_player_state"]["player_state"]["player_queue"]["shuffle_optional"].is_null());
        frame["active_device_id_optional"] = json!("local");
        assert!(control_message(&frame, "local", &phone, &Control::Pause).is_none());
        assert_eq!(
            control_message(&frame, "local", &phone, &Control::Transfer).unwrap()["update_active_device"]
                ["device_id_optional"],
            "phone"
        );
    }

    #[test]
    fn remote_updates_require_the_local_target_and_a_new_external_version() {
        let queue = json!({"current_playable_index":"0","playable_list":[{"playable_id":"42"}]});
        let desired = Desired {
            claim: 3,
            player: Some(json!({"player_queue":queue})),
            ..Desired::default()
        };
        let mut frame = json!({
            "active_device_id_optional":"phone",
            "player_state":{
                "player_queue":queue,
                "status":{"paused":true,"progress_ms":"2500",
                          "version":{"device_id":"phone","timestamp_ms":"10"}}
            },
            "devices":[{"info":{"device_id":"local"},
                        "volume_info":{"volume":0.75,
                                       "version":{"device_id":"phone","timestamp_ms":9}}}]
        });
        let mut versions = RemoteVersions::default();
        assert!(
            versions
                .events(&frame, "local", &desired, &Api::new("unused"))
                .is_empty()
        );
        assert!(matches!(
            versions.volume_event(&frame, "local"),
            Some(Event::RemoteVolume(0.75))
        ));
        assert!(versions.volume_event(&frame, "local").is_none());
        frame["active_device_id_optional"] = json!("local");
        assert!(matches!(
            versions
                .events(&frame, "local", &desired, &Api::new("unused"))
                .as_slice(),
            [Event::RemotePlayback {
                claim: 3,
                paused: true,
                position: 2.5,
                ..
            }]
        ));
        assert!(
            versions
                .events(&frame, "local", &desired, &Api::new("unused"))
                .is_empty()
        );
        frame["player_state"]["status"]["version"] = json!({"device_id":"local","timestamp_ms":11});
        assert!(
            versions
                .events(&frame, "local", &desired, &Api::new("unused"))
                .is_empty()
        );
    }
    #[test]
    fn invalid_indices_and_number_formats() {
        assert_eq!(number(&json!("50866")), Some(50866));
        assert_eq!(number(&json!(-1)), None);
        assert_eq!(number(&json!("-1")), None);
        let api = Api::new("unused");
        assert!(restore(&json!({"player_state":{"player_queue":{"playable_list":[{"playable_type":"TRACK","playable_id":"1"}],"current_playable_index":5}}}), &api).is_err());
        assert!(restore(&json!({}), &api).unwrap().is_none());
    }
    #[test]
    fn protocol_publishes_queue_then_position_without_reclaiming() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            (0..2)
                .map(|_| {
                    let Message::Text(t) = ws.read().unwrap() else {
                        panic!("expected JSON")
                    };
                    serde_json::from_str::<Value>(&t).unwrap()
                })
                .collect::<Vec<_>>()
        });
        let (mut ws, _) = tungstenite::client(
            format!("ws://{address}/"),
            TcpStream::connect(address).unwrap(),
        )
        .unwrap();
        // The production publisher accepts the same stream wrapper as the TLS transport.
        let tcp = ws.get_mut().try_clone().unwrap();
        drop(ws);
        let mut ws = WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(tcp),
            tungstenite::protocol::Role::Client,
            None,
        );
        let id = Identity {
            token: String::new(),
            uid: "1".into(),
            device: "test".into(),
        };
        let first = json!({"player_queue":{"current_playable_index":1},"status":{"progress_ms":1000,"paused":false}});
        publish(&mut ws, &id, &first, None, true).unwrap();
        let mut second = first.clone();
        second["status"]["progress_ms"] = json!(2000);
        publish(&mut ws, &id, &second, Some(&first), false).unwrap();
        let msgs = server.join().unwrap();
        assert_eq!(msgs[0]["activity_interception_type"], "INTERCEPT_EAGER");
        assert_eq!(
            msgs[0]["update_player_state"]["player_state"]["player_queue"]["current_playable_index"],
            1
        );
        assert_eq!(
            msgs[1]["activity_interception_type"],
            "DO_NOT_INTERCEPT_BY_DEFAULT"
        );
        assert_eq!(
            msgs[1]["update_playing_status"]["playing_status"]["progress_ms"],
            2000
        );
    }
    #[test]
    #[ignore]
    fn network_restore() {
        let token = crate::config::env_str("TOKEN")
            .or_else(|| crate::config::Config::load().token)
            .expect("token");
        let mut api = Api::new(&token);
        let account = api.account_status().unwrap();
        let id = Identity {
            token,
            uid: account.uid,
            device: crate::api::random_uuid(),
        };
        let mut ws = connect(&id, 0.5).unwrap();
        let until = Instant::now() + Duration::from_secs(12);
        while Instant::now() < until {
            if let Some(v) = recv(&mut ws).unwrap() {
                assert!(v.get("error").is_none(), "Ynison service error");
                if let Some(r) = restore(&v, &api).unwrap() {
                    println!(
                        "Ynison: queue={}, index={}, title={}, position={}s",
                        r.tracks.len(),
                        r.index,
                        r.tracks[r.index].title,
                        r.position
                    );
                    let _ = ws.close(None);
                    return;
                }
            }
        }
        panic!("no restorable Ynison state");
    }
}
