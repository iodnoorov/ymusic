//! Minimal client for the (unofficial) Yandex Music API.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Most requests of one operation run at once (a large library is loaded in parts). The server
/// answers 429 «Concurrency limit exceeded» when overloaded, so few.
const PARALLEL: usize = 2;

/// Pauses before repeating a request the server turned away with 429: it was not carried out.
const RETRY_429_SECS: [u64; 3] = [1, 2, 4];

/// Runs `f` for each item on up to `PARALLEL` threads; results keep the order of `items`.
/// The first error (in item order) is returned.
fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> Result<R> + Sync) -> Result<Vec<R>> {
    if items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    let workers = items.len().min(PARALLEL);
    let mut done: Vec<(usize, Result<R>)> = std::thread::scope(|s| {
        let f = &f;
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                s.spawn(move || items.iter().enumerate().skip(w).step_by(workers).map(|(i, it)| (i, f(it))).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("поток запроса упал")).collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// OAuth application of the official Yandex Music Android client.
pub const CLIENT_ID: &str = "23cabbbdc6cd418abb4b39c32c41195d";

/// API of the mobile apps.
const BASE: &str = "https://api.music.yandex.net";
/// API of the music.yandex.ru website: the same server data, its own client id and some methods of its own.
const WEB_BASE: &str = "https://api.music.yandex.ru";
/// Tracks per search request (the API returns 20 by default and accepts up to 300).
pub const SEARCH_PAGE: usize = 100;
const MD5_SALT: &str = "XGRlBW9FXlekgbPrRHuSiA";
const FILE_INFO_KEY: &str = "p93jhgh689SBReK6ghtw62";
/// `get-file-info` signing key of the website (`player.secretKey.web` in its JS bundle).
const WEB_FILE_INFO_KEY: &str = "7tvSmFbyf5hJnIHhCimDDD";
pub const MY_WAVE: &str = "user:onyourwave";

pub struct Quality {
    /// `quality` parameter of `get-file-info`; stored in the config.
    pub id: &'static str,
    pub name: &'static str,
    pub note: &'static str,
}

pub const QUALITIES: &[Quality] = &[
    Quality { id: "lq", name: "Экономия трафика", note: "≈64 кбит/с" },
    Quality { id: "nq", name: "Обычное качество", note: "≈192 кбит/с" },
];

/// Index of the quality with this id (normal quality if unknown).
pub fn quality_index(id: &str) -> usize {
    QUALITIES.iter().position(|q| q.id == id).unwrap_or(1)
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: String,
    pub album_id: Option<String>,
    pub title: String,
    pub artists: String,
    /// Main (first) artist: (id, name).
    pub main_artist: Option<(String, String)>,
    pub album: String,
    pub duration_ms: u64,
    pub available: bool,
    /// Cover image URL (400x400).
    pub cover: Option<String>,
    /// Wave batch the track came from (only for wave tracks).
    pub batch: Option<String>,
}

impl Track {
    pub fn full_id(&self) -> String {
        match &self.album_id {
            Some(a) => format!("{}:{}", self.id, a),
            None => self.id.clone(),
        }
    }

    /// Link to the track on the Yandex Music website.
    pub fn web_url(&self) -> String {
        match &self.album_id {
            Some(a) => format!("https://music.yandex.ru/album/{a}/track/{}", self.id),
            None => format!("https://music.yandex.ru/track/{}", self.id),
        }
    }

    pub fn from_json(v: &Value) -> Option<Track> {
        let id = id_str(v.get("id")?)?;
        let mut title = v["title"].as_str().unwrap_or("?").to_string();
        if let Some(ver) = v["version"].as_str().filter(|s| !s.is_empty()) {
            title = format!("{title} ({ver})");
        }
        let artists = v["artists"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let album = &v["albums"][0];
        let first = &v["artists"][0];
        Some(Track {
            id,
            album_id: id_str(&album["id"]),
            title,
            artists: if artists.is_empty() { "Неизвестный исполнитель".into() } else { artists },
            main_artist: id_str(&first["id"]).map(|id| (id, first["name"].as_str().unwrap_or("").to_string())),
            album: album["title"].as_str().unwrap_or("").to_string(),
            duration_ms: v["durationMs"].as_u64().unwrap_or(0),
            available: v["available"].as_bool().unwrap_or(true),
            cover: v["coverUri"]
                .as_str()
                .or(album["coverUri"].as_str())
                .filter(|s| !s.is_empty())
                .map(cover_url),
            batch: None,
        })
    }
}

/// Yandex Music domains a track link may point to.
const MUSIC_HOSTS: [&str; 5] = ["music.yandex.ru", "music.yandex.com", "music.yandex.by", "music.yandex.kz", "music.yandex.uz"];

/// What a Yandex Music link points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// Track id (`track:album` when the album is known).
    Track(String),
    Album(String),
    /// `/users/<owner>/playlists/<kind>`; the owner may be a login or a uid.
    Playlist { owner: String, kind: String },
    /// `/playlists/<uuid>` (newer links, optionally with a `lk.` style prefix).
    PlaylistUuid(String),
}

/// Parses a Yandex Music link to a track, album or playlist, e.g.
/// `https://music.yandex.ru/album/9/track/123?utm_source=…`. Query and fragment are dropped;
/// anything that is not a Yandex Music link is rejected without being opened.
pub fn parse_link(url: &str) -> Result<Link, &'static str> {
    let url = url.trim();
    let url = url.split(['?', '#']).next().unwrap_or("");
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !MUSIC_HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
        return Err("Это не ссылка на Яндекс Музыку");
    }
    let num = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let login = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    let uuid = |s: &str| {
        let s = s.split_once('.').filter(|(p, _)| p.bytes().all(|b| b.is_ascii_alphabetic())).map_or(s, |(_, u)| u);
        (s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then(|| s.to_string())
    };
    match path.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>()[..] {
        ["album", a, "track", t] if num(&a) && num(&t) => Ok(Link::Track(format!("{t}:{a}"))),
        ["track", t] if num(&t) => Ok(Link::Track(t.to_string())),
        // An uploaded (UGC) track.
        ["track", t] => uuid(t).map(Link::Track).ok_or("Ссылка не на трек, альбом или плейлист"),
        ["album", a] if num(&a) => Ok(Link::Album(a.to_string())),
        ["users", o, "playlists", k] if login(&o) && num(&k) => {
            Ok(Link::Playlist { owner: o.to_string(), kind: k.to_string() })
        }
        ["playlists", u] => uuid(u).map(Link::PlaylistUuid).ok_or("Ссылка не на трек, альбом или плейлист"),
        _ => Err("Ссылка не на трек, альбом или плейлист"),
    }
}

/// `avatars.yandex.net/get-music-content/…/%%` → full https URL of a 400x400 image.
pub fn cover_url(uri: &str) -> String {
    let uri = uri.replace("%%", "400x400");
    if uri.starts_with("http") { uri } else { format!("https://{uri}") }
}

fn id_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Full id (`track:album`, or just `track` without an album) of a list item `{id, albumId}`.
fn item_full_id(t: &Value) -> Option<String> {
    let id = id_str(&t["id"])?;
    Some(match id_str(&t["albumId"]) {
        Some(a) => format!("{id}:{a}"),
        None => id,
    })
}

#[derive(Clone, Debug)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub category: String,
    /// My Wave settings: (moodEnergy, diversity)
    pub settings: Option<(&'static str, &'static str)>,
}

#[derive(Clone, Debug)]
pub struct Playlist {
    /// Owner uid and playlist kind identify a playlist.
    pub owner: String,
    pub kind: String,
    pub title: String,
    pub track_count: u64,
}

impl Playlist {
    /// Unique key (titles may repeat).
    pub fn key(&self) -> String {
        format!("{}:{}", self.owner, self.kind)
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub uid: String,
    pub login: String,
    pub name: String,
    pub has_plus: bool,
}

#[derive(Clone, Debug)]
pub struct WaveBatch {
    /// `None` means legacy rotor station API is used.
    pub session_id: Option<String>,
    pub batch_id: String,
    pub tracks: Vec<Track>,
}

#[derive(Clone)]
pub struct Api {
    agent: ureq::Agent,
    token: String,
    pub uid: String,
    /// Requests go to the website API (`WEB_BASE`) instead of the mobile one.
    pub web: bool,
    /// «Меньше ИИ-музыки» is on: the website asks for wave sessions with fewer AI tracks.
    pub less_ai: bool,
}

/// TLS for every connection: the root certificates built into the program and those of the system
/// (`SSL_CERT_FILE`, `SSL_CERT_DIR` or the usual places), so that a certificate the system trusts
/// (a company or antivirus one) is accepted too. Made once.
pub fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            // Unreadable or broken system certificates are skipped: the built-in ones remain.
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("протоколы TLS по умолчанию")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn handle(r: Result<ureq::Response, ureq::Error>) -> Result<Value> {
    match r {
        Ok(resp) => Ok(resp.into_json()?),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v["error"]["message"]
                        .as_str()
                        .or(v["error"]["name"].as_str())
                        .or(v["error"].as_str())
                        // The website API: `{name, message}` without a wrapper.
                        .or(v["message"].as_str())
                        .or(v["name"].as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| body.chars().take(200).collect());
            if code == 401 {
                bail!("HTTP 401: токен недействителен, выполните `ymusic login` ({msg})");
            }
            bail!("HTTP {code}: {msg}")
        }
        Err(e) => Err(e.into()),
    }
}

impl Api {
    pub fn new(token: &str) -> Api {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent("Yandex-Music-API")
            .tls_config(tls_config())
            .build();
        Api { agent, token: token.to_string(), uid: String::new(), web: false, less_ai: false }
    }

    fn req(&self, method: &str, path: &str, query: &[(&str, &str)]) -> ureq::Request {
        let mut r = self
            .agent
            .request(method, &format!("{}{path}", if self.web { WEB_BASE } else { BASE }))
            .set("Authorization", &format!("OAuth {}", self.token))
            .set("Accept-Language", "ru");
        if self.web {
            r = r
                .set("X-Yandex-Music-Client", "YandexMusicWebNext/1.0.0")
                .set("X-Yandex-Music-Without-Invocation-Info", "1")
                .set("Origin", "https://music.yandex.ru");
            if !self.uid.is_empty() {
                r = r.set("X-Yandex-Music-Multi-Auth-User-Id", &self.uid);
            }
        } else {
            r = r.set("X-Yandex-Music-Client", "YandexMusicAndroid/24023621");
        }
        for (k, v) in query {
            r = r.query(k, v);
        }
        r
    }

    /// The answer itself: the mobile API wraps it in `{invocationInfo, result}`, the website API
    /// (asked with `X-Yandex-Music-Without-Invocation-Info`) does not.
    /// `send` makes the request; one turned away with 429 is made again after a pause.
    fn result(&self, send: impl Fn() -> Result<ureq::Response, ureq::Error>) -> Result<Value> {
        let mut r = send();
        for secs in RETRY_429_SECS {
            if !matches!(r, Err(ureq::Error::Status(429, _))) {
                break;
            }
            std::thread::sleep(Duration::from_secs(secs));
            r = send();
        }
        let mut v = handle(r)?;
        Ok(if self.web { v } else { v["result"].take() })
    }

    fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        self.result(|| self.req("GET", path, query).call())
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<Value> {
        self.result(|| self.req("POST", path, &[]).send_form(form))
    }

    /// POST with the parameters in the query and no body, as the website sends most changes.
    fn post_query(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        self.result(|| self.req("POST", path, query).call())
    }

    /// A change: the website sends its parameters in the query, the mobile apps as a form.
    fn post_change(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        if self.web {
            self.post_query(path, params)
        } else {
            self.post_form(path, params)
        }
    }

    fn post_json(&self, path: &str, query: &[(&str, &str)], body: Value) -> Result<Value> {
        self.result(|| self.req("POST", path, query).send_json(body.clone()))
    }

    // ---------------------------------------------------------------- account

    pub fn account_status(&mut self) -> Result<Account> {
        if self.web {
            let r = self.get("/account/about", &[])?;
            let uid = id_str(&r["uid"]).ok_or_else(|| anyhow!("не удалось получить uid аккаунта (токен недействителен?)"))?;
            self.uid = uid.clone();
            return Ok(Account {
                uid,
                login: r["login"].as_str().unwrap_or("").to_string(),
                name: r["publicName"].as_str().unwrap_or("").to_string(),
                has_plus: r["hasPlus"].as_bool().unwrap_or(false),
            });
        }
        let r = self.get("/account/status", &[])?;
        let acc = &r["account"];
        let uid = id_str(&acc["uid"]).ok_or_else(|| anyhow!("не удалось получить uid аккаунта (токен недействителен?)"))?;
        self.uid = uid.clone();
        Ok(Account {
            uid,
            login: acc["login"].as_str().unwrap_or("").to_string(),
            name: acc["displayName"].as_str().or(acc["fullName"].as_str()).unwrap_or("").to_string(),
            has_plus: r["plus"]["hasPlus"].as_bool().unwrap_or(false),
        })
    }

    /// «Меньше ИИ-музыки»: an account setting, recommendations mix in fewer AI-generated tracks.
    pub fn ai_reduction(&self) -> Result<bool> {
        let r = self.get("/account/settings", &[])?;
        r["aiContentReductionEnabled"].as_bool().ok_or_else(|| anyhow!("нет aiContentReductionEnabled"))
    }

    pub fn set_ai_reduction(&self, on: bool) -> Result<()> {
        self.post_change("/account/settings", &[("aiContentReductionEnabled", if on { "true" } else { "false" })])?;
        Ok(())
    }

    // ---------------------------------------------------------------- tracks

    pub fn tracks(&self, ids: &[String]) -> Result<Vec<Track>> {
        let chunks: Vec<_> = ids.chunks(200).collect();
        let parts = parallel(&chunks, |chunk| {
            let joined = chunk.join(",");
            let r = if self.web {
                self.post_form("/tracks", &[("trackIds", &joined), ("removeDuplicates", "false")])?
            } else {
                self.post_form("/tracks", &[("track-ids", &joined), ("with-positions", "false")])?
            };
            Ok(r.as_array().into_iter().flatten().filter_map(Track::from_json).collect::<Vec<_>>())
        })?;
        Ok(parts.into_iter().flatten().collect())
    }

    /// One page of track search results and the total number of found tracks. The website API
    /// does not count them: `None` there means more pages follow, the last page gives the total.
    pub fn search(&self, text: &str, page: usize) -> Result<(Vec<Track>, Option<usize>)> {
        let (n, page) = (page, page.to_string());
        let size = SEARCH_PAGE.to_string();
        if self.web {
            let r = self.get(
                "/search/instant/mixed",
                &[("text", text), ("type", "track"), ("page", &page), ("pageSize", &size), ("nocorrect", "false")],
            )?;
            let tracks: Vec<Track> =
                r["results"].as_array().into_iter().flatten().filter_map(|it| Track::from_json(&it["track"])).collect();
            let last = r["lastPage"].as_bool().unwrap_or(true) || tracks.is_empty();
            let total = last.then(|| n * SEARCH_PAGE + tracks.len());
            return Ok((tracks, total));
        }
        let r = self.get(
            "/search",
            &[("text", text), ("type", "track"), ("page", &page), ("page-size", &size), ("nocorrect", "false")],
        )?;
        let tracks = r["tracks"]["results"].as_array().into_iter().flatten().filter_map(Track::from_json).collect();
        Ok((tracks, Some(r["tracks"]["total"].as_u64().unwrap_or(0) as usize)))
    }

    /// Tracks of an artist, most popular first.
    pub fn artist_tracks(&self, artist_id: &str) -> Result<Vec<Track>> {
        const PAGE: usize = 100;
        const MAX: usize = 1000;
        // Tracks of one page and the artist's total number of tracks.
        let page = |n: usize| -> Result<(Vec<Track>, usize)> {
            let (page, size) = (n.to_string(), PAGE.to_string());
            let size_key = if self.web { "pageSize" } else { "page-size" };
            let r = self.get(&format!("/artists/{artist_id}/tracks"), &[("page", &page), (size_key, &size)])?;
            let tracks = r["tracks"].as_array().into_iter().flatten().filter_map(Track::from_json).collect();
            Ok((tracks, r["pager"]["total"].as_u64().unwrap_or(0) as usize))
        };
        // The first page tells how many there are; the rest are fetched together.
        let (mut out, total) = page(0)?;
        if out.len() == PAGE {
            let rest: Vec<usize> = (1..total.min(MAX).div_ceil(PAGE)).collect();
            out.extend(parallel(&rest, |&n| page(n).map(|(t, _)| t))?.into_iter().flatten());
        }
        out.truncate(MAX);
        Ok(out)
    }

    /// Album title and its tracks in album order (all volumes).
    pub fn album_tracks(&self, album_id: &str) -> Result<(String, Vec<Track>)> {
        let r = self.get(&format!("/albums/{album_id}/with-tracks"), &[])?;
        let tracks = r["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(Track::from_json)
            .collect();
        Ok((r["title"].as_str().unwrap_or("").to_string(), tracks))
    }

    // ---------------------------------------------------------------- playlists

    /// Playlists of the current user.
    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let r = self.get(&format!("/users/{}/playlists/list", self.uid), &[])?;
        Ok(r.as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(Playlist {
                    owner: id_str(&p["owner"]["uid"]).or_else(|| id_str(&p["uid"])).unwrap_or_else(|| self.uid.clone()),
                    kind: id_str(&p["kind"])?,
                    title: p["title"].as_str().unwrap_or("Без названия").to_string(),
                    track_count: p["trackCount"].as_u64().unwrap_or(0),
                })
            })
            .collect())
    }

    fn playlist_path(pl: &Playlist) -> String {
        format!("/users/{}/playlists/{}", pl.owner, pl.kind)
    }

    /// Tracks of a playlist, in playlist order.
    pub fn playlist_tracks(&self, pl: &Playlist) -> Result<Vec<Track>> {
        Ok(self.playlist_at(&Self::playlist_path(pl))?.1)
    }

    /// Playlist from a link: title and tracks in playlist order.
    pub fn linked_playlist(&self, link: &Link) -> Result<(String, Vec<Track>)> {
        match link {
            Link::Playlist { owner, kind } => self.playlist_at(&format!("/users/{owner}/playlists/{kind}")),
            Link::PlaylistUuid(uuid) => self.playlist_at(&format!("/playlist/{uuid}")),
            _ => anyhow::bail!("ссылка не на плейлист"),
        }
    }

    /// Title and tracks of the playlist at an API path.
    fn playlist_at(&self, path: &str) -> Result<(String, Vec<Track>)> {
        let r = self.get(path, &[])?;
        let title = r["title"].as_str().unwrap_or("").to_string();
        let items = r["tracks"].as_array().map(Vec::as_slice).unwrap_or_default();
        let full: Option<Vec<Track>> = items.iter().map(|t| Track::from_json(&t["track"])).collect();
        if let Some(tracks) = full {
            return Ok((title, tracks));
        }
        // Large playlists may come without track objects: resolve them by id.
        let ids: Vec<String> = items.iter().filter_map(item_full_id).collect();
        Ok((title, self.tracks(&ids)?))
    }

    pub fn rename_playlist(&self, pl: &Playlist, title: &str) -> Result<()> {
        self.post_change(&format!("{}/name", Self::playlist_path(pl)), &[("value", title)])?;
        Ok(())
    }

    pub fn delete_playlist(&self, pl: &Playlist) -> Result<()> {
        self.post_change(&format!("{}/delete", Self::playlist_path(pl)), &[])?;
        Ok(())
    }

    /// Applies a track diff; `revision` must match the playlist's current one, otherwise the API refuses.
    fn change_playlist(&self, pl: &Playlist, revision: u64, diff: Value) -> Result<()> {
        let revision = revision.to_string();
        let diff = diff.to_string();
        self.post_form(&format!("{}/change-relative", Self::playlist_path(pl)), &[("revision", &revision), ("diff", &diff)])?;
        Ok(())
    }

    /// Appends a track to the end of a playlist.
    pub fn add_to_playlist(&self, pl: &Playlist, track: &Track) -> Result<()> {
        let r = self.get(&Self::playlist_path(pl), &[])?;
        let revision = r["revision"].as_u64().context("нет ревизии плейлиста")?;
        // Large playlists may come without the full `tracks` list: then `trackCount` tells the end.
        let listed = r["tracks"].as_array().map_or(0, Vec::len);
        let at = listed.max(r["trackCount"].as_u64().unwrap_or(0) as usize);
        let mut t = json!({"id": track.id});
        if let Some(a) = &track.album_id {
            t["albumId"] = json!(a);
        }
        self.change_playlist(pl, revision, json!([{"op": "insert", "at": at, "tracks": [t]}]))
    }

    /// Removes the track at `index`, after checking it is still `track_id` (the playlist may have changed).
    pub fn remove_from_playlist(&self, pl: &Playlist, index: usize, track_id: &str) -> Result<()> {
        let r = self.get(&Self::playlist_path(pl), &[])?;
        let revision = r["revision"].as_u64().context("нет ревизии плейлиста")?;
        let item = &r["tracks"][index];
        let found = id_str(&item["id"]).or_else(|| id_str(&item["track"]["id"]));
        if found.as_deref() != Some(track_id) {
            bail!("плейлист изменился, обновите его (R)");
        }
        self.change_playlist(pl, revision, json!([{"op": "delete", "from": index, "to": index + 1}]))
    }

    // ---------------------------------------------------------------- likes

    /// Full ids (`track:album`) of liked music tracks, newest first, and, from the website API, the
    /// tracks themselves. Liked podcast episodes and audiobook chapters share the same list and are
    /// filtered out by the server.
    pub fn liked(&self) -> Result<(Vec<String>, Option<Vec<Track>>)> {
        if self.web {
            // The website reads «Мне нравится» as the user's playlist 3. Only ids are asked for: with
            // track objects a large library is several megabytes in one answer, and the server drops
            // a slow reader (seen on a Raspberry Pi). Pages are no way out: they repeat some tracks.
            let r = self.get(
                &format!("/users/{}/playlists/3", self.uid),
                &[("trackMetaType", "music"), ("resumeStream", "false"), ("richTracks", "false")],
            )?;
            let ids: Vec<String> = r["tracks"].as_array().into_iter().flatten().filter_map(item_full_id).collect();
            // Tracks removed from the catalog stay in the list there; the mobile API leaves them out.
            let tracks = self.tracks(&ids)?.into_iter().filter(|t| t.available).collect();
            return Ok((ids, Some(tracks)));
        }
        let r = self.get(&format!("/users/{}/likes/tracks", self.uid), &[("trackMetaType", "music")])?;
        Ok((r["library"]["tracks"].as_array().into_iter().flatten().filter_map(item_full_id).collect(), None))
    }

    pub fn set_like(&self, track: &Track, like: bool) -> Result<()> {
        self.set_mark("likes", track, like)
    }

    pub fn set_dislike(&self, track: &Track, dislike: bool) -> Result<()> {
        self.set_mark("dislikes", track, dislike)
    }

    /// Puts or removes a like (`kind` = `likes`) or a dislike (`dislikes`).
    fn set_mark(&self, kind: &str, track: &Track, on: bool) -> Result<()> {
        let id = track.full_id();
        let base = format!("/users/{}/{kind}/tracks", self.uid);
        match (self.web, on) {
            (true, true) => self.post_query(&format!("{base}/add"), &[("track-id", &id)])?,
            (true, false) => self.post_query(&format!("{base}/{id}/remove"), &[])?,
            (false, true) => self.post_form(&format!("{base}/add-multiple"), &[("track-ids", &id)])?,
            (false, false) => self.post_form(&format!("{base}/remove"), &[("track-ids", &id)])?,
        };
        Ok(())
    }

    // ---------------------------------------------------------------- rotor / wave

    pub fn stations(&self) -> Result<Vec<Station>> {
        let r = self.get("/rotor/stations/list", &[("language", "ru")])?;
        Ok(r.as_array()
            .into_iter()
            .flatten()
            .filter_map(|it| {
                let st = &it["station"];
                let ty = st["id"]["type"].as_str()?;
                let tag = st["id"]["tag"].as_str()?;
                Some(Station {
                    id: format!("{ty}:{tag}"),
                    name: st["name"].as_str().unwrap_or(tag).to_string(),
                    category: category_name(ty).to_string(),
                    settings: None,
                })
            })
            .collect())
    }

    /// Changes My Wave settings (mood / character) in the account.
    fn wave_settings(&self, mood: &str, diversity: &str) -> Result<()> {
        self.post_json(
            &format!("/rotor/station/{MY_WAVE}/settings3"),
            &[],
            json!({"moodEnergy": mood, "diversity": diversity, "language": "any", "type": "rotor"}),
        )?;
        Ok(())
    }

    /// POST to a wave session; the website marks it when «Меньше ИИ-музыки» is on.
    fn rotor_req(&self, path: &str) -> ureq::Request {
        let r = self.req("POST", path, &[]);
        if self.web && self.less_ai {
            r.set("X-Yandex-Music-AI-Content-Rate", "reduced")
        } else {
            r
        }
    }

    fn parse_batch(r: &Value, session: Option<String>) -> WaveBatch {
        let batch_id = r["batchId"].as_str().unwrap_or("").to_string();
        let tracks = r["sequence"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| Track::from_json(&s["track"]))
            .map(|mut t| {
                t.batch = Some(batch_id.clone());
                t
            })
            .collect();
        WaveBatch { session_id: session, batch_id, tracks }
    }

    /// Starts a station. My Wave takes its settings (mood, character; `None` — the defaults): the
    /// mobile API keeps them in the account, the website passes them as seeds of the session.
    pub fn wave_start(&self, station: &str, settings: Option<(&str, &str)>) -> Result<WaveBatch> {
        let mut seeds = vec![station.to_string()];
        if station == MY_WAVE {
            let (mood, diversity) = settings.unwrap_or(("all", "default"));
            if !self.web {
                let _ = self.wave_settings(mood, diversity);
            } else {
                if mood != "all" {
                    seeds.push(format!("settingMoodEnergy:{mood}"));
                }
                if diversity != "default" {
                    seeds.push(format!("settingDiversity:{diversity}"));
                }
            }
        }
        let body = json!({"seeds": seeds, "includeTracksInResponse": true, "includeWaveModel": false, "interactive": true});
        let session = self
            .result(|| self.rotor_req("/rotor/session/new").send_json(body.clone()))
            .and_then(|r| {
                let id = r["radioSessionId"].as_str().ok_or_else(|| anyhow!("нет radioSessionId"))?.to_string();
                let b = Self::parse_batch(&r, Some(id));
                if b.tracks.is_empty() {
                    bail!("волна вернула пустой список");
                }
                Ok(b)
            });
        match session {
            Ok(b) => Ok(b),
            Err(e) => self.station_tracks(station, None).map_err(|e2| anyhow!("{e:#}; legacy: {e2:#}")),
        }
    }

    pub fn wave_more(&self, station: &str, session: Option<&str>, queue: &[String]) -> Result<WaveBatch> {
        match session {
            Some(id) => {
                let r = self.result(|| self.rotor_req(&format!("/rotor/session/{id}/tracks")).send_json(json!({"queue": queue})))?;
                Ok(Self::parse_batch(&r, Some(id.to_string())))
            }
            None => self.station_tracks(station, queue.last().map(String::as_str)),
        }
    }

    fn station_tracks(&self, station: &str, queue: Option<&str>) -> Result<WaveBatch> {
        let mut q = vec![("settings2", "true")];
        if let Some(last) = queue {
            q.push(("queue", last));
        }
        let r = self.get(&format!("/rotor/station/{station}/tracks"), &q)?;
        Ok(Self::parse_batch(&r, None))
    }

    /// Sends a wave feedback event (`radioStarted`, `trackStarted`, `trackFinished`, `skip`, `like`, `dislike`...).
    pub fn wave_feedback(&self, station: &str, session: Option<&str>, batch: &str, event: Value) -> Result<()> {
        match session {
            Some(id) => {
                // The website's path ends with a slash.
                let slash = if self.web { "/" } else { "" };
                self.post_json(&format!("/rotor/session/{id}/feedback{slash}"), &[], json!({"event": event, "batchId": batch}))?;
            }
            None => {
                let ty = event["type"].as_str().unwrap_or("");
                if matches!(ty, "radioStarted" | "trackStarted" | "trackFinished" | "skip") {
                    let q: Vec<(&str, &str)> = if batch.is_empty() { vec![] } else { vec![("batch-id", batch)] };
                    self.post_json(&format!("/rotor/station/{station}/feedback"), &q, event)?;
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- listening history

    /// Reports how a track is being listened to (`/plays`): the server keeps it in the listening
    /// history and counts it for recommendations. Both APIs take the same report.
    pub fn send_play(&self, p: &PlayReport) -> Result<()> {
        let now = iso_now();
        let mut play = json!({
            "playId": p.play_id,
            "from": format!("{}-ymusic-{}-default", if self.web { "web" } else { "mobile" }, p.context.kind),
            "trackId": p.track.id,
            "totalPlayedSeconds": round3(p.played),
            "endPositionSeconds": round3(p.end_position),
            "trackLengthSeconds": round3(p.track.duration_ms as f64 / 1000.0),
            "timestamp": now,
            "startTimestamp": p.started,
            "context": p.context.kind,
            "contextItem": p.context.item,
            "fromCache": false,
            "isRestored": false,
            "seek": p.seek,
            "pause": p.pause,
            "maxPlayerStage": "play",
            "isRepeated": false,
        });
        if let Some(a) = &p.track.album_id {
            play["albumId"] = json!(a);
        }
        if p.context.kind == "playlist" {
            play["playlistId"] = json!(p.context.item);
        }
        if let Some(reason) = p.reason {
            play["changeReason"] = json!(reason);
        }
        if let Some((session, batch)) = &p.radio {
            play["radioSessionId"] = json!(session);
            play["batchId"] = json!(batch);
        }
        self.post_json("/plays", &[("clientNow", &now)], json!({"plays": [play]}))?;
        Ok(())
    }

    // ---------------------------------------------------------------- streaming

    /// Downloads and decodes an image (album cover).
    pub fn fetch_image(&self, url: &str) -> Result<image::DynamicImage> {
        use std::io::Read;
        let mut bytes = Vec::new();
        self.agent.get(url).call()?.into_reader().take(10 << 20).read_to_end(&mut bytes)?;
        Ok(image::load_from_memory(&bytes)?)
    }

    /// Stream URL of a track in the given quality (`QUALITIES` id).
    pub fn track_url(&self, track_id: &str, quality: &str) -> Result<String> {
        let low = quality == "lq";
        match self.file_info(track_id, quality) {
            // Uploaded tracks: here the server falls back to 192 kbit/s, the low quality (AAC 64)
            // is listed only by download-info.
            Ok(info) if low && info.quality != "lq" => Ok(self.download_info(track_id, true).map_or(info.url, |i| i.url)),
            Ok(info) => Ok(info.url),
            Err(e) => self.download_info(track_id, low).map(|i| i.url).map_err(|e2| anyhow!("{e:#}; {e2:#}")),
        }
    }

    /// A stream from the older download-info: the best one, or with `low` the smallest AAC.
    fn download_info(&self, track_id: &str, low: bool) -> Result<FileInfo> {
        let r = self.get(&format!("/tracks/{track_id}/download-info"), &[])?;
        let infos = r.as_array().ok_or_else(|| anyhow!("пустой download-info"))?;
        let kbps = |i: &&Value| i["bitrateInKbps"].as_u64().unwrap_or(0);
        let formats = infos.iter().filter(|i| matches!(i["codec"].as_str(), Some("mp3") | Some("aac")));
        let chosen = if low {
            formats.min_by_key(|i| (i["codec"] != "aac", kbps(i)))
        } else {
            formats.max_by_key(|i| (i["codec"] == "mp3", kbps(i)))
        };
        let best = chosen.ok_or_else(|| anyhow!("нет доступных форматов"))?;
        let info_url = best["downloadInfoUrl"].as_str().ok_or_else(|| anyhow!("нет downloadInfoUrl"))?;
        let xml = self.agent.get(info_url).call()?.into_string()?;
        let tag = |name: &str| -> Result<String> {
            let open = format!("<{name}>");
            let close = format!("</{name}>");
            let s = xml.find(&open).ok_or_else(|| anyhow!("нет <{name}> в download-info"))? + open.len();
            let e = xml[s..].find(&close).ok_or_else(|| anyhow!("битый download-info"))? + s;
            Ok(xml[s..e].to_string())
        };
        let (host, path, ts, s) = (tag("host")?, tag("path")?, tag("ts")?, tag("s")?);
        let sign = Md5::digest(format!("{MD5_SALT}{}{s}", path.trim_start_matches('/')).as_bytes());
        let sign: String = sign.iter().map(|b| format!("{b:02x}")).collect();
        Ok(FileInfo { url: format!("https://{host}/get-mp3/{sign}/{ts}{path}"), quality: String::new(), bitrate: kbps(&best) })
    }

    fn file_info(&self, track_id: &str, quality: &str) -> Result<FileInfo> {
        let id = track_id.split(':').next().unwrap_or(track_id);
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
        // Low quality exists only in mp4 containers: without `*-mp4` codecs the server falls back to 192 kbit/s.
        let (codecs, transports) = ("flac,flac-mp4,aac,he-aac,mp3,aac-mp4,he-aac-mp4", "raw");
        let key = if self.web { WEB_FILE_INFO_KEY } else { FILE_INFO_KEY };
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes())?;
        mac.update(format!("{ts}{id}{quality}{}{transports}", codecs.replace(',', "")).as_bytes());
        let sign = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let sign = sign.trim_end_matches('=');
        let r = self.get(
            "/get-file-info",
            &[("ts", &ts), ("trackId", id), ("quality", quality), ("codecs", codecs), ("transports", transports), ("sign", sign)],
        )?;
        let di = &r["downloadInfo"];
        let url = di["url"].as_str().or(di["urls"][0].as_str()).context("get-file-info не вернул ссылку")?;
        Ok(FileInfo {
            url: url.to_string(),
            quality: di["quality"].as_str().unwrap_or_default().to_string(),
            bitrate: di["bitrate"].as_u64().unwrap_or(0),
        })
    }
}

struct FileInfo {
    url: String,
    /// The quality the server gave, which may differ from the one asked for.
    quality: String,
    #[cfg_attr(not(test), allow(dead_code))]
    bitrate: u64,
}

pub fn category_name(ty: &str) -> &str {
    match ty {
        "user" => "Моя волна",
        "genre" => "Жанры",
        "mood" => "Настроение",
        "activity" => "Занятия",
        "epoch" => "Эпохи",
        "local" => "Местное",
        "author" => "Авторы",
        _ => "Другое",
    }
}

/// Built-in stations shown before (or instead of) the list from the API.
/// «Моя волна» with the default settings.
pub fn my_wave() -> Station {
    Station { id: MY_WAVE.into(), name: "Моя волна".into(), category: "Моя волна".into(), settings: None }
}

pub fn default_stations() -> Vec<Station> {
    let wave = |name: &str, s: Option<(&'static str, &'static str)>| Station {
        id: MY_WAVE.into(),
        name: name.into(),
        category: "Моя волна".into(),
        settings: s,
    };
    let mut v = vec![
        my_wave(),
        wave("Моя волна · Любимое", Some(("all", "favorite"))),
        wave("Моя волна · Незнакомое", Some(("all", "discover"))),
        wave("Моя волна · Популярное", Some(("all", "popular"))),
        wave("Моя волна · Бодрое", Some(("active", "default"))),
        wave("Моя волна · Весёлое", Some(("fun", "default"))),
        wave("Моя волна · Спокойное", Some(("calm", "default"))),
        wave("Моя волна · Грустное", Some(("sad", "default"))),
    ];
    let items: &[(&str, &str)] = &[
        ("genre:rock", "Рок"),
        ("genre:metal", "Метал"),
        ("genre:rusrock", "Русский рок"),
        ("genre:alternative", "Альтернатива"),
        ("genre:indie", "Инди"),
        ("genre:punk", "Панк"),
        ("genre:pop", "Поп"),
        ("genre:ruspop", "Русская поп-музыка"),
        ("genre:electronics", "Электроника"),
        ("genre:dance", "Танцевальная"),
        ("genre:rap", "Рэп и хип-хоп"),
        ("genre:rusrap", "Русский рэп"),
        ("genre:jazz", "Джаз"),
        ("genre:blues", "Блюз"),
        ("genre:classicalmusic", "Классическая музыка"),
        ("genre:soundtrack", "Саундтреки"),
        ("genre:lounge", "Лаунж"),
        ("mood:happy", "Весёлое"),
        ("mood:sad", "Грустное"),
        ("mood:calm", "Спокойное"),
        ("mood:energetic", "Бодрое"),
        ("mood:beautiful", "Красивое"),
        ("mood:dream", "Мечтательное"),
        ("mood:aggressive", "Агрессивное"),
        ("activity:workout", "Спорт"),
        ("activity:run", "Бег"),
        ("activity:party", "Вечеринка"),
        ("activity:work-background", "Работа"),
        ("activity:road-trip", "В дороге"),
        ("activity:sleep", "Сон"),
        ("activity:beloved", "Любовь"),
        ("epoch:the-greatest-hits", "Вечные хиты"),
        ("epoch:fifties", "50-е"),
        ("epoch:sixties", "60-е"),
        ("epoch:seventies", "70-е"),
        ("epoch:eighties", "80-е"),
        ("epoch:nineties", "90-е"),
        ("epoch:zeroes", "00-е"),
    ];
    for (id, name) in items {
        let ty = id.split(':').next().unwrap();
        v.push(Station { id: (*id).into(), name: (*name).into(), category: category_name(ty).into(), settings: None });
    }
    v
}

/// Current time as ISO-8601 in UTC (for rotor feedback).
/// Seconds as the website sends them: at most three decimals.
fn round3(s: f64) -> f64 {
    (s * 1000.0).round() / 1000.0
}

/// Where the playing list comes from, as the listening history names it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayContext {
    /// `radio`, `playlist`, `album`, `artist`, `search` or `various` (anything else).
    pub kind: &'static str,
    /// Station, `owner:kind` of a playlist, album or artist id, search text.
    pub item: String,
}

impl PlayContext {
    pub fn new(kind: &'static str, item: impl Into<String>) -> PlayContext {
        PlayContext { kind, item: item.into() }
    }
}

/// One report of a track's playback (see `Api::send_play`).
#[derive(Clone, Debug)]
pub struct PlayReport {
    /// Same for all reports about one playback of the track.
    pub play_id: String,
    pub track: Track,
    pub context: PlayContext,
    /// Wave session and batch the track came from.
    pub radio: Option<(String, String)>,
    /// When the playback started (ISO-8601).
    pub started: String,
    /// Seconds actually played (not counting seeks) and the position now.
    pub played: f64,
    pub end_position: f64,
    /// The track was sought / paused at some point.
    pub seek: bool,
    pub pause: bool,
    /// Why it ended: `finish`, `skip`, `back_skip`, `dislike`, `other_track`; `in_progress` for a
    /// report while it plays; `None` for the start and for a pause.
    pub reason: Option<&'static str>,
}

/// A random id in the UUID v4 format.
pub fn random_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut x = nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(std::process::id());
    let mut next = || {
        // splitmix64
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&next().to_be_bytes());
    bytes[8..].copy_from_slice(&next().to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn iso_now() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_format() {
        let s = iso_now();
        assert_eq!(s.len(), 24);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn track_parsing() {
        let v = json!({"id": 123, "title": "Song", "version": "Remix",
            "artists": [{"name": "A"}, {"name": "B"}], "albums": [{"id": 9, "title": "Alb"}], "durationMs": 1000});
        let t = Track::from_json(&v).unwrap();
        assert_eq!(t.full_id(), "123:9");
        assert_eq!(t.web_url(), "https://music.yandex.ru/album/9/track/123");
        assert_eq!(t.title, "Song (Remix)");
        assert_eq!(t.artists, "A, B");
        assert_eq!(t.main_artist, None);
        let v = json!({"id": 1, "artists": [{"id": 42, "name": "A"}, {"id": 7, "name": "B"}]});
        assert_eq!(Track::from_json(&v).unwrap().main_artist, Some(("42".into(), "A".into())));
        assert_eq!(t.cover, None);
        assert_eq!(cover_url("avatars.yandex.net/x/%%"), "https://avatars.yandex.net/x/400x400");
    }

    /// Token for network tests: `YMUSIC_TOKEN`, otherwise the one saved in the config.
    fn test_token() -> String {
        crate::config::env_str("TOKEN").or_else(|| crate::config::Config::load().token).expect("token")
    }

    /// Signed-in clients of both APIs for network tests: mobile, then website.
    fn apis() -> [Api; 2] {
        let token = test_token();
        [false, true].map(|web| {
            let mut api = Api::new(&token);
            api.web = web;
            api.account_status().unwrap();
            api
        })
    }

    /// Network test: needs a token (see `test_token`). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn artist_tracks() {
        for api in apis() {
            let tracks = api.artist_tracks("161243").unwrap();
            assert!(tracks.len() > 100, "web={}: {}", api.web, tracks.len());
            assert!(tracks.iter().all(|t| t.main_artist.is_some()));
        }
    }

    /// Network test: needs a token (see `test_token`). Both APIs know the same account.
    #[test]
    #[ignore]
    fn accounts() {
        let [mobile, web] = apis();
        let token = test_token();
        let (mut m, mut w) = (Api::new(&token), Api::new(&token));
        w.web = true;
        let (a, b) = (m.account_status().unwrap(), w.account_status().unwrap());
        assert_eq!((a.uid, a.login, a.has_plus), (b.uid, b.login, b.has_plus));
        assert_eq!(mobile.uid, web.uid);
    }

    /// Network test: needs a token (see `test_token`). Only reads the likes.
    #[test]
    #[ignore]
    fn liked() {
        for api in apis() {
            let (ids, tracks) = api.liked().unwrap();
            assert!(!ids.is_empty());
            let tracks = tracks.map_or_else(|| api.tracks(&ids), Ok).unwrap();
            assert!(tracks.len() * 10 > ids.len() * 8, "web={}: {} of {}", api.web, tracks.len(), ids.len());
        }
    }

    /// Network test: WRITES to the account — likes and dislikes a track, then takes both back.
    /// Run explicitly: `cargo test like_edit -- --ignored`.
    #[test]
    #[ignore]
    fn like_edit() {
        let ids = |api: &Api, kind: &str| -> Vec<String> {
            let r = api.get(&format!("/users/{}/{kind}/tracks", api.uid), &[]).unwrap();
            r["library"]["tracks"].as_array().unwrap().iter().filter_map(|t| id_str(&t["id"])).collect()
        };
        for api in apis() {
            // A catalog track that is neither liked nor disliked.
            let t = Track::from_json(&json!({"id": "39089632", "albums": [{"id": 60058}]})).unwrap();
            assert!(!ids(&api, "likes").contains(&t.id) && !ids(&api, "dislikes").contains(&t.id), "{} отмечен", t.id);
            api.set_like(&t, true).unwrap();
            let liked = ids(&api, "likes").contains(&t.id);
            api.set_like(&t, false).unwrap();
            assert!(liked && !ids(&api, "likes").contains(&t.id), "web={}", api.web);
            api.set_dislike(&t, true).unwrap();
            let disliked = ids(&api, "dislikes").contains(&t.id);
            api.set_dislike(&t, false).unwrap();
            assert!(disliked && !ids(&api, "dislikes").contains(&t.id), "web={}", api.web);
        }
    }

    /// Network test: needs a token (see `test_token`). Starts wave sessions (no feedback is sent).
    #[test]
    #[ignore]
    fn waves() {
        for api in apis() {
            let b = api.wave_start(MY_WAVE, Some(("calm", "favorite"))).unwrap();
            assert!(b.session_id.is_some() && !b.tracks.is_empty(), "web={}", api.web);
            let queue: Vec<String> = b.tracks.iter().map(Track::full_id).collect();
            let more = api.wave_more(MY_WAVE, b.session_id.as_deref(), &queue).unwrap();
            assert!(!more.tracks.is_empty(), "web={}", api.web);
            assert!(!api.wave_start("genre:rock", None).unwrap().tracks.is_empty());
            assert!(api.stations().unwrap().len() > 10);
        }
        // Back to the default settings of My Wave in the account (the mobile API keeps them there).
        apis()[0].wave_settings("all", "default").unwrap();
    }

    #[test]
    fn parallel_keeps_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let items: Vec<usize> = (0..23).collect();
        let (now, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let out = super::parallel(&items, |&i| {
            peak.fetch_max(now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(((i * 7) % 5) as u64));
            now.fetch_sub(1, Ordering::SeqCst);
            Ok(i * 10)
        })
        .unwrap();
        assert_eq!(out, items.iter().map(|i| i * 10).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) <= super::PARALLEL);

        let err = super::parallel(&items, |&i| if i == 5 || i == 17 { Err(anyhow!("bad {i}")) } else { Ok(i) });
        assert_eq!(err.unwrap_err().to_string(), "bad 5");
        assert_eq!(super::parallel(&[] as &[usize], |&i| Ok(i)).unwrap(), Vec::<usize>::new());
        assert_eq!(super::parallel(&[7], |&i| Ok(i)).unwrap(), [7]);
    }

    #[test]
    fn links() {
        let not_ym = Err("Это не ссылка на Яндекс Музыку");
        let other = Err("Ссылка не на трек, альбом или плейлист");
        assert_eq!(parse_link("https://music.yandex.ru/album/9/track/123?utm_source=x&utm_medium=y"), Ok(Link::Track("123:9".into())));
        assert_eq!(parse_link("  music.yandex.com/track/123/#top "), Ok(Link::Track("123".into())));
        assert_eq!(parse_link("http://MUSIC.yandex.kz/album/9/track/123"), Ok(Link::Track("123:9".into())));
        assert_eq!(parse_link("https://music.yandex.ru/album/9?activeTab=about"), Ok(Link::Album("9".into())));
        assert_eq!(
            parse_link("https://music.yandex.ru/users/some.user-1/playlists/1002?utm_source=x"),
            Ok(Link::Playlist { owner: "some.user-1".into(), kind: "1002".into() })
        );
        let uuid = "e46028ae-ce60-430d-bf2c-7e7c78e2c682";
        assert_eq!(parse_link(&format!("https://music.yandex.ru/playlists/{uuid}")), Ok(Link::PlaylistUuid(uuid.into())));
        assert_eq!(parse_link(&format!("https://music.yandex.ru/playlists/lk.{uuid}?x=1")), Ok(Link::PlaylistUuid(uuid.into())));
        assert_eq!(parse_link(&format!("https://music.yandex.ru/track/{uuid}")), Ok(Link::Track(uuid.into())));
        assert_eq!(parse_link("https://music.yandex.ru.evil.com/track/1"), not_ym);
        assert_eq!(parse_link("https://evil.com/?r=music.yandex.ru/track/1"), not_ym);
        assert_eq!(parse_link("https://user@music.yandex.ru/track/1"), not_ym);
        assert_eq!(parse_link("https://yandex.ru/track/1"), not_ym);
        assert_eq!(parse_link(""), not_ym);
        assert_eq!(parse_link("https://music.yandex.ru/artist/9"), other);
        assert_eq!(parse_link("https://music.yandex.ru/track/12a"), other);
        assert_eq!(parse_link("https://music.yandex.ru/users/a/playlists/x"), other);
        assert_eq!(parse_link("https://music.yandex.ru/playlists/../../x"), other);
    }

    /// Network test: needs a token (see `test_token`). Reads a playlist of the token's account.
    #[test]
    #[ignore]
    fn linked_playlists() {
        for api in apis() {
            linked_playlists_with(&api);
        }
    }

    fn linked_playlists_with(api: &Api) {
        let r = api.get(&format!("/users/{}/playlists/list", api.uid), &[]).unwrap();
        let p = r.as_array().unwrap().iter().find(|p| p["trackCount"].as_u64() > Some(0)).expect("non-empty playlist");
        let login = r[0]["owner"]["login"].as_str().unwrap().to_string();
        let by_login = Link::Playlist { owner: login, kind: id_str(&p["kind"]).unwrap() };
        let by_uuid = Link::PlaylistUuid(p["playlistUuid"].as_str().unwrap().into());
        let (t1, a) = api.linked_playlist(&by_login).unwrap();
        let (t2, b) = api.linked_playlist(&by_uuid).unwrap();
        assert_eq!(t1, p["title"].as_str().unwrap());
        assert_eq!(t1, t2);
        assert!(!a.is_empty());
        assert_eq!(a.len(), b.len());
    }

    /// Network test: needs a token (see `test_token`).
    #[test]
    #[ignore]
    fn album_tracks() {
        for api in apis() {
            let (title, tracks) = api.album_tracks("3782599").unwrap();
            assert!(!title.is_empty());
            assert!(tracks.len() > 1, "{}", tracks.len());
            assert!(tracks.iter().all(|t| t.album_id.as_deref() == Some("3782599")));
        }
    }

    /// Network test: needs a token (see `test_token`). Only reads the setting.
    #[test]
    #[ignore]
    fn ai_reduction() {
        let [mobile, web] = apis();
        assert_eq!(mobile.ai_reduction().unwrap(), web.ai_reduction().unwrap());
    }

    /// Network test: needs a token (see `test_token`). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn search_pages() {
        for api in apis() {
            let (first, total) = api.search("сплин", 0).unwrap();
            assert_eq!(first.len(), SEARCH_PAGE);
            // The website API does not count the results.
            assert!(if api.web { total.is_none() } else { total > Some(2 * SEARCH_PAGE) }, "{total:?}");
            let (second, _) = api.search("сплин", 1).unwrap();
            assert!(!second.is_empty());
            assert_ne!(first[0].id, second[0].id);
        }
        // The last page tells the total.
        let (tracks, total) = apis()[1].search("сплин", 50).unwrap();
        assert_eq!(total, Some(50 * SEARCH_PAGE + tracks.len()));
    }

    /// Network test: needs a token (see `test_token`). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn playlists() {
        for api in apis() {
            let lists = api.playlists().unwrap();
            assert!(!lists.is_empty());
            let pl = lists.iter().find(|p| p.track_count > 0).expect("non-empty playlist");
            assert_eq!(api.playlist_tracks(pl).unwrap().len() as u64, pl.track_count);
        }
    }

    /// Network test: WRITES to the account — creates a temporary private playlist, edits it and deletes it.
    /// Run explicitly: `cargo test playlist_edit -- --ignored`.
    #[test]
    #[ignore]
    fn playlist_edit() {
        for api in apis() {
            playlist_edit_with(&api);
        }
    }

    fn playlist_edit_with(api: &Api) {
        let path = format!("/users/{}/playlists/create", api.uid);
        let form = [("title", "ymusic-test"), ("visibility", "private")];
        let r = api.post_change(&path, &form);
        let kind = id_str(&r.unwrap()["kind"]).expect("kind");
        let pl = Playlist { owner: api.uid.clone(), kind, title: "ymusic-test".into(), track_count: 0 };
        // Delete the playlist whatever happens below.
        struct Cleanup<'a>(&'a Api, Playlist);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                if self.0.playlists().is_ok_and(|l| l.iter().any(|p| p.key() == self.1.key())) {
                    let _ = self.0.delete_playlist(&self.1);
                }
            }
        }
        let _cleanup = Cleanup(api, pl.clone());

        api.rename_playlist(&pl, "ymusic-test 2").unwrap();
        assert!(api.playlists().unwrap().iter().any(|p| p.key() == pl.key() && p.title == "ymusic-test 2"));

        let catalog = Track::from_json(&json!({"id": "39089632", "albums": [{"id": 60058}]})).unwrap();
        let ugc = Track::from_json(&json!({"id": "bfc384d6-bd20-47e0-89d7-7a8657a2236a"})).unwrap();
        api.add_to_playlist(&pl, &catalog).unwrap();
        api.add_to_playlist(&pl, &ugc).unwrap();
        let ids = |api: &Api| api.playlist_tracks(&pl).unwrap().into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(api), ["39089632", "bfc384d6-bd20-47e0-89d7-7a8657a2236a"]);

        // A stale view (wrong track at the position) is refused and nothing changes.
        assert!(api.remove_from_playlist(&pl, 0, &ugc.id).is_err());
        assert_eq!(ids(api).len(), 2);
        api.remove_from_playlist(&pl, 0, &catalog.id).unwrap();
        assert_eq!(ids(api), ["bfc384d6-bd20-47e0-89d7-7a8657a2236a"]);

        api.delete_playlist(&pl).unwrap();
        assert!(!api.playlists().unwrap().iter().any(|p| p.key() == pl.key()));
    }

    /// Network test: needs a token (see `test_token`). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn qualities() {
        for api in apis() {
            qualities_with(&api);
        }
    }

    fn qualities_with(api: &Api) {
        let lq = api.file_info("39089632", "lq").unwrap();
        let nq = api.file_info("39089632", "nq").unwrap();
        assert!(lq.bitrate > 0 && lq.bitrate < nq.bitrate, "{} {}", lq.bitrate, nq.bitrate);
        assert!(api.track_url("39089632:60058", "lq").unwrap().starts_with("https://"));
        // Uploaded (UGC) tracks resolve too; their low quality comes from download-info.
        let ugc = "bfc384d6-bd20-47e0-89d7-7a8657a2236a";
        assert!(api.track_url(ugc, "lq").unwrap().starts_with("https://"));
        assert_eq!(api.file_info(ugc, "lq").unwrap().quality, "nq", "the server still falls back");
        let low = api.download_info(ugc, true).unwrap();
        assert!(low.bitrate > 0 && low.bitrate < 192, "{}", low.bitrate);
        assert_eq!(api.download_info(ugc, false).unwrap().bitrate, 192);
        let size = |q: &str| -> u64 {
            let r = api.agent.get(&api.track_url(ugc, q).unwrap()).call().unwrap();
            r.header("Content-Length").and_then(|l| l.parse().ok()).unwrap_or(0)
        };
        let (lq, nq) = (size("lq"), size("nq"));
        assert!(lq > 0 && lq * 2 < nq, "{lq} {nq}");
    }

    /// Network test: WRITES to the listening history — reports the start of a track from both APIs.
    /// Run explicitly: `cargo test send_plays -- --ignored`.
    #[test]
    #[ignore]
    fn send_plays() {
        for api in apis() {
            let track = api.tracks(&["39089632:60058".to_string()]).unwrap().remove(0);
            let report = PlayReport {
                play_id: random_uuid(),
                track,
                context: PlayContext::new("album", "60058"),
                radio: None,
                started: iso_now(),
                played: 0.0,
                end_position: 0.0,
                seek: false,
                pause: false,
                reason: None,
            };
            api.send_play(&report).unwrap();
        }
    }

    #[test]
    fn uuids() {
        let (a, b) = (random_uuid(), random_uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!("89ab".contains(&a[19..20]), "{a}");
        assert_eq!(a.matches('-').count(), 4);
    }

    /// Network test: downloads and decodes a real image from Yandex CDN.
    #[test]
    #[ignore]
    fn cover_download() {
        let img = Api::new("x").fetch_image(&cover_url("avatars.yandex.net/get-music-misc/34161/rotor-genre-pop-icon/%%")).unwrap();
        assert!(img.width() > 0);
    }
}
