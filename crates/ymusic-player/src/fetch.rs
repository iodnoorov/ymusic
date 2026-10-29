//! Downloads a track into an anonymous temporary file while it is being decoded. A dropped or stalled connection is
//! resumed with a Range request from where it stopped; reads wait for the bytes they need.

use crate::Lock;
use std::sync::PoisonError;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A connection that sends nothing for this long is dropped and reopened.
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Reconnects in a row without receiving anything before the download fails.
const MAX_RETRIES: u32 = 3;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Default)]
struct State {
    file: Option<std::fs::File>,
    downloaded: u64,
    network_error: bool,
    /// File size, when the server tells it.
    total: Option<u64>,
    done: bool,
    error: Option<String>,
    cancelled: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.locked());
        self.changed.notify_all();
    }
}

/// A download running in its own thread. Dropping every handle (and reader) does not stop it:
/// `cancel` does.
#[derive(Clone)]
pub struct Download(Arc<Shared>);

impl Download {
    pub fn start(url: &str, tls: &Arc<rustls::ClientConfig>) -> Download {
        Download::start_with(url, tls, STALL_TIMEOUT)
    }

    fn start_with(url: &str, tls: &Arc<rustls::ClientConfig>, stall: Duration) -> Download {
        let shared = Arc::new(Shared::default());
        match tempfile::tempfile() {
            Ok(file) => shared.state.locked().file = Some(file),
            Err(e) => {
                shared.update(|s| s.error = Some(format!("не удалось создать временный файл трека: {e}")));
                return Download(shared);
            }
        }
        let (s, url, tls) = (shared.clone(), url.to_string(), tls.clone());
        // Only fails when the system is out of threads; the readers then get an error.
        if let Err(e) = std::thread::Builder::new().name("player-download".into()).spawn({
            let s = s.clone();
            move || run(&s, &url, tls, stall)
        }) {
            s.update(|st| st.error = Some(e.to_string()));
        }
        Download(shared)
    }

    /// Stops the download; readers waiting for data get an error.
    pub fn cancel(&self) {
        self.0.update(|s| s.cancelled = true);
    }

    /// The download gave up: the connection could not be made or kept.
    pub fn failed(&self) -> bool {
        self.0.state.locked().network_error
    }

    #[cfg(test)]
    pub(crate) fn for_test(data: &[u8], done: bool) -> Download {
        let file = tempfile::tempfile().unwrap();
        file.write_all_at(data, 0).unwrap();
        Download(Arc::new(Shared { state: Mutex::new(State {
            file: Some(file), downloaded: data.len() as u64, done, ..State::default()
        }), changed: Condvar::new() }))
    }

    pub fn reader(&self) -> Reader {
        Reader { shared: self.0.clone(), pos: 0 }
    }
}

fn run(shared: &Shared, url: &str, tls: Arc<rustls::ClientConfig>, stall: Duration) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(stall)
        .timeout_read(stall)
        .tls_config(tls)
        .build();
    let mut retries = 0;
    loop {
        if shared.state.locked().cancelled { return; }
        let have = shared.state.locked().downloaded;
        let mut req = agent.get(url);
        if have > 0 {
            req = req.set("Range", &format!("bytes={have}-"));
        }
        let error = match req.call() {
            Ok(resp) => {
                // A server that ignores Range sends the whole file again: skip what is already here.
                let skip = if resp.status() == 206 { 0 } else { have };
                let total = match resp.header("Content-Range").and_then(|r| r.rsplit('/').next()?.parse().ok()) {
                    Some(t) => Some(t),
                    None => resp.header("Content-Length").and_then(|l| l.parse::<u64>().ok()).map(|l| l + have - skip),
                };
                shared.update(|s| s.total = s.total.or(total));
                match receive(shared, resp.into_reader(), skip) {
                    Ok(true) => {
                        shared.update(|s| s.done = true);
                        return;
                    }
                    Ok(false) => return,
                    Err((e, got)) => {
                        if got {
                            retries = 0;
                        }
                        e
                    }
                }
            }
            Err(e) => e.to_string(),
        };
        retries += 1;
        if retries > MAX_RETRIES || shared.state.locked().cancelled {
            shared.update(|s| { s.error = Some(error); s.network_error = true; });
            return;
        }
        std::thread::sleep(Duration::from_millis(500 * u64::from(retries)));
    }
}

/// Appends the response body to the buffer. `Ok(true)` — the file is complete, `Ok(false)` —
/// cancelled, `Err` — the connection broke (and whether anything arrived before that).
fn receive(shared: &Shared, mut body: impl Read, mut skip: u64) -> Result<bool, (String, bool)> {
    let mut buf = vec![0u8; CHUNK];
    let mut got = false;
    loop {
        let n = body.read(&mut buf).map_err(|e| (e.to_string(), got))?;
        let mut state = shared.state.locked();
        if state.cancelled {
            return Ok(false);
        }
        if n == 0 {
            let complete = state.total.is_none_or(|t| state.downloaded >= t);
            drop(state);
            return if complete { Ok(true) } else { Err(("соединение оборвалось".into(), got)) };
        }
        let from = (skip as usize).min(n);
        skip -= from as u64;
        let data = &buf[from..n];
        let result = if state.downloaded + data.len() as u64 > MAX_FILE {
            Err(io::Error::other("файл трека превышает 512 МиБ"))
        } else {
            state.file.as_ref().expect("download file").write_all_at(data, state.downloaded)
        };
        if let Err(e) = result {
            state.error = Some(format!("не удалось сохранить загружаемый трек: {e}"));
            drop(state);
            shared.changed.notify_all();
            return Ok(false);
        }
        state.downloaded += data.len() as u64;
        got |= !data.is_empty();
        drop(state);
        shared.changed.notify_all();
    }
}

/// Reads the downloaded file, waiting for bytes that have not arrived yet.
pub struct Reader {
    shared: Arc<Shared>,
    pos: u64,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        let mut s = self.shared.state.locked();
        loop {
            if s.cancelled {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "загрузка отменена"));
            }
            let len = s.downloaded;
            if self.pos < len {
                let n = ((len - self.pos) as usize).min(buf.len());
                s.file.as_ref().ok_or_else(|| io::Error::other("нет временного файла трека"))?
                    .read_exact_at(&mut buf[..n], self.pos)?;
                self.pos += n as u64;
                return Ok(n);
            }
            if s.done {
                return Ok(0);
            }
            if let Some(e) = &s.error {
                return Err(io::Error::other(e.clone()));
            }
            s = self.shared.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => {
                // The size is needed: wait for it (or for the whole file).
                let mut s = self.shared.state.locked();
                let len = loop {
                    if let Some(t) = s.total {
                        break t;
                    }
                    if s.done {
                        break s.downloaded;
                    }
                    if s.cancelled || s.error.is_some() {
                        return Err(io::Error::other("размер файла неизвестен"));
                    }
                    s = self.shared.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
                };
                len as i64 + d
            }
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "позиция до начала файла"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for Reader {
    fn is_seekable(&self) -> bool {
        true
    }

    /// Unknown on purpose: with a length the prober looks for tags at the end of the file, which
    /// would wait for the whole download before playback starts.
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Download, Duration};

    /// No server here speaks TLS: any configuration does.
    pub fn tls() -> std::sync::Arc<rustls::ClientConfig> {
        let roots = rustls::RootCertStore::empty();
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        std::sync::Arc::new(config)
    }
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
    use std::net::TcpListener;

    /// Serves `body` over HTTP: the first connection sends `cut` bytes of it and then stalls, later
    /// ones honour Range. Returns the URL and a counter of the connections made.
    fn server(body: Vec<u8>, cut: usize) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/track", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (mut stream, body) = (stream.unwrap(), body.clone());
                let n = c.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut from = 0;
                    for line in BufReader::new(stream.try_clone().unwrap()).lines() {
                        let line = line.unwrap();
                        if let Some(r) = line.strip_prefix("Range: bytes=") {
                            from = r.trim_end_matches('-').parse().unwrap();
                        }
                        if line.is_empty() {
                            break;
                        }
                    }
                    let part = &body[from..];
                    let status = if from > 0 { "206 Partial Content" } else { "200 OK" };
                    let range = format!("Content-Range: bytes {from}-{}/{}\r\n", body.len() - 1, body.len());
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
                        part.len(),
                        if from > 0 { range.as_str() } else { "" }
                    );
                    // The client may hang up on a stalled connection: write errors don't matter.
                    let _ = stream.write_all(head.as_bytes());
                    if n == 0 {
                        let _ = stream.write_all(&part[..cut]);
                        std::thread::sleep(Duration::from_secs(3));
                    } else {
                        let _ = stream.write_all(part);
                    }
                });
            }
        });
        (url, count)
    }

    #[test]
    fn resumes_a_stalled_download() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (url, count) = server(body.clone(), 100_000);
        let d = Download::start_with(&url, &tls(), Duration::from_millis(500));
        let mut r = d.reader();
        // Seeking to the end needs the size, which the first response already gave.
        assert_eq!(r.seek(SeekFrom::End(-10)).unwrap(), body.len() as u64 - 10);
        r.seek(SeekFrom::Start(0)).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert!(got == body, "{} bytes", got.len());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn cancel_wakes_a_waiting_reader() {
        let (url, _) = server(vec![1; 1000], 10);
        let d = Download::start(&url, &tls());
        let mut r = d.reader();
        let mut buf = [0u8; 100];
        assert_eq!(r.read(&mut buf).unwrap(), 10);
        let d2 = d.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            d2.cancel();
        });
        assert_eq!(r.read(&mut buf).unwrap_err().kind(), std::io::ErrorKind::ConnectionAborted);
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    #[test]
    fn empty_reads_do_not_wait_and_spooled_bytes_support_backward_seek() {
        let d = Download::for_test(b"abcdef", false);
        let mut r = d.reader();
        assert_eq!(r.read(&mut []).unwrap(), 0);
        let mut buf = [0; 3];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"abc");
        r.seek(SeekFrom::Start(1)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"bcd");
        d.cancel();
        assert_eq!(r.read(&mut buf).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
    }
    #[test]
    fn spool_error_wakes_reader_and_is_not_a_network_failure() {
        let d = Download::for_test(b"", false);
        // A deliberately read-only descriptor simulates a local storage failure.
        d.0.state.locked().file = Some(std::fs::File::open("Cargo.toml").unwrap());
        assert!(!receive(&d.0, &b"data"[..], 0).unwrap());
        assert!(d.reader().read(&mut [0; 4]).is_err());
        assert!(!d.failed());
    }
}
