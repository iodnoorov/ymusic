//! Minimal client for the (unofficial) Yandex Music API.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// OAuth application of the official Yandex Music Android client.
pub const CLIENT_ID: &str = "23cabbbdc6cd418abb4b39c32c41195d";

const BASE: &str = "https://api.music.yandex.net";
const MD5_SALT: &str = "XGRlBW9FXlekgbPrRHuSiA";
const FILE_INFO_KEY: &str = "p93jhgh689SBReK6ghtw62";
pub const MY_WAVE: &str = "user:onyourwave";

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

#[derive(Clone, Debug)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub category: String,
    /// My Wave settings: (moodEnergy, diversity)
    pub settings: Option<(&'static str, &'static str)>,
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
                        .map(String::from)
                })
                .unwrap_or_else(|| body.chars().take(200).collect());
            if code == 401 {
                bail!("HTTP 401: токен недействителен, выполните `yamusic login` ({msg})");
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
            .build();
        Api { agent, token: token.to_string(), uid: String::new() }
    }

    fn req(&self, method: &str, path: &str) -> ureq::Request {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{BASE}{path}") };
        self.agent
            .request(method, &url)
            .set("Authorization", &format!("OAuth {}", self.token))
            .set("X-Yandex-Music-Client", "YandexMusicAndroid/24023621")
            .set("Accept-Language", "ru")
    }

    fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let mut r = self.req("GET", path);
        for (k, v) in query {
            r = r.query(k, v);
        }
        Ok(handle(r.call())?["result"].take())
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<Value> {
        Ok(handle(self.req("POST", path).send_form(form))?["result"].take())
    }

    fn post_json(&self, path: &str, query: &[(&str, &str)], body: Value) -> Result<Value> {
        let mut r = self.req("POST", path);
        for (k, v) in query {
            r = r.query(k, v);
        }
        Ok(handle(r.send_json(body))?["result"].take())
    }

    // ---------------------------------------------------------------- account

    pub fn account_status(&mut self) -> Result<Account> {
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

    // ---------------------------------------------------------------- tracks

    pub fn tracks(&self, ids: &[String]) -> Result<Vec<Track>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(200) {
            let joined = chunk.join(",");
            let r = self.post_form("/tracks", &[("track-ids", &joined), ("with-positions", "false")])?;
            out.extend(r.as_array().into_iter().flatten().filter_map(Track::from_json));
        }
        Ok(out)
    }

    pub fn search(&self, text: &str) -> Result<Vec<Track>> {
        let r = self.get("/search", &[("text", text), ("type", "track"), ("page", "0"), ("nocorrect", "false")])?;
        Ok(r["tracks"]["results"].as_array().into_iter().flatten().filter_map(Track::from_json).collect())
    }

    /// Tracks of an artist, most popular first.
    pub fn artist_tracks(&self, artist_id: &str) -> Result<Vec<Track>> {
        const PAGE: usize = 100;
        const MAX: usize = 1000;
        let mut out = Vec::new();
        for page in 0.. {
            let page = page.to_string();
            let size = PAGE.to_string();
            let r = self.get(&format!("/artists/{artist_id}/tracks"), &[("page", &page), ("page-size", &size)])?;
            let tracks = r["tracks"].as_array().map(Vec::as_slice).unwrap_or_default();
            out.extend(tracks.iter().filter_map(Track::from_json));
            let total = r["pager"]["total"].as_u64().unwrap_or(0) as usize;
            if tracks.len() < PAGE || out.len() >= total.min(MAX) {
                break;
            }
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- likes

    /// Full ids (`track:album`) of liked tracks, newest first.
    pub fn liked_ids(&self) -> Result<Vec<String>> {
        let r = self.get(&format!("/users/{}/likes/tracks", self.uid), &[])?;
        Ok(r["library"]["tracks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                let id = id_str(&t["id"])?;
                Some(match id_str(&t["albumId"]) {
                    Some(a) => format!("{id}:{a}"),
                    None => id,
                })
            })
            .collect())
    }

    pub fn set_like(&self, track: &Track, like: bool) -> Result<()> {
        let action = if like { "add-multiple" } else { "remove" };
        self.post_form(&format!("/users/{}/likes/tracks/{action}", self.uid), &[("track-ids", &track.full_id())])?;
        Ok(())
    }

    pub fn set_dislike(&self, track: &Track, dislike: bool) -> Result<()> {
        let action = if dislike { "add-multiple" } else { "remove" };
        self.post_form(&format!("/users/{}/dislikes/tracks/{action}", self.uid), &[("track-ids", &track.full_id())])?;
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

    /// Changes My Wave settings (mood / character).
    pub fn wave_settings(&self, mood: &str, diversity: &str) -> Result<()> {
        self.post_json(
            &format!("/rotor/station/{MY_WAVE}/settings3"),
            &[],
            json!({"moodEnergy": mood, "diversity": diversity, "language": "any", "type": "rotor"}),
        )?;
        Ok(())
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

    pub fn wave_start(&self, station: &str) -> Result<WaveBatch> {
        let session = self
            .post_json(
                "/rotor/session/new",
                &[],
                json!({"seeds": [station], "includeTracksInResponse": true, "includeWaveModel": false, "interactive": true}),
            )
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
                let r = self.post_json(&format!("/rotor/session/{id}/tracks"), &[], json!({"queue": queue}))?;
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
                self.post_json(&format!("/rotor/session/{id}/feedback"), &[], json!({"event": event, "batchId": batch}))?;
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

    // ---------------------------------------------------------------- streaming

    /// Downloads and decodes an image (album cover).
    pub fn fetch_image(&self, url: &str) -> Result<image::DynamicImage> {
        use std::io::Read;
        let mut bytes = Vec::new();
        self.agent.get(url).call()?.into_reader().take(10 << 20).read_to_end(&mut bytes)?;
        Ok(image::load_from_memory(&bytes)?)
    }

    pub fn track_url(&self, track_id: &str) -> Result<String> {
        match self.download_info_url(track_id) {
            Ok(u) => Ok(u),
            Err(e) => self.file_info_url(track_id).map_err(|e2| anyhow!("{e:#}; {e2:#}")),
        }
    }

    fn download_info_url(&self, track_id: &str) -> Result<String> {
        let r = self.get(&format!("/tracks/{track_id}/download-info"), &[])?;
        let infos = r.as_array().ok_or_else(|| anyhow!("пустой download-info"))?;
        let best = infos
            .iter()
            .filter(|i| matches!(i["codec"].as_str(), Some("mp3") | Some("aac")))
            .max_by_key(|i| (i["codec"] == "mp3", i["bitrateInKbps"].as_u64().unwrap_or(0)))
            .ok_or_else(|| anyhow!("нет доступных форматов"))?;
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
        Ok(format!("https://{host}/get-mp3/{sign}/{ts}{path}"))
    }

    fn file_info_url(&self, track_id: &str) -> Result<String> {
        let id = track_id.split(':').next().unwrap_or(track_id);
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string();
        let (quality, codecs, transports) = ("lossless", "flac,aac,he-aac,mp3", "raw");
        let mut mac = Hmac::<Sha256>::new_from_slice(FILE_INFO_KEY.as_bytes())?;
        mac.update(format!("{ts}{id}{quality}{}{transports}", codecs.replace(',', "")).as_bytes());
        let sign = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let sign = sign.trim_end_matches('=');
        let r = self.get(
            "/get-file-info",
            &[("ts", &ts), ("trackId", id), ("quality", quality), ("codecs", codecs), ("transports", transports), ("sign", sign)],
        )?;
        let di = &r["downloadInfo"];
        di["url"]
            .as_str()
            .or(di["urls"][0].as_str())
            .map(String::from)
            .context("get-file-info не вернул ссылку")
    }
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
pub fn default_stations() -> Vec<Station> {
    let wave = |name: &str, s: Option<(&'static str, &'static str)>| Station {
        id: MY_WAVE.into(),
        name: name.into(),
        category: "Моя волна".into(),
        settings: s,
    };
    let mut v = vec![
        wave("Моя волна", None),
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

    /// Network test: needs a token in the config. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn artist_tracks() {
        let token = crate::config::Config::load().token.expect("token");
        let tracks = Api::new(&token).artist_tracks("161243").unwrap();
        assert!(tracks.len() > 100, "{}", tracks.len());
        assert!(tracks.iter().all(|t| t.main_artist.is_some()));
    }

    /// Network test: downloads and decodes a real image from Yandex CDN.
    #[test]
    #[ignore]
    fn cover_download() {
        let img = Api::new("x").fetch_image(&cover_url("avatars.yandex.net/get-music-misc/34161/rotor-genre-pop-icon/%%")).unwrap();
        assert!(img.width() > 0);
    }
}
