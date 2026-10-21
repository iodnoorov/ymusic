//! Downloads a track into memory while it is being decoded. A dropped or stalled connection is
//! resumed with a Range request from where it stopped; reads wait for the bytes they need.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A connection that sends nothing for this long is dropped and reopened.
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Reconnects in a row without receiving anything before the download fails.
const MAX_RETRIES: u32 = 3;
const CHUNK: usize = 64 * 1024;

#[derive(Default)]
struct State {
    data: Vec<u8>,
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
        f(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }
}

/// A download running in its own thread. Dropping every handle (and reader) does not stop it:
/// `cancel` does.
#[derive(Clone)]
pub struct Download(Arc<Shared>);

impl Download {
    pub fn start(url: &str) -> Download {
        Download::start_with(url, STALL_TIMEOUT)
    }

    fn start_with(url: &str, stall: Duration) -> Download {
        let shared = Arc::new(Shared::default());
        let (s, url) = (shared.clone(), url.to_string());
        std::thread::spawn(move || run(&s, &url, stall));
        Download(shared)
    }

    /// Stops the download; readers waiting for data get an error.
    pub fn cancel(&self) {
        self.0.update(|s| s.cancelled = true);
    }

    pub fn reader(&self) -> Reader {
        Reader { shared: self.0.clone(), pos: 0 }
    }
}

fn run(shared: &Shared, url: &str, stall: Duration) {
    let agent = ureq::AgentBuilder::new().timeout_connect(stall).timeout_read(stall).build();
    let mut retries = 0;
    loop {
        let have = shared.state.lock().unwrap().data.len() as u64;
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
        if retries > MAX_RETRIES || shared.state.lock().unwrap().cancelled {
            shared.update(|s| s.error = Some(error));
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
        let mut state = shared.state.lock().unwrap();
        if state.cancelled {
            return Ok(false);
        }
        if n == 0 {
            let complete = state.total.is_none_or(|t| state.data.len() as u64 >= t);
            drop(state);
            return if complete { Ok(true) } else { Err(("соединение оборвалось".into(), got)) };
        }
        let from = (skip as usize).min(n);
        skip -= from as u64;
        state.data.extend_from_slice(&buf[from..n]);
        got = true;
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
        let mut s = self.shared.state.lock().unwrap();
        loop {
            if s.cancelled {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "загрузка отменена"));
            }
            let len = s.data.len() as u64;
            if self.pos < len {
                let n = ((len - self.pos) as usize).min(buf.len());
                let from = self.pos as usize;
                buf[..n].copy_from_slice(&s.data[from..from + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if s.done {
                return Ok(0);
            }
            if let Some(e) = &s.error {
                return Err(io::Error::other(e.clone()));
            }
            s = self.shared.changed.wait(s).unwrap();
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
                let mut s = self.shared.state.lock().unwrap();
                let len = loop {
                    if let Some(t) = s.total {
                        break t;
                    }
                    if s.done {
                        break s.data.len() as u64;
                    }
                    if s.cancelled || s.error.is_some() {
                        return Err(io::Error::other("размер файла неизвестен"));
                    }
                    s = self.shared.changed.wait(s).unwrap();
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
mod tests {
    use super::{Download, Duration};
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
        let d = Download::start_with(&url, Duration::from_millis(500));
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
        let d = Download::start(&url);
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
