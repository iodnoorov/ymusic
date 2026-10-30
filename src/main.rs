mod app;
mod auth;
mod clipboard;
mod config;
mod effect;
mod input;
mod keymap;
mod media;
mod title;
mod ui;

use ymusic_api as api;
use ymusic_core::{library, queue};
use ymusic_player as player;
use ymusic_player::eq;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "ymusic", version, about = "Яндекс Музыка в терминале: Моя волна, лайки, станции")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Сразу запустить Мою волну
    #[arg(short, long)]
    wave: bool,
    /// Сразу запустить станцию, например genre:rock, mood:calm, activity:workout
    #[arg(short, long, value_name = "ID")]
    station: Option<String>,
    /// Без иконок Nerd Font (если шрифт не установлен)
    #[arg(long)]
    no_icons: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Войти в аккаунт (меню: через браузер или по токену)
    Login {
        /// Использовать готовый OAuth-токен
        #[arg(short, long, conflicts_with = "browser")]
        token: Option<String>,
        /// Сразу войти через окно браузера (токен подхватится автоматически)
        #[arg(short, long)]
        browser: bool,
    },
    /// Выйти (удалить сохранённый токен)
    Logout,
    /// Показать текущий аккаунт
    Whoami,
}

/// Locking that goes on after a panic in another thread holding the lock: the state under these
/// locks is updated in small steps, and one failed thread should not bring the rest down with a
/// second panic.
trait Lock<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> Lock<T> for std::sync::Mutex<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load();

    match cli.command {
        Some(Cmd::Login { token, browser }) => return login(&mut cfg, token, browser),
        Some(Cmd::Logout) => {
            cfg.token = None;
            cfg.save()?;
            println!("Токен удалён.");
            warn_env_token();
            return Ok(());
        }
        Some(Cmd::Whoami) => {
            let mut api = api::Api::new(&token(&mut cfg)?);
            api.web = cfg.settings.web_api;
            let a = api.account_status()?;
            println!("{} ({}) uid={} Плюс: {}", a.name, a.login, a.uid, if a.has_plus { "да" } else { "нет" });
            return Ok(());
        }
        None => {}
    }

    let token = token(&mut cfg)?;
    let mut api = api::Api::new(&token);
    api.web = cfg.settings.web_api;
    let account = api.account_status().context("не удалось войти (попробуйте `ymusic login`)")?;

    let (tx, rx) = mpsc::channel();
    // `YMUSIC_OUTPUT` applies to this run only, as `--no-icons` below.
    let output_env = config::env_str("OUTPUT").map(|o| app::Output::from_id(Some(&o)));
    let output = output_env.unwrap_or(cfg.settings.output);
    let options = player::Options {
        output: output.id(),
        alsa_device: config::env_str("ALSA_DEVICE"),
        tls: api::tls_config(),
    };
    let player = player::Player::spawn(tx.clone(), cfg.settings.volume, options)?;
    let (remote, input_tx, signal_tx) = (tx.clone(), tx.clone(), tx.clone());
    let mut runtime = effect::Runtime::new(player, tx);
    let s = &cfg.settings;
    let mut app = app::App::new(api, account, s.volume);
    app.start();
    app.set_eq(eq::index(s.eq.id));
    app.set_crossfade(s.crossfade);
    app.set_art(s.art.unwrap_or(if cfg.covers { app::Art::Cover } else { app::Art::None }));
    app.finish_track = s.finish_track;
    app.output = output;
    app.hide_account = s.hide_account;
    app.remaining_time = s.remaining_time;
    if let Some(session) = &s.lastfm {
        app.lastfm = app::Lastfm::On(session.clone());
    }
    app.restore_scrobbles(cfg.lastfm_queue.clone());
    app.restore_podcast_progress(cfg.podcast_progress.clone());
    app.queue.set_repeat(s.repeat);
    // `--no-icons` and `YMUSIC_NO_ICONS` apply to this run only; the config keeps its value
    // unless icons are switched in the settings window.
    let no_icons = cli.no_icons || config::env("NO_ICONS").is_some();
    app.set_icons(s.icons && !no_icons);
    let run = RunOnly { icons: no_icons.then_some(false), output: output_env };
    app.quality = api::quality_index(s.quality.id);
    if let Some(id) = cli.station {
        app.start_station_id(&id);
    } else if cli.wave {
        app.start_station_id(api::MY_WAVE);
    }
    runtime.run(app.take_effects(), &app.task_api(), app.generation());

    // A changed window size and SIGTERM wake the loop: the screen is redrawn at once; SIGTERM ends
    // the loop like `q` (the sound stops, settings are saved). SIGHUP keeps its default action:
    // the sound stops with the process, and the settings are saved by then.
    {
        use signal_hook::consts::{SIGTERM, SIGWINCH};
        let mut signals = signal_hook::iterator::Signals::new([SIGWINCH, SIGTERM])?;
        std::thread::Builder::new().name("signals".into()).spawn(move || {
            for signal in signals.forever() {
                let _ = signal_tx.send(if signal == SIGTERM { app::Msg::Terminate } else { app::Msg::Resized });
            }
        })?;
    }

    let mut terminal = ratatui::init();
    // Pasted text arrives as one event instead of a stream of hotkeys.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    // Must run after entering the alternate screen and before reading events.
    app.picker = Some(query_picker());
    // Keys are read here rather than by crossterm (it drops the physical key the terminal tells),
    // in a thread of their own: they come as messages like everything else.
    input::Reader::new().spawn(move |i| {
        let _ = input_tx.send(app::Msg::Input(i));
    });
    // A terminal that does not know a query may print part of it (Apple's Terminal shows the
    // kitty graphics one as text), and the first frame only writes cells that are not blank.
    terminal.clear()?;
    let mut kitty = false;
    let mut title = title::Title::start();
    // Media keys and desktop player widgets; their commands come back as messages.
    let mut media = media::Media::start(move |r| {
        let _ = remote.send(app::Msg::Remote(r));
    });
    let res = (|| -> Result<()> {
        // Redraw only when something changed: at once on input and messages, otherwise at
        // 12.5 fps while the spectrum moves, 5 fps while something animates and 1 fps when idle.
        // In between the loop sleeps until a message comes.
        let truecolor = ui::truecolor();
        let mut dirty = true;
        let mut last_draw = Instant::now();
        let mut saving = config::SaveState::new(&cfg.settings);
        let mut save_error = None;
        while !app.quit {
            if std::mem::take(&mut app.clear_images) {
                delete_images(app.picker.as_ref());
                // Full redraw: the screen is erased, which also removes sixel images.
                terminal.clear()?;
                dirty = true;
            }
            let frame = Duration::from_millis(match () {
                _ if app.spectrum_moving() => 80,
                _ if app.is_animating() => 200,
                _ => 1000,
            });
            title.set(&app.window_title());
            if dirty || last_draw.elapsed() >= frame {
                terminal.draw(|f| {
                    ui::draw(f, &mut app);
                    if !truecolor {
                        ui::to_256(f.buffer_mut());
                    }
                })?;
                last_draw = Instant::now();
                dirty = false;
            }
            // Until the next frame, or the moment to save changed settings.
            let mut wait = frame.saturating_sub(last_draw.elapsed());
            if let Some(until_save) = saving.wait(Instant::now()) {
                wait = wait.min(until_save);
            }
            // On macOS «Исполняется» delivers its commands on this thread's run loop, which
            // `media.sync` turns: it must come round often.
            #[cfg(target_os = "macos")]
            let wait = wait.min(Duration::from_millis(50));
            let first = match rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            for m in first.into_iter().chain(rx.try_iter()) {
                match m {
                    app::Msg::Input(input::Input::KittyProtocol) => {
                        if !std::mem::replace(&mut kitty, true) {
                            input::kitty_on();
                        }
                    }
                    app::Msg::Input(i) => {
                        dirty = true;
                        match i {
                            input::Input::Key(k) => app.on_key(k),
                            input::Input::Paste(s) => app.on_paste(&s),
                            input::Input::KittyProtocol => {}
                        }
                    }
                    app::Msg::Resized => {
                        app.on_resize();
                        dirty = true;
                    }
                    app::Msg::Terminate => app.quit = true,
                    m => {
                        // Playback position is shown by the regular animation frames.
                        dirty |= !matches!(m, app::Msg::Player(player::PlayerEvent::TimePos(_)));
                        app.on_msg(m);
                    }
                }
            }
            app.tick();
            let now = Instant::now();
            saving.observe(
                settings_of(&app, &cfg.settings, run),
                app.scrobbles_revision().wrapping_add(app.podcast_progress_revision()), now,
            );
            if saving.ready(now) {
                match saving.flush(&mut cfg, app.pending_scrobbles(), app.pending_podcast_progress(), now, Config::save) {
                    Ok(()) => save_error = None,
                    Err(e) => {
                        let message = format!("Не удалось сохранить настройки: {e:#}");
                        if save_error.as_ref() != Some(&message) {
                            app.set_error(&message);
                            save_error = Some(message);
                            dirty = true;
                        }
                    }
                }
            }
            // What the keys and messages above asked for.
            runtime.run(app.take_effects(), &app.task_api(), app.generation());
            let (state, position) = app.media_state();
            media.sync(state, position);
        }
        Ok(())
    })();
    if kitty {
        input::kitty_off();
    }
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    title.restore();
    ratatui::restore();

    // Keep the last checkpoint before stopping output. Unsent progress remains durable below.
    app.capture_podcast_progress();
    runtime.player.apply(player::PlayerCmd::Stop);
    let history = app.quit_play();
    let pending = app.pending_podcast_progress();
    if history.is_some() || !pending.items.is_empty() {
        let (done_tx, done_rx) = mpsc::channel();
        if let Some(report) = history {
            let api = app.api.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let _ = api.send_play(&report);
                let _ = done_tx.send(None);
            });
        }
        if !pending.items.is_empty() {
            let api = app.api.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                for report in pending.items {
                    let result = api.podcast_save_progress(&report).map_err(|e| api::ApiError::from_anyhow(&e));
                    if done_tx.send(Some((report, result))).is_err() { break; }
                }
            });
        }
        drop(done_tx);
        let until = Instant::now() + Duration::from_secs(2);
        while let Ok(result) = done_rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            if let Some((report, result)) = result { app.podcast_progress_saved(&report, result); }
        }
    }

    cfg.settings = settings_of(&app, &cfg.settings, run);
    cfg.lastfm_queue = app.pending_scrobbles();
    cfg.podcast_progress = app.pending_podcast_progress();
    if app.logout {
        cfg.token = None;
    }
    let saved = cfg.save().context("не удалось сохранить настройки при выходе");
    if app.logout && res.is_ok() && saved.is_ok() {
        println!("Вы вышли из аккаунта. Войти снова: ymusic login");
        warn_env_token();
    }
    if let Err(e) = saved {
        if res.is_ok() { return Err(e); }
        eprintln!("{e:#}");
    }
    match app.exit_error.take() {
        Some(e) if res.is_ok() => Err(anyhow::anyhow!(e)),
        _ => res,
    }
}

/// Asks the terminal which image protocol it supports. ratatui-image reads the answer in its own
/// thread, which keeps reading stdin until the final status reply arrives, swallowing key presses
/// meanwhile; after a 1 s timeout it falls back to halfblocks but leaves that thread running.
/// Inside tmux the query goes through passthrough, so without an attached client (or with an outer
/// terminal that does not answer) the reply never comes. Then the status is requested directly:
/// the nearest terminal (tmux itself) answers and the thread ends. If the thread is already gone,
/// crossterm drops the extra reply.
fn query_picker() -> ratatui_image::picker::Picker {
    use std::io::Write;
    let start = Instant::now();
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::from_fontsize((8, 16)));
    if start.elapsed() >= Duration::from_millis(900) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[5n").and_then(|()| out.flush());
        // Let the reply reach that thread before the event loop starts reading stdin.
        std::thread::sleep(Duration::from_millis(100));
    }
    picker
}

/// Removes the images from the terminal: with the kitty protocol an image stays on screen until it
/// is deleted, even when the text under it is redrawn.
fn delete_images(picker: Option<&ratatui_image::picker::Picker>) {
    use ratatui_image::picker::ProtocolType;
    use std::io::Write;
    if picker.is_none_or(|p| p.protocol_type() != ProtocolType::Kitty) {
        return;
    }
    let cmd = "\x1b_Ga=d,d=A\x1b\\";
    let mut out = std::io::stdout();
    let _ = if std::env::var_os("TMUX").is_some() {
        // Through tmux passthrough: ESC inside is doubled.
        write!(out, "\x1bPtmux;{}\x1b\\", cmd.replace('\x1b', "\x1b\x1b"))
    } else {
        out.write_all(cmd.as_bytes())
    };
    let _ = out.flush();
}

/// Values the command line or the environment set for this run only (`--no-icons`,
/// `YMUSIC_OUTPUT`): while the program keeps them, the config keeps its own.
#[derive(Clone, Copy)]
struct RunOnly {
    icons: Option<bool>,
    output: Option<app::Output>,
}

/// The value to save: the program's, unless it is still the one set for this run only.
fn own<T: PartialEq>(now: T, run_only: Option<T>, saved: T) -> T {
    if run_only.is_some_and(|r| r == now) { saved } else { now }
}

/// The settings as they are in the program now.
fn settings_of(app: &app::App, saved: &config::Settings, run: RunOnly) -> config::Settings {
    config::Settings {
        volume: app.save_volume(),
        icons: own(app.icons, run.icons, saved.icons),
        art: Some(app.art),
        eq: &eq::PRESETS[app.eq],
        quality: &api::QUALITIES[app.quality],
        crossfade: app.crossfade,
        finish_track: app.finish_track,
        repeat: app.queue.repeat(),
        web_api: app.api.web,
        output: own(app.output, run.output, saved.output),
        hide_account: app.hide_account,
        remaining_time: app.remaining_time,
        lastfm: app.lastfm_session().cloned(),
    }
}

/// Logout removes only the saved token: one from the environment keeps working.
fn warn_env_token() {
    if config::env_str("TOKEN").is_some_and(|t| !t.is_empty()) {
        println!("Задан YMUSIC_TOKEN: пока переменная есть, программа входит с этим токеном.");
    }
}

fn token(cfg: &mut Config) -> Result<String> {
    if let Some(t) = config::env_str("TOKEN").filter(|t| !t.is_empty()) {
        return Ok(t);
    }
    if let Some(t) = cfg.token.clone() {
        return Ok(t);
    }
    println!("Вы не вошли в аккаунт.");
    login(cfg, None, false)?;
    cfg.token.clone().context("нет токена")
}

fn login(cfg: &mut Config, token: Option<String>, browser: bool) -> Result<()> {
    let token = match token {
        Some(t) => auth::parse_token(&t).context("некорректный токен")?,
        None if browser => auth::browser_login()?,
        None => auth::interactive_login()?,
    };
    let mut api = api::Api::new(&token);
    let acc = api.account_status().context("токен не подошёл")?;
    cfg.token = Some(token);
    cfg.save()?;
    println!(
        "\n  ✓ Вход выполнен: {} ({}){}\n  Токен сохранён в {}",
        acc.name,
        acc.login,
        if acc.has_plus { " · Плюс" } else { " · без Плюса (только 30-секундные превью)" },
        config::path().display()
    );
    Ok(())
}
