//! A bounded HTTP range worker and a sparse temporary-file cache. Only explicitly received byte
//! ranges are readable: the unwritten holes in the sparse file are never audio data.

use crate::Lock;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Socket operations are bounded, including retries of partially received bodies.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRIES: u32 = 3;
const MAX_FILE: u64 = 512 * 1024 * 1024;
/// Range requests are at most 256 KiB; look ahead at most 1 MiB from the latest reader position.
const BLOCK: u64 = 256 * 1024;
const PREFETCH: u64 = 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Default)]
struct State {
    file: Option<std::fs::File>,
    /// Sorted, disjoint, non-adjacent half-open ranges whose writes have completed.
    valid: Vec<Range<u64>>,
    total: Option<u64>,
    /// Latest reader position; a missing demanded byte always precedes speculative work.
    demand: u64,
    waiting: usize,
    sequential: bool,
    network_error: bool,
    error: Option<String>,
    cancelled: bool,
}

impl State {
    fn available(&self, pos: u64) -> u64 {
        self.valid.iter().find(|r| r.start <= pos && pos < r.end).map_or(0, |r| r.end - pos)
    }

    fn record(&mut self, mut range: Range<u64>) {
        if range.is_empty() { return; }
        let first = self.valid.partition_point(|r| r.end < range.start);
        let mut end = first;
        while let Some(r) = self.valid.get(end) {
            if r.start > range.end { break; }
            range.start = range.start.min(r.start);
            range.end = range.end.max(r.end);
            end += 1;
        }
        self.valid.splice(first..end, [range]);
    }

    fn size(&mut self, size: u64) -> Result<(), String> {
        if size > MAX_FILE {
            return Err("файл трека превышает 512 МиБ".into());
        }
        if self.total.is_some_and(|old| old != size)
            || self.valid.last().is_some_and(|r| r.end > size)
        {
            return Err("размер HTTP-файла изменился во время загрузки".into());
        }
        self.total = Some(size);
        Ok(())
    }

    fn request(&self) -> Option<Range<u64>> {
        let limit = self.total.unwrap_or(MAX_FILE + 1);
        let mut pos = self.demand;
        let ahead = self.demand.saturating_add(PREFETCH).min(limit);
        while pos < ahead {
            let have = self.available(pos);
            if have == 0 {
                // Do not refetch a received prefix after a short/disconnected response.
                let block_end = (pos / BLOCK + 1) * BLOCK;
                let next_cached = self.valid.iter().find(|r| r.start > pos).map_or(limit, |r| r.start);
                return Some(pos..block_end.min(ahead).min(next_cached));
            }
            pos += have;
        }
        None
    }
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

/// Exactly one network worker belongs to each download. Cancellation wakes readers immediately;
/// no decoder or control thread owns or waits for the worker's socket.
#[derive(Clone)]
pub struct Download(Arc<Shared>);

impl Download {
    pub fn start(url: &str, tls: &Arc<rustls::ClientConfig>) -> Download {
        Self::start_with(url, tls, IO_TIMEOUT)
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
        if let Err(e) = std::thread::Builder::new().name("player-download".into()).spawn({
            let s = s.clone();
            move || run(&s, &url, tls, stall)
        }) {
            s.update(|st| st.error = Some(e.to_string()));
        }
        Download(shared)
    }

    pub fn cancel(&self) {
        self.0.update(|s| s.cancelled = true);
    }

    pub fn failed(&self) -> bool {
        self.0.state.locked().network_error
    }

    /// This source, rather than a future preloaded source, is waiting for uncached bytes/size.
    pub fn waiting(&self) -> bool {
        let s = self.0.state.locked();
        s.waiting != 0 && !s.cancelled && s.error.is_none()
    }

    #[cfg(test)]
    pub(crate) fn for_test(data: &[u8], done: bool) -> Download {
        let file = tempfile::tempfile().unwrap();
        file.write_all_at(data, 0).unwrap();
        let mut state = State { file: Some(file), total: done.then_some(data.len() as u64), ..State::default() };
        state.record(0..data.len() as u64);
        Download(Arc::new(Shared { state: Mutex::new(state), changed: Condvar::new() }))
    }

    pub fn reader(&self) -> Reader {
        Reader { shared: self.0.clone(), pos: 0 }
    }
}

#[derive(Debug, PartialEq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

fn content_range(value: &str) -> Option<ContentRange> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, inclusive) = (start.parse::<u64>().ok()?, end.parse::<u64>().ok()?);
    let end = inclusive.checked_add(1)?;
    let total = if total == "*" { None } else { Some(total.parse::<u64>().ok()?) };
    (start < end && total.is_none_or(|n| end <= n)).then_some(ContentRange { start, end, total })
}

fn content_length(resp: &ureq::Response) -> Result<Option<u64>, String> {
    resp.header("Content-Length").map(|v| v.parse().map_err(|_| "неверный Content-Length".into())).transpose()
}

/// A newly demanded hole preempts speculative reads at the next read/deadline boundary.
/// Sequential responses pause at the lookahead limit instead of refetching their prefix.
fn preempted(s: &State, request: &Range<u64>, offset: u64, sequential: bool) -> bool {
    if s.cancelled || s.error.is_some() { return true; }
    if s.total.is_some_and(|n| s.demand >= n) { return true; }
    if s.available(s.demand) == 0 {
        if sequential { offset > s.demand } else { !request.contains(&s.demand) }
    } else {
        !sequential && offset >= s.demand.saturating_add(PREFETCH)
    }
}

enum Received {
    Complete,
    Interrupted,
    Retry(String),
    Fatal(String),
}

fn receive(shared: &Shared, mut body: impl Read, request: &Range<u64>, mut offset: u64,
    expected_end: Option<u64>, sequential: bool, progress: &mut bool) -> Received
{
    let mut buf = [0u8; CHUNK];
    loop {
        let amount = {
            let mut s = shared.state.locked();
            loop {
                if preempted(&s, request, offset, sequential) { return Received::Interrupted; }
                if expected_end == Some(offset) { return Received::Complete; }
                let ahead = s.demand.saturating_add(PREFETCH);
                if !sequential || offset < ahead {
                    let amount = expected_end.map_or(CHUNK as u64, |end| (end - offset).min(CHUNK as u64));
                    break if sequential { amount.min(ahead - offset) as usize } else { amount as usize };
                }
                // Keep this healthy HTTP 200 body alive while paused. Reader progress, seeks,
                // cancellation and terminal errors all notify this same condition variable.
                s = shared.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let read = body.read(&mut buf[..amount]);
        let mut s = shared.state.locked();
        if s.cancelled { return Received::Interrupted; }
        let n = match read {
            Ok(n) => n,
            Err(e) => return if preempted(&s, request, offset, sequential) {
                Received::Interrupted
            } else {
                Received::Retry(e.to_string())
            },
        };
        if n == 0 {
            if expected_end.is_some_and(|end| offset < end) {
                return Received::Retry("соединение оборвалось до конца HTTP-диапазона".into());
            }
            // Only an unbounded, sequential response's EOF establishes an unknown size.
            if sequential && let Err(e) = s.size(offset) { return Received::Fatal(e); }
            drop(s);
            shared.changed.notify_all();
            return Received::Complete;
        }
        let end = offset + n as u64;
        if end > MAX_FILE { return Received::Fatal("файл трека превышает 512 МиБ".into()); }
        let fresh = s.available(offset) < n as u64;
        if let Err(e) = s.file.as_ref().expect("download file").write_all_at(&buf[..n], offset) {
            return Received::Fatal(format!("не удалось сохранить загружаемый трек: {e}"));
        }
        s.record(offset..end);
        *progress |= fresh;
        offset = end;
        drop(s);
        shared.changed.notify_all();
    }
}

fn fetch(shared: &Shared, agent: &ureq::Agent, url: &str, request: &Range<u64>, progress: &mut bool) -> Received {
    let sequential = shared.state.locked().sequential;
    let mut req = agent.get(url).set("Accept-Encoding", "identity");
    if !sequential { req = req.set("Range", &format!("bytes={}-{}", request.start, request.end - 1)); }
    let resp = match req.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(416, resp)) => {
            let total = resp.header("Content-Range").and_then(|v| v.strip_prefix("bytes */"))
                .and_then(|v| v.parse::<u64>().ok());
            return match total {
                Some(n) if request.start >= n => {
                    let result = shared.state.locked().size(n);
                    shared.changed.notify_all();
                    match result { Ok(()) => Received::Complete, Err(e) => Received::Fatal(e) }
                }
                _ => Received::Fatal("неверный ответ HTTP 416 / Content-Range".into()),
            };
        }
        Err(e) => return Received::Retry(e.to_string()),
    };
    if resp.header("Content-Encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Received::Fatal("сервер сжал HTTP-диапазон вопреки Accept-Encoding: identity".into());
    }
    let length = match content_length(&resp) { Ok(n) => n, Err(e) => return Received::Fatal(e) };
    let (offset, end, sequential) = match resp.status() {
        206 => {
            let Some(range) = resp.header("Content-Range").and_then(content_range) else {
                return Received::Fatal("нет корректного Content-Range для HTTP 206".into());
            };
            if sequential || range.start != request.start || range.end > request.end
                || length.is_some_and(|n| n != range.end - range.start)
            {
                return Received::Fatal("HTTP 206 не соответствует запрошенному диапазону".into());
            }
            if let Some(total) = range.total {
                let result = shared.state.locked().size(total);
                shared.changed.notify_all();
                if let Err(e) = result { return Received::Fatal(e); }
            }
            (range.start, Some(range.end), false)
        }
        200 => {
            let result = {
                let mut s = shared.state.locked();
                s.sequential = true;
                length.map_or(Ok(()), |n| s.size(n))
            };
            shared.changed.notify_all();
            if let Err(e) = result { return Received::Fatal(e); }
            // A server ignoring Range always starts at zero, not at the requested offset.
            (0, length, true)
        }
        status => return Received::Fatal(format!("неожиданный HTTP-статус {status}")),
    };
    receive(shared, resp.into_reader(), request, offset, end, sequential, progress)
}


fn run(shared: &Shared, url: &str, tls: Arc<rustls::ClientConfig>, io_timeout: Duration) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(io_timeout)
        .timeout_read(io_timeout)
        .timeout_write(io_timeout)
        .tls_config(tls)
        .build();
    let mut retries = 0;
    loop {
        let request = {
            let mut s = shared.state.locked();
            loop {
                if s.cancelled || s.error.is_some() { return; }
                if let Some(range) = s.request() { break range; }
                s = shared.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let mut progress = false;
        let result = fetch(shared, &agent, url, &request, &mut progress);
        if progress { retries = 0; }
        match result {
            Received::Complete | Received::Interrupted => {}
            Received::Fatal(e) => {
                shared.update(|s| s.error = Some(e));
                return;
            }
            Received::Retry(e) => {
                let mut s = shared.state.locked();
                if s.cancelled { return; }
                // Do not charge failure/retry delay to a different, newly demanded range.
                if s.available(s.demand) == 0 && !request.contains(&s.demand) { continue; }
                retries += 1;
                if retries > MAX_RETRIES {
                    s.error = Some(e);
                    s.network_error = true;
                    drop(s);
                    shared.changed.notify_all();
                    return;
                }
                drop(shared.changed.wait_timeout(s, Duration::from_millis(250 * u64::from(retries)))
                    .unwrap_or_else(PoisonError::into_inner));
            }
        }
    }
}

/// Reads only valid cached ranges and asks the worker for every missing offset.
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
            if s.total.is_some_and(|n| self.pos >= n) { return Ok(0); }
            if self.pos > MAX_FILE {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "позиция за пределами 512 МиБ"));
            }
            let available = s.available(self.pos);
            if available != 0 {
                let n = available.min(buf.len() as u64) as usize;
                s.file.as_ref().ok_or_else(|| io::Error::other("нет временного файла трека"))?
                    .read_exact_at(&mut buf[..n], self.pos)?;
                self.pos += n as u64;
                s.demand = self.pos;
                drop(s);
                self.shared.changed.notify_all();
                return Ok(n);
            }
            if let Some(e) = &s.error { return Err(io::Error::other(e.clone())); }
            s.demand = self.pos;
            s.waiting += 1;
            self.shared.changed.notify_all();
            s = self.shared.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
            s.waiting -= 1;
        }
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                let mut s = self.shared.state.locked();
                let len = loop {
                    if s.cancelled {
                        return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "загрузка отменена"));
                    }
                    if let Some(t) = s.total { break t; }
                    if let Some(e) = &s.error { return Err(io::Error::other(e.clone())); }
                    // With unknown length, discover EOF sequentially instead of waiting forever
                    // at the end of the ordinary prefetch window.
                    let pos = s.demand.min(MAX_FILE);
                    s.demand = pos + s.available(pos);
                    s.waiting += 1;
                    self.shared.changed.notify_all();
                    s = self.shared.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
                    s.waiting -= 1;
                };
                len.checked_add_signed(d)
            }
        }.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "позиция за границами файла"))?;
        self.pos = target;
        self.shared.update(|s| s.demand = target);
        Ok(target)
    }
}

impl symphonia::core::io::MediaSource for Reader {
    fn is_seekable(&self) -> bool { true }

    fn byte_len(&self) -> Option<u64> { self.shared.state.locked().total }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;
    use symphonia::core::io::MediaSource;

    pub fn tls() -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().unwrap()
            .with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
    }

    #[derive(Clone, Copy)]
    enum Mode { Ranges, Ignore, Truncate, Short, Unknown, UnknownRanges, Oversize, Invalid, Stall }

    struct Server {
        url: String,
        requests: Arc<Mutex<Vec<Range<u64>>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn new(data: Vec<u8>, mode: Mode) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/track", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (seen, stopped, data) = (requests.clone(), stop.clone(), Arc::new(data));
            let thread = std::thread::spawn(move || {
                let mut connections = Vec::new();
                while !stopped.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(_) => break,
                    };
                    let (data, seen, stopped) = (data.clone(), seen.clone(), stopped.clone());
                    connections.push(std::thread::spawn(move || {
                        // BSD/macOS inherit the listener's nonblocking mode; Linux does not.
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                        let mut request = 0..data.len() as u64;
                        let mut ranged = false;
                        for line in BufReader::new(stream.try_clone().unwrap()).lines() {
                            let Ok(line) = line else { return; };
                            if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                                let (start, end) = value.split_once('-').unwrap();
                                request = start.parse().unwrap()..end.parse::<u64>().unwrap() + 1;
                                ranged = true;
                            }
                            if line.is_empty() { break; }
                        }
                        let first = {
                            let mut requests = seen.locked();
                            let first = requests.is_empty();
                            requests.push(request.clone());
                            first
                        };
                        let sequential = matches!(mode, Mode::Ignore | Mode::Unknown) || !ranged;
                        if !sequential && request.start >= data.len() as u64 {
                            let _ = write!(stream, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", data.len());
                            return;
                        }
                        let from = if sequential { 0 } else { request.start as usize };
                        let mut to = if sequential { data.len() } else { (request.end as usize).min(data.len()) };
                        if matches!(mode, Mode::Short) { to = from + (to - from).div_ceil(2); }
                        let status = if sequential { "200 OK" } else { "206 Partial Content" };
                        let length = if matches!(mode, Mode::Unknown) { String::new() }
                            else { format!("Content-Length: {}\r\n", to - from) };
                        let range = if sequential { String::new() } else {
                            let start = if matches!(mode, Mode::Invalid) { from + 1 } else { from };
                            let total = match mode {
                                Mode::UnknownRanges => "*".into(),
                                Mode::Oversize => (MAX_FILE + 1).to_string(),
                                _ => data.len().to_string(),
                            };
                            format!("Content-Range: bytes {start}-{}/{total}\r\n", to - 1)
                        };
                        if write!(stream, "HTTP/1.1 {status}\r\n{length}{range}Connection: close\r\n\r\n").is_err() { return; }
                        if first && matches!(mode, Mode::Stall) {
                            let _ = stream.write_all(&data[from..(from + 16).min(to)]);
                            while !stopped.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_millis(5)); }
                        } else {
                            if first && matches!(mode, Mode::Truncate) { to = (from + CHUNK + 7).min(to); }
                            let _ = stream.write_all(&data[from..to]);
                        }
                    }));
                }
                for connection in connections { let _ = connection.join(); }
            });
            Self { url, requests, stop, thread: Some(thread) }
        }

        fn download(&self) -> Download { Download::start_with(&self.url, &tls(), Duration::from_millis(200)) }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
        }
    }

    fn bytes(len: usize) -> Vec<u8> { (0..len).map(|i| (i % 251 + 1) as u8).collect() }

    #[test]
    fn far_read_backward_read_and_unfilled_holes() {
        let body = bytes(4 * PREFETCH as usize);
        let server = Server::new(body.clone(), Mode::Ranges);
        let d = server.download();
        let mut reader = d.reader();
        let mut got = [0; 37];
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, body[..37]);
        assert_eq!(reader.byte_len(), Some(body.len() as u64));
        let tail = reader.seek(SeekFrom::End(-37)).unwrap();
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, body[tail as usize..]);
        let hole = 2 * PREFETCH + 19;
        assert_eq!(d.0.state.locked().available(hole), 0, "prefix prefetch filled a remote hole");
        reader.seek(SeekFrom::Start(hole)).unwrap();
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, body[hole as usize..hole as usize + 37]);
        reader.seek(SeekFrom::Start(3)).unwrap();
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, body[3..40]);
        assert!(server.requests.locked().iter().any(|r| r.start == tail));
        d.cancel();
    }

    #[test]
    fn mp4_with_moov_beyond_the_prefetch_window_opens_and_seeks() {
        let mut body = include_bytes!("../tests/data/aac-lc.m4a").to_vec();
        let mut pos = 0;
        let movie = loop {
            assert!(pos + 8 <= body.len(), "fixture has no moov atom");
            let short = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
            let len = match short {
                0 => body.len() - pos,
                1 => u64::from_be_bytes(body[pos + 8..pos + 16].try_into().unwrap()) as usize,
                len => len as usize,
            };
            assert!(len >= 8 && pos + len <= body.len());
            if &body[pos + 4..pos + 8] == b"moov" { break pos..pos + len; }
            pos += len;
        };
        let moov = body[movie.clone()].to_vec();
        // Preserve every original media offset: replace the old moov with an equal-size free
        // atom, then put its unchanged contents after a remote padding atom.
        body[movie.start + 4..movie.start + 8].copy_from_slice(b"free");
        let padding_start = body.len();
        let padding_len = (2 * PREFETCH + 8) as u32;
        body.extend_from_slice(&padding_len.to_be_bytes());
        body.extend_from_slice(b"free");
        body.resize(padding_start + padding_len as usize, 0);
        let tail = body.len() as u64;
        body.extend_from_slice(&moov);
        let server = Server::new(body, Mode::Ranges);
        let d = server.download();
        let mut decoder = crate::decode::Decoder::open(Box::new(d.reader())).unwrap();
        assert!(decoder.next_chunk().unwrap().is_some());
        assert!((decoder.seek(1.).unwrap() - 1.).abs() < 0.01);
        assert!(decoder.next_chunk().unwrap().is_some());
        assert!(server.requests.locked().iter().any(|r| r.end > tail && r.start > tail - PREFETCH));
        assert_eq!(d.0.state.locked().available(tail - PREFETCH / 2), 0);
        d.cancel();
    }

    #[test]
    fn range_ignoring_server_is_read_from_zero() {
        let body = bytes(2 * PREFETCH as usize + 113);
        let server = Server::new(body.clone(), Mode::Ignore);
        let d = server.download();
        let mut reader = d.reader();
        reader.seek(SeekFrom::End(-57)).unwrap();
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, body[body.len() - 57..]);
        reader.seek(SeekFrom::Start(PREFETCH + 3)).unwrap();
        let mut middle = [0; 99];
        reader.read_exact(&mut middle).unwrap();
        assert_eq!(middle, body[PREFETCH as usize + 3..PREFETCH as usize + 102]);
        d.cancel();
    }

    #[test]
    fn transport_regression_sequential_playback_keeps_one_healthy_response() {
        let body = bytes(4 * PREFETCH as usize + 113);
        let server = Server::new(body.clone(), Mode::Ignore);
        let d = server.download();
        let mut reader = d.reader();
        let mut offset = 0;
        let mut buffer = vec![0; PREFETCH as usize / 2];
        for _ in 0..4 {
            reader.read_exact(&mut buffer).unwrap();
            assert_eq!(buffer, body[offset..offset + buffer.len()]);
            offset += buffer.len();
            let until = Instant::now() + Duration::from_secs(2);
            let mut state = d.0.state.locked();
            while state.available(offset as u64) < PREFETCH {
                let remaining = until.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "prefetch did not reach the next window");
                state = d.0.changed.wait_timeout(state, remaining).unwrap().0;
            }
            drop(state);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(server.requests.locked().len(), 1, "healthy sequential body was restarted");
        d.cancel();
    }

    #[test]
    fn cancellation_wakes_a_sequential_response_paused_at_prefetch_limit() {
        let d = Download::for_test(b"", false);
        let shared = d.0.clone();
        let (done, completed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let body = bytes(3 * PREFETCH as usize);
            let result = receive(&shared, &body[..], &(0..BLOCK), 0, Some(body.len() as u64), true, &mut false);
            done.send(matches!(result, Received::Interrupted)).unwrap();
        });
        let until = Instant::now() + Duration::from_secs(2);
        let mut state = d.0.state.locked();
        while state.available(0) < PREFETCH {
            let remaining = until.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "sequential response did not fill its lookahead");
            state = d.0.changed.wait_timeout(state, remaining).unwrap().0;
        }
        assert_eq!(state.valid, vec![0..PREFETCH], "paused download exceeded its lookahead");
        drop(state);
        // A full lookahead is neither EOF nor an interruption. The body stays owned until
        // reader progress or cancellation, rather than being discarded for the next window.
        assert!(matches!(completed.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
        d.cancel();
        assert!(completed.recv_timeout(Duration::from_millis(200)).unwrap(), "paused receive did not cancel");
        worker.join().unwrap();
        assert_eq!(d.reader().read(&mut [0; 1]).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn disconnected_response_retries_after_the_received_prefix() {
        let body = bytes(BLOCK as usize + 71);
        let server = Server::new(body.clone(), Mode::Truncate);
        let d = server.download();
        let mut got = Vec::new();
        d.reader().read_to_end(&mut got).unwrap();
        assert_eq!(got, body);
        assert!(server.requests.locked().iter().any(|r| r.start == CHUNK as u64 + 7));
        d.cancel();
    }

    #[test]
    fn valid_short_ranges_are_continued_not_treated_as_eof() {
        let body = bytes(BLOCK as usize + 71);
        let server = Server::new(body.clone(), Mode::Short);
        let d = server.download();
        let mut got = Vec::new();
        d.reader().read_to_end(&mut got).unwrap();
        assert_eq!(got, body);
        d.cancel();
    }

    #[test]
    fn unknown_size_is_discovered_at_sequential_eof() {
        let body = bytes(PREFETCH as usize + 113);
        let server = Server::new(body.clone(), Mode::Unknown);
        let d = server.download();
        let mut reader = d.reader();
        assert_eq!(reader.seek(SeekFrom::End(-19)).unwrap(), body.len() as u64 - 19);
        assert_eq!(reader.byte_len(), Some(body.len() as u64));
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, body[body.len() - 19..]);
        d.cancel();
    }

    #[test]
    fn unknown_range_size_is_discovered_by_validated_416() {
        let body = bytes(2 * PREFETCH as usize + 113);
        let server = Server::new(body.clone(), Mode::UnknownRanges);
        let d = server.download();
        let mut reader = d.reader();
        assert_eq!(reader.seek(SeekFrom::End(-19)).unwrap(), body.len() as u64 - 19);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, body[body.len() - 19..]);
        assert_eq!(reader.byte_len(), Some(body.len() as u64));
        assert!(server.requests.locked().iter().any(|r| r.start == body.len() as u64));
        d.cancel();
    }

    #[test]
    fn oversized_http_metadata_fails_before_creating_cached_ranges() {
        let server = Server::new(bytes(4096), Mode::Oversize);
        let d = server.download();
        assert!(d.reader().read(&mut [0; 1]).is_err());
        assert!(d.0.state.locked().valid.is_empty());
    }

    #[test]
    fn malformed_content_range_never_populates_the_cache() {
        let server = Server::new(bytes(4096), Mode::Invalid);
        let d = server.download();
        assert!(d.reader().read(&mut [0; 1]).is_err());
        assert!(d.0.state.locked().valid.is_empty());
        assert!(!d.failed());
    }

    #[test]
    fn demanded_tail_preempts_a_stalled_prefix() {
        let body = bytes(3 * PREFETCH as usize);
        let server = Server::new(body.clone(), Mode::Stall);
        let d = server.download();
        let mut reader = d.reader();
        let mut prefix = [0; 16];
        reader.read_exact(&mut prefix).unwrap();
        let tail = reader.seek(SeekFrom::End(-16)).unwrap();
        let began = Instant::now();
        let mut got = [0; 16];
        reader.read_exact(&mut got).unwrap();
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(got, body[tail as usize..]);
        d.cancel();
    }

    #[test]
    fn cancellation_wakes_readers_without_waiting_for_the_socket() {
        let server = Server::new(bytes(4096), Mode::Stall);
        let d = Download::start_with(&server.url, &tls(), Duration::from_secs(3));
        let mut reader = d.reader();
        reader.read_exact(&mut [0; 16]).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(reader.read(&mut [0; 1]).unwrap_err().kind()).unwrap());
        let began = Instant::now();
        while !d.waiting() {
            assert!(began.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
        d.cancel();
        assert_eq!(rx.recv_timeout(Duration::from_millis(200)).unwrap(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn empty_reads_backward_cache_and_storage_failure() {
        let d = Download::for_test(b"abcdef", false);
        let mut reader = d.reader();
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let mut got = [0; 3];
        reader.read_exact(&mut got).unwrap();
        reader.seek(SeekFrom::Start(1)).unwrap();
        reader.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"bcd");
        d.cancel();
        assert_eq!(reader.read(&mut got).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        let d = Download::for_test(b"", false);
        d.0.state.locked().file = Some(std::fs::File::open("Cargo.toml").unwrap());
        let result = receive(&d.0, &b"data"[..], &(0..4), 0, Some(4), false, &mut false);
        let Received::Fatal(error) = result else { panic!("storage failure was not fatal"); };
        d.0.update(|s| s.error = Some(error));
        assert!(d.reader().read(&mut [0; 4]).is_err());
        assert!(!d.failed());
    }

    #[test]
    fn content_ranges_and_size_cap_are_strict() {
        assert_eq!(content_range("bytes 17-20/100"), Some(ContentRange { start: 17, end: 21, total: Some(100) }));
        for invalid in ["bytes 9-8/100", "bytes 0-100/100", "bytes 0-18446744073709551615/*", "bytes */12"] {
            assert!(content_range(invalid).is_none(), "{invalid}");
        }
        let mut s = State::default();
        assert!(s.size(MAX_FILE + 1).is_err());
        s.size(100).unwrap();
        assert!(s.size(99).is_err());
    }
}
