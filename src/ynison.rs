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
pub enum Event {
    Restore(Restored),
    Relinquished { claim: u64 },
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
}
pub struct Client {
    desired: Arc<Mutex<Desired>>,
    done: mpsc::Receiver<()>,
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
        std::thread::Builder::new()
            .name("ynison".into())
            .spawn(move || {
                worker(Identity { token, uid, device }, api, tx, shared);
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            desired,
            done,
            last: Instant::now() - Duration::from_secs(2),
        })
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
fn connect(id: &Identity) -> Result<Socket> {
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
    let full = json!({"player_state":state,"device":{"volume":0.5,"capabilities":{"can_be_player":true,"can_be_remote_controller":false,"volume_granularity":100},"info":{"device_id":id.device,"type":"WEB","title":"ymusic","app_name":"ymusic","app_version":env!("CARGO_PKG_VERSION")},"volume_info":{"volume":0.5,"version":null},"is_shadow":false},"is_currently_active":false});
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
fn worker(id: Identity, api: Api, tx: mpsc::Sender<Msg>, desired: Arc<Mutex<Desired>>) {
    let mut retry = Instant::now();
    let mut failed = false;
    let mut forbidden_epoch = None;
    let mut last_claim = None;
    let mut blocked_claim = None;
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
            let mut ws = connect(&id)?;
            let mut previous = None;
            let mut restored = false;
            let mut claimed = None;
            let mut blocked = blocked_claim
                .filter(|(epoch, _)| *epoch == d.epoch)
                .map(|(_, claim)| claim);
            let mut volume = -1.0;
            let mut received = false;
            let mut confirmed = false;
            loop {
                let current = desired.locked().clone();
                if !current.enabled || current.epoch != d.epoch {
                    let _ = ws.close(None);
                    return Ok(());
                }
                if let Some(state) = current.player.as_ref().filter(|_| received) {
                    if blocked.is_none_or(|c| current.claim > c)
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
                    if blocked.is_none_or(|c| current.claim > c) && current.volume != volume {
                        let msg = envelope(
                            "update_volume_info",
                            json!({"device_id":id.device,"volume_info":{"volume":current.volume,"version":version(&id.device)}}),
                            false,
                        );
                        ws.send(Message::Text(msg.to_string()))?;
                        volume = current.volume;
                    }
                }
                if current.shutdown {
                    let _ = ws.close(None);
                    return Ok(());
                }
                if let Some(v) = recv(&mut ws)? {
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
                    if !restored
                        && current.player.is_none()
                        && let Some(r) = restore(&v, &api)?
                    {
                        restored = true;
                        let _ = tx.send(Msg::Ynison {
                            epoch: d.epoch,
                            event: Event::Restore(r),
                        });
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
        let mut ws = connect(&id).unwrap();
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
