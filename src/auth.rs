//! Login: through a browser window (token is captured automatically) or by pasting a token.
//!
//! Browser login opens the Yandex OAuth page in a separate browser instance with a temporary
//! profile and remote debugging enabled. After the user signs in, Yandex redirects to
//! `https://music.yandex.ru/#access_token=…`; the token is read from that URL and the window is closed.
//!
//! Chromium-based browsers are driven over CDP through a pair of pipes (`--remote-debugging-pipe`),
//! which no other process can reach. Firefox-based ones have only a debugging port for WebDriver
//! BiDi: the browser picks a free one itself and reports it, and the WebDriver session is taken at
//! once (a browser allows one), so another local process has only a moment to get in.

use crate::api::CLIENT_ID;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, PipeReader, PipeWriter, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

pub fn auth_url() -> String {
    crate::config::env_str("LOGIN_URL")
        .unwrap_or_else(|| format!("https://oauth.yandex.ru/authorize?response_type=token&client_id={CLIENT_ID}"))
}

/// Interactive menu shown by `ymusic login` and on first start.
pub fn interactive_login() -> Result<String> {
    println!("\n  \x1b[1;33mВход в Яндекс Музыку\x1b[0m\n");
    println!("  1) Через браузер — откроется окно Яндекса, войдите, токен подхватится сам");
    println!("  2) По токену     — вставить OAuth-токен\n");
    let choice = prompt("  Выбор [1]: ")?;
    match choice.trim() {
        "" | "1" => browser_login().or_else(|e| {
            println!("\n  \x1b[31mНе получилось войти через браузер:\x1b[0m {e:#}");
            token_prompt()
        }),
        "2" => token_prompt(),
        other => bail!("неизвестный вариант: {other}"),
    }
}

fn prompt(text: &str) -> Result<String> {
    print!("{text}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

/// Asks the user to paste a token (or a URL containing `access_token=`).
pub fn token_prompt() -> Result<String> {
    println!("\n  Где взять токен: откройте ссылку ниже, войдите, и после перенаправления");
    println!("  скопируйте адрес страницы целиком (или часть после access_token= до &):\n");
    println!("  \x1b[4m{}\x1b[0m\n", auth_url());
    let line = prompt("  Токен или URL: ")?;
    parse_token(line.trim()).ok_or_else(|| anyhow!("токен не найден во введённой строке"))
}

pub fn parse_token(s: &str) -> Option<String> {
    if let Some(t) = extract_token(s) {
        return Some(t);
    }
    let s = s.trim();
    let ok = s.len() >= 20 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| s.to_string())
}

/// Finds `access_token=<token>` in arbitrary text (URL, JSON message...).
fn extract_token(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(i) = rest.find("access_token=") {
        rest = &rest[i + "access_token=".len()..];
        let tok: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        if tok.len() >= 20 {
            return Some(tok);
        }
    }
    None
}

// ---------------------------------------------------------------- browser detection

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Firefox,
    Chromium,
}

struct Browser {
    bin: String,
    kind: Kind,
}

#[cfg(target_os = "linux")]
const CANDIDATES: &[&str] = &[
    "zen-browser",
    "zen",
    "firefox",
    "librewolf",
    "floorp",
    "waterfox",
    "chromium",
    "google-chrome-stable",
    "google-chrome",
    "yandex-browser-stable",
    "yandex-browser",
    "brave",
    "brave-browser",
    "vivaldi-stable",
    "vivaldi",
    "microsoft-edge-stable",
    "thorium-browser",
];

fn classify(bin: &str) -> Option<Kind> {
    let name = Path::new(bin).file_name()?.to_string_lossy().to_lowercase();
    if ["firefox", "zen", "librewolf", "waterfox", "floorp", "mullvad"].iter().any(|k| name.contains(k)) {
        Some(Kind::Firefox)
    } else if ["chrom", "brave", "vivaldi", "edge", "yandex", "opera", "thorium"].iter().any(|k| name.contains(k)) {
        Some(Kind::Chromium)
    } else {
        None
    }
}

fn which(bin: &str) -> Option<String> {
    if bin.contains('/') {
        return Path::new(bin).is_file().then(|| bin.to_string());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Binary of the default browser from its .desktop file.
#[cfg(target_os = "linux")]
fn default_browser_bin() -> Option<String> {
    let out = Command::new("xdg-settings").args(["get", "default-web-browser"]).stderr(Stdio::null()).output().ok()?;
    let desktop = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if desktop.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share")),
    ];
    dirs.extend(
        std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
            .split(':')
            .map(String::from),
    );
    for d in dirs {
        let Ok(content) = std::fs::read_to_string(PathBuf::from(d).join("applications").join(&desktop)) else {
            continue;
        };
        let exec = content.lines().find_map(|l| l.strip_prefix("Exec="))?;
        return exec
            .split_whitespace()
            .find(|t| !t.contains('=') && *t != "env")
            .map(|t| t.trim_matches('"').to_string());
    }
    None
}

#[cfg(target_os = "linux")]
fn candidates() -> impl Iterator<Item = String> {
    let default = default_browser_bin().and_then(|b| which(&b));
    default.into_iter().chain(CANDIDATES.iter().filter_map(|c| which(c)))
}

#[cfg(target_os = "linux")]
fn resolve(bin: &str) -> Option<String> {
    which(bin)
}

/// Application bundles looked for in /Applications and ~/Applications.
#[cfg(target_os = "macos")]
const APPS: &[&str] = &[
    "Zen.app",
    "Zen Browser.app",
    "Firefox.app",
    "LibreWolf.app",
    "Floorp.app",
    "Waterfox.app",
    "Chromium.app",
    "Google Chrome.app",
    "Yandex.app",
    "Brave Browser.app",
    "Vivaldi.app",
    "Microsoft Edge.app",
    "Thorium.app",
];

/// The program inside an application bundle (`Contents/MacOS/<CFBundleExecutable>`).
#[cfg(target_os = "macos")]
fn app_bin(app: &Path) -> Option<String> {
    let out = Command::new("defaults")
        .arg("read")
        .arg(app.join("Contents/Info"))
        .arg("CFBundleExecutable")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let exe = String::from_utf8(out.stdout).ok()?.trim().to_string();
    let bin = app.join("Contents/MacOS").join(exe);
    bin.is_file().then(|| bin.to_string_lossy().into_owned())
}

/// The bundle id that handles https links in `defaults read` of LaunchServices' `LSHandlers`:
/// entries are `{ … }` blocks, the handler is the `LSHandlerRoleAll` that is not `-` (the one
/// inside `LSHandlerPreferredVersions`).
#[cfg(target_os = "macos")]
fn https_handler(handlers: &str) -> Option<String> {
    handlers
        .split("},")
        .find(|e| e.lines().any(|l| l.trim() == "LSHandlerURLScheme = https;"))?
        .lines()
        .filter_map(|l| l.trim().strip_prefix("LSHandlerRoleAll = ")?.strip_suffix(';'))
        .map(|v| v.trim_matches('"'))
        .find(|v| *v != "-")
        .map(String::from)
}

/// Binary of the default browser: its bundle id from LaunchServices, the bundle by Spotlight.
#[cfg(target_os = "macos")]
fn default_browser_bin() -> Option<String> {
    let out = Command::new("defaults")
        .args(["read", "com.apple.LaunchServices/com.apple.launchservices.secure", "LSHandlers"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let id = https_handler(&String::from_utf8(out.stdout).ok()?)?;
    let out = Command::new("mdfind").arg(format!("kMDItemCFBundleIdentifier == '{id}'")).stderr(Stdio::null()).output().ok()?;
    let app = String::from_utf8(out.stdout).ok()?.lines().next()?.to_string();
    app_bin(Path::new(&app))
}

#[cfg(target_os = "macos")]
fn candidates() -> impl Iterator<Item = String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dirs = [PathBuf::from("/Applications"), PathBuf::from(home).join("Applications")];
    let apps = APPS.iter().flat_map(move |a| dirs.clone().map(|d| d.join(a))).filter(|p| p.is_dir());
    default_browser_bin().into_iter().chain(apps.filter_map(|p| app_bin(&p)))
}

/// `YMUSIC_BROWSER`: a program, or on macOS also an application bundle.
#[cfg(target_os = "macos")]
fn resolve(bin: &str) -> Option<String> {
    which(bin).or_else(|| app_bin(Path::new(bin)))
}

fn find_browser() -> Option<Browser> {
    if let Some(b) = crate::config::env_str("BROWSER") {
        let bin = resolve(&b)?;
        let kind = classify(&bin).unwrap_or(Kind::Chromium);
        return Some(Browser { bin, kind });
    }
    candidates().find_map(|bin| classify(&bin).map(|kind| Browser { bin, kind }))
}

// ---------------------------------------------------------------- browser login

struct Session {
    child: Child,
    profile: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// Ctrl+C (or SIGTERM, or the terminal closed) during browser login: the wait loops stop and
/// `Session` closes the browser and removes the profile with the Yandex cookies in it (the signals'
/// default action would exit without running `Drop`).
static CANCELLED: AtomicBool = AtomicBool::new(false);

fn check_cancel() -> Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        bail!("вход отменён");
    }
    Ok(())
}

/// A fresh private (0700) directory for the browser profile: it holds the Yandex session cookies.
/// The name is not predictable, and an existing path (or symlink) is never reused.
fn create_profile_dir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let mut last = None;
    for _ in 0..16 {
        let random = crate::api::random_u64();
        let dir = std::env::temp_dir().join(format!("ymusic-login-{}-{random:016x}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e).context("не удалось создать временный профиль браузера"),
        }
    }
    Err(last.map_or_else(|| anyhow!("не удалось создать временный профиль браузера"), anyhow::Error::from))
}

/// Our ends of the pipes a Chromium-based browser speaks CDP over, and its ends, to be closed here
/// once it has started.
struct Pipes {
    to_browser: PipeWriter,
    from_browser: PipeReader,
    theirs: (PipeReader, PipeWriter),
}

/// Gives the browser `cmd` starts the descriptors 3 (commands to it) and 4 (its replies and events)
/// that `--remote-debugging-pipe` expects.
fn debugging_pipes(cmd: &mut Command) -> Result<Pipes> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let (their_in, to_browser) = std::io::pipe()?;
    let (from_browser, their_out) = std::io::pipe()?;
    let (input, output) = (their_in.as_raw_fd(), their_out.as_raw_fd());
    // SAFETY: runs in the child between fork and exec and calls only fcntl and dup2, which are
    // async-signal-safe. The descriptors are first moved above 10 so that neither is overwritten
    // while the other is put in place; dup2 clears close-on-exec on the copies the browser gets.
    unsafe {
        cmd.pre_exec(move || {
            let (input, output) = (libc::fcntl(input, libc::F_DUPFD_CLOEXEC, 10), libc::fcntl(output, libc::F_DUPFD_CLOEXEC, 10));
            if input < 0 || output < 0 || libc::dup2(input, 3) < 0 || libc::dup2(output, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Pipes { to_browser, from_browser, theirs: (their_in, their_out) })
}

pub fn browser_login() -> Result<String> {
    let browser = find_browser().ok_or_else(|| {
        anyhow!("не найден поддерживаемый браузер (Firefox/Zen/LibreWolf или Chromium/Chrome/Brave/Яндекс Браузер); укажите его в YMUSIC_BROWSER")
    })?;
    let profile = create_profile_dir()?;
    let headless = crate::config::env("BROWSER_HEADLESS").is_some();
    let url = auth_url();

    let mut cmd = Command::new(&browser.bin);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut pipes = None;
    match browser.kind {
        Kind::Firefox => {
            if let Err(e) = std::fs::write(profile.join("user.js"), FIREFOX_PREFS) {
                let _ = std::fs::remove_dir_all(&profile);
                return Err(e.into());
            }
            cmd.arg("--profile").arg(&profile).args(["--no-remote", "--new-instance"]);
            // Any free port: the browser tells which one on stderr.
            cmd.args(["--remote-debugging-port", "0"]).stderr(Stdio::piped());
            if headless {
                cmd.arg("--headless");
            }
            cmd.arg("about:blank");
        }
        Kind::Chromium => {
            cmd.arg(format!("--user-data-dir={}", profile.display()));
            cmd.arg("--remote-debugging-pipe");
            cmd.args(["--no-first-run", "--no-default-browser-check", "--new-window"]);
            if headless {
                cmd.arg("--headless=new");
            }
            cmd.arg(&url);
            match debugging_pipes(&mut cmd) {
                Ok(p) => pipes = Some(p),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&profile);
                    return Err(e);
                }
            }
        }
    }
    let name = Path::new(&browser.bin).file_name().unwrap_or_default().to_string_lossy().into_owned();
    println!("\n  Открываю окно браузера ({name})… Войдите в аккаунт Яндекса.");
    println!("  После входа окно закроется само. Ctrl+C — отмена.");
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&profile);
            return Err(e).with_context(|| format!("не удалось запустить {}", browser.bin));
        }
    };
    let stderr = child.stderr.take();
    let mut session = Session { child, profile };
    CANCELLED.store(false, Ordering::Relaxed);
    let mut handlers = Vec::new();
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        handlers.push(unsafe { signal_hook::low_level::register(signal, || CANCELLED.store(true, Ordering::Relaxed)) }?);
    }
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let token = match (pipes, stderr) {
        (Some(pipes), _) => cdp_capture(pipes, &mut session.child, deadline),
        (None, Some(stderr)) => bidi_capture(stderr, &url, &mut session.child, deadline),
        (None, None) => Err(anyhow!("нет связи с браузером")),
    };
    for h in handlers {
        signal_hook::low_level::unregister(h);
    }
    // Ctrl+C reaches the browser too, so it may close first: report the cancel, not the closed window.
    token.map_err(|e| if CANCELLED.load(Ordering::Relaxed) { anyhow!("вход отменён") } else { e })
}

const FIREFOX_PREFS: &str = r#"
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("zen.welcome-screen.seen", true);
"#;

fn ws_connect(url: &str, child: &mut Child, deadline: Instant) -> Result<Ws> {
    let start = Instant::now();
    loop {
        match tungstenite::connect(url) {
            Ok((mut ws, _)) => {
                if let MaybeTlsStream::Plain(s) = ws.get_mut() {
                    s.set_read_timeout(Some(Duration::from_millis(400)))?;
                }
                return Ok(ws);
            }
            Err(e) => {
                check_cancel()?;
                if child.try_wait()?.is_some() {
                    bail!("браузер завершился при запуске");
                }
                if start.elapsed() > Duration::from_secs(30) || Instant::now() > deadline {
                    bail!("не удалось подключиться к браузеру: {e}");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn ws_send(ws: &mut Ws, v: Value) -> Result<()> {
    ws.send(Message::Text(v.to_string()))?;
    Ok(())
}

fn ws_recv(ws: &mut Ws) -> Result<Option<String>> {
    match ws.read() {
        Ok(Message::Text(t)) => Ok(Some(t.to_string())),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(_) => bail!("окно браузера закрыто до завершения входа"),
    }
}

/// A debugging connection to the browser: JSON messages both ways.
trait Channel {
    fn send_json(&mut self, v: Value) -> Result<()>;
    /// The next message, or `None` after a short wait without one.
    fn next_message(&mut self) -> Result<Option<String>>;
}

impl Channel for Ws {
    fn send_json(&mut self, v: Value) -> Result<()> {
        ws_send(self, v)
    }

    fn next_message(&mut self) -> Result<Option<String>> {
        ws_recv(self)
    }
}

/// CDP over the pipes: messages end in a NUL byte; a thread reads them.
struct Pipe {
    to_browser: PipeWriter,
    messages: Receiver<String>,
}

impl Pipe {
    fn new(to_browser: PipeWriter, from_browser: PipeReader) -> Pipe {
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(from_browser);
            loop {
                let mut msg = Vec::new();
                match r.read_until(0, &mut msg) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if msg.last() == Some(&0) {
                            msg.pop();
                        }
                        if tx.send(String::from_utf8_lossy(&msg).into_owned()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Pipe { to_browser, messages }
    }
}

impl Channel for Pipe {
    fn send_json(&mut self, v: Value) -> Result<()> {
        let mut msg = v.to_string().into_bytes();
        msg.push(0);
        self.to_browser.write_all(&msg).context("окно браузера закрыто до завершения входа")
    }

    fn next_message(&mut self) -> Result<Option<String>> {
        match self.messages.recv_timeout(Duration::from_millis(400)) {
            Ok(m) => Ok(Some(m)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("окно браузера закрыто до завершения входа"),
        }
    }
}

/// Reads messages until one of them contains the token; the request `tick` makes is sent every
/// half a second (its reply may hold the token too), and the one `react` makes to a message, if any.
fn wait_token(
    ch: &mut impl Channel,
    child: &mut Child,
    deadline: Instant,
    mut tick: impl FnMut() -> Value,
    mut react: impl FnMut(&str) -> Option<Value>,
) -> Result<String> {
    let mut last = Instant::now();
    loop {
        check_cancel()?;
        if Instant::now() > deadline {
            bail!("время ожидания входа истекло");
        }
        if child.try_wait()?.is_some() {
            bail!("окно браузера закрыто до завершения входа");
        }
        if let Some(text) = ch.next_message()? {
            if let Some(t) = extract_token(&text) {
                return Ok(t);
            }
            if let Some(request) = react(&text) {
                let _ = ch.send_json(request);
            }
        }
        if last.elapsed() > Duration::from_millis(500) {
            last = Instant::now();
            // A failed request shows up as a closed window on the next read.
            let _ = ch.send_json(tick());
        }
    }
}

/// The address of the WebDriver BiDi server, as a Firefox-based browser reports it on stderr:
/// `WebDriver BiDi listening on ws://127.0.0.1:<port>`. The rest of stderr is drained, so that
/// the browser never blocks on it.
fn bidi_address(stderr: std::process::ChildStderr, child: &mut Child) -> Result<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            if let Some(i) = line.find("ws://127.0.0.1:") {
                let _ = tx.send(line[i..].trim().to_string());
            }
        }
    });
    let start = Instant::now();
    loop {
        check_cancel()?;
        if child.try_wait()?.is_some() {
            bail!("браузер завершился при запуске");
        }
        if start.elapsed() > Duration::from_secs(30) {
            bail!("браузер не сообщил адрес WebDriver BiDi");
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(address) => return Ok(address),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("браузер не сообщил адрес WebDriver BiDi"),
        }
    }
}

/// Firefox-based browsers: WebDriver BiDi.
fn bidi_capture(stderr: std::process::ChildStderr, url: &str, child: &mut Child, deadline: Instant) -> Result<String> {
    let address = bidi_address(stderr, child)?;
    let mut ws = ws_connect(&format!("{address}/session"), child, deadline)?;
    ws_send(&mut ws, json!({"id": 1, "method": "session.new", "params": {"capabilities": {}}}))?;
    let events = ["browsingContext.navigationStarted", "browsingContext.fragmentNavigated", "browsingContext.load", "network.responseStarted"];
    ws_send(&mut ws, json!({"id": 2, "method": "session.subscribe", "params": {"events": events}}))?;
    ws_send(&mut ws, json!({"id": 3, "method": "browsingContext.getTree", "params": {}}))?;

    // Wait for the top-level context and navigate it to the login page.
    // The window may not exist yet right after start: poll, and create a tab if it never shows up.
    let start = Instant::now();
    let mut attempts = 0;
    let context = loop {
        if start.elapsed() > Duration::from_secs(20) {
            bail!("браузер не ответил на запрос WebDriver BiDi");
        }
        let Some(text) = ws_recv(&mut ws)? else { continue };
        let v: Value = serde_json::from_str(&text).unwrap_or_default();
        if v["id"] == 1 && v["type"] == "error" {
            bail!("BiDi: {}", v["message"].as_str().unwrap_or("session.new failed"));
        }
        if v["id"] == 3 {
            if let Some(c) = v["result"]["contexts"][0]["context"].as_str() {
                break c.to_string();
            }
            attempts += 1;
            std::thread::sleep(Duration::from_millis(300));
            let req = if attempts < 10 {
                json!({"id": 3, "method": "browsingContext.getTree", "params": {}})
            } else {
                json!({"id": 5, "method": "browsingContext.create", "params": {"type": "window"}})
            };
            ws_send(&mut ws, req)?;
        }
        if v["id"] == 5 {
            match v["result"]["context"].as_str() {
                Some(c) => break c.to_string(),
                None => bail!("BiDi: не удалось открыть окно"),
            }
        }
    };
    let navigate = json!({"id": 4, "method": "browsingContext.navigate", "params": {"context": context, "url": url}});
    ws_send(&mut ws, navigate.clone())?;

    let mut id = 100;
    let tick = || {
        id += 1;
        json!({"id": id, "method": "browsingContext.getTree", "params": {}})
    };
    // The browser's own start-up load of about:blank may still be under way and abort the
    // navigation (NS_BINDING_ABORTED, seen on a slow machine): repeat it.
    let mut retries = 0;
    let react = |text: &str| {
        let v: Value = serde_json::from_str(text).ok()?;
        if v["id"] != 4 || v["type"] != "error" || retries >= 5 {
            return None;
        }
        retries += 1;
        std::thread::sleep(Duration::from_millis(300));
        Some(navigate.clone())
    };
    wait_token(&mut ws, child, deadline, tick, react)
}

/// Chromium-based browsers: Chrome DevTools Protocol over the pipes. The browser has opened the
/// login page itself; the addresses of its tabs are watched for the token: the tabs' own
/// (`Target.*`) and their navigations (`Page.*`, which also tell a move to another `#…` within
/// the page).
fn cdp_capture(pipes: Pipes, child: &mut Child, deadline: Instant) -> Result<String> {
    // Without our copies of the browser's ends, its exit closes the pipe.
    drop(pipes.theirs);
    let mut pipe = Pipe::new(pipes.to_browser, pipes.from_browser);
    pipe.send_json(json!({"id": 1, "method": "Target.setDiscoverTargets", "params": {"discover": true}}))?;
    let attach = json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true});
    pipe.send_json(json!({"id": 2, "method": "Target.setAutoAttach", "params": attach}))?;
    let ids = std::cell::Cell::new(100);
    let next_id = || {
        ids.set(ids.get() + 1);
        ids.get()
    };
    let tick = || json!({"id": next_id(), "method": "Target.getTargets"});
    // Every tab attached to gets its page events turned on.
    let react = |text: &str| {
        let v: Value = serde_json::from_str(text).ok()?;
        if v["method"] != "Target.attachedToTarget" || v["params"]["targetInfo"]["type"] != "page" {
            return None;
        }
        let session = v["params"]["sessionId"].as_str()?;
        Some(json!({"id": next_id(), "sessionId": session, "method": "Page.enable"}))
    };
    wait_token(&mut pipe, child, deadline, tick, react)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse() {
        assert_eq!(
            super::parse_token("https://music.yandex.ru/#access_token=y0_ABCDEFGHIJKLMNOPQRSTUV&token_type=bearer").as_deref(),
            Some("y0_ABCDEFGHIJKLMNOPQRSTUV")
        );
        assert_eq!(super::parse_token("y0_XYZXYZXYZXYZXYZXYZXYZ").as_deref(), Some("y0_XYZXYZXYZXYZXYZXYZXYZ"));
        assert_eq!(super::parse_token("https://music.yandex.ru/"), None);
        assert_eq!(super::parse_token("short"), None);
    }

    #[test]
    fn classify() {
        use super::{classify, Kind};
        assert_eq!(classify("/usr/bin/zen-browser"), Some(Kind::Firefox));
        assert_eq!(classify("firefox"), Some(Kind::Firefox));
        assert_eq!(classify("google-chrome-stable"), Some(Kind::Chromium));
        assert_eq!(classify("yandex-browser"), Some(Kind::Chromium));
        assert_eq!(classify("flatpak"), None);
    }

    /// The descriptors `--remote-debugging-pipe` expects reach the child; messages go both ways.
    #[test]
    fn debugging_pipes() {
        use super::Channel;
        // A stand-in for the browser: sends back on 4 what comes on 3.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "cat <&3 >&4"]);
        let pipes = super::debugging_pipes(&mut cmd).unwrap();
        let mut child = cmd.spawn().unwrap();
        drop(pipes.theirs);
        let mut pipe = super::Pipe::new(pipes.to_browser, pipes.from_browser);
        pipe.send_json(serde_json::json!({"id": 1, "method": "Target.getTargets"})).unwrap();
        let echoed = loop {
            if let Some(m) = pipe.next_message().unwrap() {
                break m;
            }
        };
        assert_eq!(echoed, r#"{"id":1,"method":"Target.getTargets"}"#);
        // Our end closed: the child reads the end of input and exits, which closes its end too.
        let super::Pipe { to_browser, messages } = pipe;
        drop(to_browser);
        assert!(child.wait().unwrap().success());
        assert!(messages.recv().is_err());
    }

    #[test]
    fn profile_dir() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (super::create_profile_dir().unwrap(), super::create_profile_dir().unwrap());
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = (std::fs::remove_dir(a), std::fs::remove_dir(b));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn https_handler() {
        let handlers = r#"(
        {
        LSHandlerContentType = "public.html";
        LSHandlerRoleAll = "com.apple.safari";
    },
        {
        LSHandlerPreferredVersions =         {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "org.mozilla.firefox";
        LSHandlerURLScheme = https;
    },
        {
        LSHandlerRoleAll = "com.google.chrome";
        LSHandlerURLScheme = http;
    }
)"#;
        assert_eq!(super::https_handler(handlers).as_deref(), Some("org.mozilla.firefox"));
        assert_eq!(super::https_handler("()"), None);
    }
}
