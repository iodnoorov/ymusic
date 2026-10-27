//! Last.fm: «now playing» and scrobbles (https://www.last.fm/api/scrobbling), connected with the
//! desktop authorization (https://www.last.fm/api/desktopauth).

use md5::{Digest, Md5};
use serde_json::Value;
use std::time::Duration;

const API: &str = "https://ws.audioscrobbler.com/2.0/";
/// The program's account on Last.fm (https://www.last.fm/api/account/create).
const KEY: &str = "b3275a2a561d9006a38081c0e182021d";
const SECRET: &str = "048927389cbe2588242dc0a80b36f493";
/// Most scrobbles one request takes.
pub const MAX_BATCH: usize = 50;

/// The program has its key: Last.fm can be connected.
pub fn available() -> bool {
    !KEY.is_empty()
}

/// A listened track as Last.fm takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Scrobble {
    pub artist: String,
    pub track: String,
    pub album: String,
    /// Seconds (0 — unknown).
    pub duration: u64,
    /// When the playback started, Unix time.
    pub timestamp: u64,
}

/// An error of a Last.fm request: its code (https://www.last.fm/api/errorcodes), 0 if the server
/// did not answer.
#[derive(Clone, Debug)]
pub struct Error {
    pub code: u32,
    pub message: String,
}

impl Error {
    /// The session is no longer valid (access revoked): Last.fm must be connected again.
    pub fn is_session(&self) -> bool {
        matches!(self.code, 4 | 9)
    }

    /// The user has not confirmed the access in the browser yet.
    pub fn not_authorized(&self) -> bool {
        self.code == 14
    }

    /// Worth trying again later: no answer, the service is down or busy.
    pub fn is_temporary(&self) -> bool {
        matches!(self.code, 0 | 8 | 11 | 16 | 29)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// `api_sig`: MD5 of the parameters sorted by name, each name followed by its value, and the secret.
fn signature(params: &[(String, String)], secret: &str) -> String {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort();
    let mut s = String::new();
    for (k, v) in sorted {
        s.push_str(k);
        s.push_str(v);
    }
    s.push_str(secret);
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A signed call of `method` (POST, the answer in JSON).
fn call(method: &str, params: Vec<(String, String)>) -> Result<Value> {
    let mut params = params;
    params.push(("method".into(), method.into()));
    params.push(("api_key".into(), KEY.into()));
    let sig = signature(&params, SECRET);
    params.push(("api_sig".into(), sig));
    params.push(("format".into(), "json".into()));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("ymusic/", env!("CARGO_PKG_VERSION")))
        .tls_config(crate::tls_config())
        .build();
    let form: Vec<(&str, &str)> = params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let answer = match agent.post(API).send_form(&form) {
        Ok(r) => r.into_json::<Value>().map_err(|e| Error { code: 0, message: format!("Last.fm: {e}") })?,
        // Errors come with an error status and the same JSON.
        Err(ureq::Error::Status(code, r)) => {
            r.into_json::<Value>().unwrap_or_else(|_| serde_json::json!({"error": 0, "message": format!("HTTP {code}")}))
        }
        Err(e) => return Err(Error { code: 0, message: format!("Last.fm: {e}") }),
    };
    match answer["error"].as_u64() {
        Some(code) => {
            let message = answer["message"].as_str().unwrap_or("неизвестная ошибка");
            Err(Error { code: code as u32, message: format!("Last.fm: {message}") })
        }
        None => Ok(answer),
    }
}

fn param(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_string(), v.into())
}

/// A token for connecting: confirmed by the user at `auth_url`, then traded for a session.
pub fn token() -> Result<String> {
    let v = call("auth.getToken", Vec::new())?;
    v["token"].as_str().map(String::from).ok_or_else(|| Error { code: 0, message: "Last.fm: нет токена в ответе".into() })
}

/// The page where the user lets the program scrobble.
pub fn auth_url(token: &str) -> String {
    format!("https://www.last.fm/api/auth/?api_key={KEY}&token={token}")
}

/// The session for a confirmed token: (user name, session key). Error 14 until it is confirmed.
pub fn session(token: &str) -> Result<(String, String)> {
    let v = call("auth.getSession", vec![param("token", token)])?;
    let s = &v["session"];
    match (s["name"].as_str(), s["key"].as_str()) {
        (Some(name), Some(key)) => Ok((name.to_string(), key.to_string())),
        _ => Err(Error { code: 0, message: "Last.fm: нет сессии в ответе".into() }),
    }
}

/// The track's fields; `suffix` is `[i]` in a batch of scrobbles.
fn track_params(t: &Scrobble, suffix: &str, params: &mut Vec<(String, String)>) {
    params.push(param(&format!("artist{suffix}"), t.artist.as_str()));
    params.push(param(&format!("track{suffix}"), t.track.as_str()));
    if !t.album.is_empty() {
        params.push(param(&format!("album{suffix}"), t.album.as_str()));
    }
    if t.duration > 0 {
        params.push(param(&format!("duration{suffix}"), t.duration.to_string()));
    }
}

/// Shows the track as playing now on the user's profile.
pub fn now_playing(session: &str, t: &Scrobble) -> Result<()> {
    let mut params = vec![param("sk", session)];
    track_params(t, "", &mut params);
    call("track.updateNowPlaying", params).map(drop)
}

/// Adds listened tracks to the user's history (at most `MAX_BATCH`).
pub fn scrobble(session: &str, tracks: &[Scrobble]) -> Result<()> {
    let mut params = vec![param("sk", session)];
    for (i, t) in tracks.iter().take(MAX_BATCH).enumerate() {
        let suffix = format!("[{i}]");
        track_params(t, &suffix, &mut params);
        params.push(param(&format!("timestamp{suffix}"), t.timestamp.to_string()));
    }
    call("track.scrobble", params).map(drop)
}

/// When a track counts as listened: half of it or 4 minutes, whichever comes first; tracks of
/// 30 seconds or shorter are never scrobbled. `None` — never.
pub fn scrobble_after(duration: f64) -> Option<f64> {
    (duration > 30.0).then(|| (duration / 2.0).min(240.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_by_sorted_params() {
        let params = vec![param("token", "t"), param("method", "auth.getSession"), param("api_key", "k")];
        // md5("api_keykmethodauth.getSessiontokentsecret")
        let expected: String =
            Md5::digest(b"api_keykmethodauth.getSessiontokentsecret").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(signature(&params, "secret"), expected);
        assert_eq!(expected.len(), 32);
    }

    #[test]
    fn batch_params() {
        let t = Scrobble { artist: "A".into(), track: "T".into(), album: String::new(), duration: 200, timestamp: 1 };
        let mut p = Vec::new();
        track_params(&t, "[0]", &mut p);
        assert_eq!(p, [param("artist[0]", "A"), param("track[0]", "T"), param("duration[0]", "200")]);
    }

    /// The key and the signature are accepted: a token comes, and it is not confirmed yet.
    #[test]
    #[ignore]
    fn network_token() {
        let t = token().unwrap();
        assert!(!t.is_empty());
        let e = session(&t).unwrap_err();
        assert!(e.not_authorized(), "{e:?}");
    }

    #[test]
    fn listened_after() {
        assert_eq!(scrobble_after(30.0), None);
        assert_eq!(scrobble_after(100.0), Some(50.0));
        assert_eq!(scrobble_after(600.0), Some(240.0));
    }
}
