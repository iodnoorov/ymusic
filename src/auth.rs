//! Login: through a browser window (token is captured automatically) or by pasting a token.
//!
//! Browser login opens the Yandex OAuth page in a separate browser instance with a temporary
//! profile and remote debugging enabled (WebDriver BiDi for Firefox-based browsers, CDP for
//! Chromium-based ones). After the user signs in, Yandex redirects to
//! `https://music.yandex.ru/#access_token=…`; the token is read from that URL and the window is closed.

use crate::api::CLIENT_ID;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Ctrl+C during browser login: the wait loops stop and `Session` closes the browser and
/// removes the profile (the default SIGINT action would exit without running `Drop`).
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
    use std::hash::{BuildHasher, Hasher};
    use std::os::unix::fs::DirBuilderExt;
    let mut last = None;
    for _ in 0..16 {
        // RandomState is seeded from the OS random source: a cheap unpredictable suffix.
        let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let dir = std::env::temp_dir().join(format!("ymusic-login-{}-{random:016x}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e).context("не удалось создать временный профиль браузера"),
        }
    }
    Err(last.map_or_else(|| anyhow!("не удалось создать временный профиль браузера"), anyhow::Error::from))
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn browser_login() -> Result<String> {
    let browser = find_browser().ok_or_else(|| {
        anyhow!("не найден поддерживаемый браузер (Firefox/Zen/LibreWolf или Chromium/Chrome/Brave/Яндекс Браузер); укажите его в YMUSIC_BROWSER")
    })?;
    let port = free_port()?;
    let profile = create_profile_dir()?;
    let headless = crate::config::env("BROWSER_HEADLESS").is_some();

    let mut cmd = Command::new(&browser.bin);
    match browser.kind {
        Kind::Firefox => {
            if let Err(e) = std::fs::write(profile.join("user.js"), FIREFOX_PREFS) {
                let _ = std::fs::remove_dir_all(&profile);
                return Err(e.into());
            }
            cmd.arg("--profile").arg(&profile).args(["--no-remote", "--new-instance"]);
            cmd.arg("--remote-debugging-port").arg(port.to_string());
            if headless {
                cmd.arg("--headless");
            }
            cmd.arg("about:blank");
        }
        Kind::Chromium => {
            cmd.arg(format!("--user-data-dir={}", profile.display()));
            cmd.arg(format!("--remote-debugging-port={port}"));
            cmd.args(["--no-first-run", "--no-default-browser-check", "--new-window"]);
            if headless {
                cmd.arg("--headless=new");
            }
            cmd.arg("about:blank");
        }
    }
    let name = Path::new(&browser.bin).file_name().unwrap_or_default().to_string_lossy().into_owned();
    println!("\n  Открываю окно браузера ({name})… Войдите в аккаунт Яндекса.");
    println!("  После входа окно закроется само. Ctrl+C — отмена.");
    let child = match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&profile);
            return Err(e).with_context(|| format!("не удалось запустить {}", browser.bin));
        }
    };
    let mut session = Session { child, profile };
    CANCELLED.store(false, Ordering::Relaxed);
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    let sigint = unsafe {
        signal_hook::low_level::register(signal_hook::consts::SIGINT, || CANCELLED.store(true, Ordering::Relaxed))
    }?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let url = auth_url();
    let token = match browser.kind {
        Kind::Firefox => bidi_capture(port, &url, &mut session.child, deadline),
        Kind::Chromium => cdp_capture(port, &url, &mut session.child, deadline),
    };
    signal_hook::low_level::unregister(sigint);
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

/// Reads messages until one of them contains the token. `tick` runs periodically.
fn wait_token(
    ws: &mut Ws,
    child: &mut Child,
    deadline: Instant,
    mut tick: impl FnMut(&mut Ws) -> Option<String>,
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
        if let Some(text) = ws_recv(ws)? {
            if let Some(t) = extract_token(&text) {
                return Ok(t);
            }
        }
        if last.elapsed() > Duration::from_millis(500) {
            last = Instant::now();
            if let Some(t) = tick(ws) {
                return Ok(t);
            }
        }
    }
}

/// Firefox-based browsers: WebDriver BiDi.
fn bidi_capture(port: u16, url: &str, child: &mut Child, deadline: Instant) -> Result<String> {
    let mut ws = ws_connect(&format!("ws://127.0.0.1:{port}/session"), child, deadline)?;
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
    ws_send(&mut ws, json!({"id": 4, "method": "browsingContext.navigate", "params": {"context": context, "url": url}}))?;

    let mut id = 100;
    wait_token(&mut ws, child, deadline, |ws| {
        id += 1;
        let _ = ws_send(ws, json!({"id": id, "method": "browsingContext.getTree", "params": {}}));
        None
    })
}

/// Chromium-based browsers: Chrome DevTools Protocol.
fn cdp_capture(port: u16, url: &str, child: &mut Child, deadline: Instant) -> Result<String> {
    let list_url = format!("http://127.0.0.1:{port}/json/list");
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(2)).build();
    let start = Instant::now();
    let ws_url = loop {
        check_cancel()?;
        if child.try_wait()?.is_some() {
            bail!("браузер завершился при запуске");
        }
        if start.elapsed() > Duration::from_secs(30) {
            bail!("не удалось подключиться к браузеру (CDP)");
        }
        if let Some(v) = agent.get(&list_url).call().ok().and_then(|r| r.into_json::<Value>().ok()) {
            let page = v.as_array().into_iter().flatten().find(|p| p["type"] == "page");
            if let Some(ws) = page.and_then(|p| p["webSocketDebuggerUrl"].as_str()) {
                break ws.to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut ws = ws_connect(&ws_url, child, deadline)?;
    ws_send(&mut ws, json!({"id": 1, "method": "Network.enable"}))?;
    ws_send(&mut ws, json!({"id": 2, "method": "Page.enable"}))?;
    ws_send(&mut ws, json!({"id": 3, "method": "Page.navigate", "params": {"url": url}}))?;
    wait_token(&mut ws, child, deadline, |_| {
        let text = agent.get(&list_url).call().ok()?.into_string().ok()?;
        extract_token(&text)
    })
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
