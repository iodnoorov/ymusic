use super::{cover_url, id_str, Api, Track, SEARCH_PAGE};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Music,
    Podcast,
    Book,
    Other,
}

impl TrackKind {
    pub(super) fn from_json(v: &Value) -> Self {
        let album = &v["albums"][0];
        let track_type = v["type"].as_str().unwrap_or("");
        let album_type = album["type"].as_str().unwrap_or("");
        if matches!(track_type, "audiobook" | "audiobook-chapter" | "book" | "book-chapter")
            || matches!(album_type, "audiobook" | "book")
        {
            Self::Book
        } else if track_type == "podcast-episode" || album_type == "podcast" {
            Self::Podcast
        } else if v["metaType"] == "podcast" || album["metaType"] == "podcast" {
            // Non-music without a concrete type is not necessarily a podcast (books share metaType).
            Self::Other
        } else if matches!(track_type, "" | "music") {
            Self::Music
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamProgress {
    pub position: f64,
    pub finished: bool,
}

impl StreamProgress {
    pub(super) fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            position: v["endPositionSec"].as_f64()?,
            finished: v["everFinished"].as_bool().unwrap_or(false),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Podcast {
    pub id: String,
    pub title: String,
    pub description: String,
    pub cover: Option<String>,
    pub episodes: usize,
}

impl Podcast {
    fn from_json(v: &Value) -> Option<Self> {
        if v["type"] != "podcast" {
            return None;
        }
        Some(Self {
            id: id_str(&v["id"] )?,
            title: v["title"].as_str().unwrap_or("").to_string(),
            description: description_text(v["description"].as_str().or(v["shortDescription"].as_str()).unwrap_or("")),
            cover: v["coverUri"].as_str().or(v["cover"]["uri"].as_str()).filter(|s| !s.is_empty()).map(cover_url),
            episodes: v["trackCount"].as_u64().unwrap_or(0) as usize,
        })
    }
}

/// Strip markup before decoding entities so escaped literal tags stay visible.
fn description_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        rest = &rest[start..];
        if rest.starts_with("<!--") && let Some(end) = rest.find("-->") {
            rest = &rest[end + 3..];
            continue;
        }
        if let Some((end, name)) = description_tag(rest) {
            let breaks = if ["p", "div", "section", "article", "blockquote",
                "h1", "h2", "h3", "h4", "h5", "h6"].iter().any(|tag| name.eq_ignore_ascii_case(tag)) {
                2
            } else if ["br", "li", "ul", "ol", "tr"].iter().any(|tag| name.eq_ignore_ascii_case(tag)) {
                1
            } else {
                0
            };
            if !text.is_empty() && breaks > 0 {
                let existing = text.chars().rev().take_while(|&c| c == '\n').count();
                for _ in existing..breaks {
                    text.push('\n');
                }
            }
            rest = &rest[end..];
        } else {
            text.push('<');
            rest = &rest[1..];
        }
    }
    text.push_str(rest);
    let decoded = html_escape::decode_html_entities(&text);
    let mut plain = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    while let Some(c) = chars.next() {
        let c = match c {
            '\r' => { let _ = chars.next_if_eq(&'\n'); '\n' }
            '\n' | '\t' => c,
            _ if c.is_control() => continue,
            _ => c,
        };
        if !plain.is_empty() || !c.is_whitespace() { plain.push(c); }
    }
    plain.truncate(plain.trim_end().len());
    plain
}

/// Recognize only actual tag-shaped input, including quoted attribute delimiters.
fn description_tag(input: &str) -> Option<(usize, &str)> {
    let bytes = input.as_bytes();
    let start = if bytes.get(1) == Some(&b'/') { 2 } else { 1 };
    if !bytes.get(start)?.is_ascii_alphabetic() {
        return None;
    }
    let mut end = start;
    while bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-') {
        end += 1;
    }
    if !bytes.get(end).is_some_and(|b| b.is_ascii_whitespace() || *b == b'/' || *b == b'>') {
        return None;
    }
    let name = &input[start..end];
    let mut quote = None;
    for (offset, &byte) in bytes[end..].iter().enumerate() {
        match (quote, byte) {
            (Some(q), b) if q == b => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some((end + offset + 1, name)),
            _ => {}
        }
    }
    None
}

fn canonical_progress(progress: Option<StreamProgress>) -> Result<StreamProgress> {
    progress.context("свежий ответ сервера не содержит streamProgress; прогресс не подтверждён")
}

/// A durable progress event. Its timestamp belongs to the event, not to a retry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PodcastProgress {
    pub track_id: String,
    pub position: f64,
    pub length: f64,
    pub finished: bool,
    pub timestamp: String,
}

impl Api {
    pub fn podcast_liked(&self) -> Result<Vec<Podcast>> {
        let base = format!("/users/{}/likes/albums", self.uid);
        if !self.web {
            let r = self.get(&base, &[("rich", "true")])?;
            return Ok(r.as_array().into_iter().flatten().filter_map(|v| Podcast::from_json(&v["album"])).collect());
        }
        let mut out = Vec::new();
        let mut page = 0usize;
        loop {
            let r = self.get(&format!("{base}/page"), &[
                ("rich", "true"), ("page", &page.to_string()), ("pageSize", "100"), ("metaType", "podcast"),
            ])?;
            let items = r["albums"].as_array().context("нет списка подкастов в ответе")?;
            out.extend(items.iter().filter_map(|v| Podcast::from_json(&v["album"])));
            let total = r["pager"]["total"].as_u64().context("нет числа подкастов в ответе")? as usize;
            if items.is_empty() || (page + 1) * 100 >= total {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    pub fn podcast_new(&self) -> Result<Vec<Podcast>> {
        let r = self.get("/non-music/new-episodes", &[])?;
        let mut seen = HashSet::new();
        Ok(r["blocks"].as_array().into_iter().flatten()
            .flat_map(|b| b["entities"].as_array().into_iter().flatten())
            .filter_map(|e| Podcast::from_json(&e["album"]))
            .filter(|p| seen.insert(p.id.clone())).collect())
    }

    pub fn podcast_search(&self, text: &str, page: usize) -> Result<(Vec<Podcast>, Option<usize>)> {
        let page_text = page.to_string();
        let size = SEARCH_PAGE.to_string();
        let r = self.get(if self.web { "/search/instant/mixed" } else { "/search" }, &[
            ("text", text), ("type", "podcast"), ("page", &page_text),
            (if self.web { "pageSize" } else { "page-size" }, &size), ("nocorrect", "false"),
        ])?;
        if self.web {
            let shows: Vec<_> = r["results"].as_array().into_iter().flatten()
                .filter_map(|v| Podcast::from_json(&v["podcast"])).collect();
            let total = (r["lastPage"].as_bool().unwrap_or(true) || shows.is_empty())
                .then_some(page * SEARCH_PAGE + shows.len());
            Ok((shows, total))
        } else {
            Ok((r["podcasts"]["results"].as_array().into_iter().flatten().filter_map(Podcast::from_json).collect(),
                Some(r["podcasts"]["total"].as_u64().unwrap_or(0) as usize)))
        }
    }

    /// One page in the server's episode order, with fresh account-specific progress.
    pub fn podcast_episodes(&self, id: &str, page: usize) -> Result<(Podcast, Vec<Track>, usize)> {
        let r = self.get(&format!("/albums/{id}/with-tracks"), &[
            ("page", &page.to_string()), (if self.web { "pageSize" } else { "page-size" }, &SEARCH_PAGE.to_string()),
            ("richTracks", "true"), ("withListeningFinished", "true"),
        ])?;
        let show = Podcast::from_json(&r).context("альбом не является подкастом")?;
        let total = r["pager"]["total"].as_u64().unwrap_or(show.episodes as u64) as usize;
        let ids: Vec<_> = r["volumes"].as_array().into_iter().flatten()
            .filter_map(Value::as_array).flatten().filter_map(|t| id_str(&t["id"])).collect();
        let tracks = self.podcast_tracks_with_progress(&ids)?.into_iter().filter(Track::is_podcast).collect();
        Ok((show, tracks, total))
    }

    /// Multipart is required by the website for reliable withProgress, including repeated trackIds.
    fn podcast_tracks_with_progress(&self, ids: &[String]) -> Result<Vec<Track>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        const BOUNDARY: &str = "ymusic-podcast-track-request";
        let mut body = String::new();
        for id in ids {
            // Track ids are interpolated into multipart fields, never into its headers.
            body.push_str(&format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"trackIds\"\r\n\r\n{id}\r\n"));
        }
        body.push_str(&format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"withProgress\"\r\n\r\ntrue\r\n--{BOUNDARY}--\r\n"));
        let r = self.result(|| self.req("POST", "/tracks", &[])
            .set("Content-Type", &format!("multipart/form-data; boundary={BOUNDARY}"))
            .send_bytes(body.as_bytes()).map_err(Box::new))?;
        let values = r.as_array().context("нет эпизодов в ответе")?;
        // Preserve album page order even if /tracks changes its result ordering.
        let mut tracks: std::collections::HashMap<_, _> = values.iter().filter_map(Track::from_json)
            .map(|t| (t.id.clone(), t)).collect();
        Ok(ids.iter().filter_map(|id| tracks.remove(id)).collect())
    }

    pub fn podcast_episode(&self, id: &str) -> Result<Track> {
        let track = self.podcast_tracks_with_progress(&[id.to_string()])?.into_iter().next()
            .ok_or_else(|| anyhow!("эпизод не найден"))?;
        if !track.is_podcast() {
            bail!("трек не является эпизодом подкаста");
        }
        Ok(track)
    }

    pub fn podcast_description(&self, id: &str) -> Result<String> {
        let r = self.get(&format!("/tracks/{id}/supplement"), &[])?;
        // Supplements may contain HTML as well as already plain-text descriptions.
        Ok(description_text(r["description"].as_str().unwrap_or("")))
    }

    pub fn podcast_set_liked(&self, id: &str, liked: bool) -> Result<()> {
        let base = format!("/users/{}/likes/albums", self.uid);
        match (self.web, liked) {
            (true, true) => self.post_query(&format!("{base}/add"), &[("album-id", id)])?,
            (true, false) => self.post_query(&format!("{base}/{id}/remove"), &[])?,
            (false, true) => self.post_form(&format!("{base}/add-multiple"), &[("album-ids", id)])?,
            (false, false) => self.post_form(&format!("{base}/remove"), &[("album-ids", id)])?,
        };
        Ok(())
    }

    /// Sync is not an acknowledgement: stale events may be ignored. Read canonical progress back.
    pub fn podcast_save_progress(&self, progress: &PodcastProgress) -> Result<StreamProgress> {
        self.post_json("/streams/progress/sync", &[], json!({
            "lastSyncTimestamp": progress.timestamp,
            "trackStreams": [{
                "trackId": progress.track_id,
                "positionSec": progress.position,
                "trackLengthSec": progress.length,
                "finished": progress.finished,
                "timestamp": progress.timestamp,
            }],
        }))?;
        canonical_progress(self.podcast_episode(&progress.track_id)?.progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_decode_entities_after_stripping_markup() {
        assert_eq!(
            description_text("<p>&laquo;Привет&raquo; &#8212; &#x1F3A7;</p><p>Второй<br/>ряд<br>ещё <a href=\"https://example.test/?q=>\">ссылка</a></p>"),
            "«Привет» — 🎧\n\nВторой\nряд\nещё ссылка"
        );
        assert_eq!(
            description_text("&lt;text&gt; &amp; 1 < 2; 3 > 1\nUnicode: 日本語"),
            "<text> & 1 < 2; 3 > 1\nUnicode: 日本語"
        );
        assert_eq!(description_text("<a title='quoted > value'>visible</a>"), "visible");
        assert_eq!(description_text("before <unfinished"), "before <unfinished");
        assert_eq!(description_text("a\r\nb\rc\u{1b}[31md\u{9b}e\u{7}"), "a\nb\nc[31mde");
    }


    #[test]
    fn missing_canonical_progress_is_not_an_acknowledgement() {
        for value in [
            json!({"id": 1, "type": "podcast-episode"}),
            json!({"id": 1, "type": "podcast-episode", "streamProgress": null}),
            json!({"id": 1, "type": "podcast-episode", "streamProgress": {}}),
        ] {
            let track = Track::from_json(&value).unwrap();
            assert!(canonical_progress(track.progress).is_err());
        }
        let progress = StreamProgress { position: 0.0, finished: false };
        assert_eq!(canonical_progress(Some(progress.clone())).unwrap(), progress);
    }

    #[test]
    fn concrete_types_distinguish_podcasts_from_books() {
        for (value, expected) in [
            (json!({"id": 1}), TrackKind::Music),
            (json!({"id": 1, "metaType": "podcast"}), TrackKind::Other),
            (json!({"id": 1, "type": "podcast-episode"}), TrackKind::Podcast),
            (json!({"id": 1, "albums": [{"type": "podcast", "title": "Show"}]}), TrackKind::Podcast),
            (json!({"id": 1, "albums": [{"type": "audiobook", "metaType": "podcast"}]}), TrackKind::Book),
        ] {
            assert_eq!(Track::from_json(&value).unwrap().kind, expected);
        }
        let track = Track::from_json(&json!({"id": 1, "type": "podcast-episode",
            "albums": [{"type": "podcast", "title": "Show"}],
            "pubDate": "2026-01-02", "streamProgress": {"endPositionSec": 12.75, "everFinished": true}})).unwrap();
        assert_eq!(track.artists, "Show");
        assert_eq!(track.published.as_deref(), Some("2026-01-02"));
        assert_eq!(track.progress, Some(StreamProgress { position: 12.75, finished: true }));
        assert!(Podcast::from_json(&json!({"id": 1, "type": "audiobook", "metaType": "podcast"})).is_none());
    }
}
