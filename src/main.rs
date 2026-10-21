mod api;
mod app;
mod auth;
mod clipboard;
mod config;
mod eq;
mod input;
mod mpris;
mod player;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
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
            api.web = cfg.web_api;
            let a = api.account_status()?;
            println!("{} ({}) uid={} Плюс: {}", a.name, a.login, a.uid, if a.has_plus { "да" } else { "нет" });
            return Ok(());
        }
        None => {}
    }

    let token = token(&mut cfg)?;
    let mut api = api::Api::new(&token);
    api.web = cfg.web_api;
    let account = api.account_status().context("не удалось войти (попробуйте `ymusic login`)")?;

    let (tx, rx) = mpsc::channel();
    let player = player::Player::spawn(tx.clone(), cfg.volume)?;
    let remote = tx.clone();
    let mut app = app::App::new(api, account, tx, Box::new(player), cfg.volume);
    app.start();
    app.set_eq(eq::index(&cfg.eq));
    app.set_crossfade(cfg.crossfade);
    let art = app::Art::from_id(&cfg.art).unwrap_or(if cfg.covers { app::Art::Cover } else { app::Art::None });
    app.set_art(art);
    app.finish_track = cfg.finish_track;
    // `--no-icons` and `YMUSIC_NO_ICONS` apply to this run only; the config keeps its value
    // unless icons are switched in the settings window.
    let icons = cfg.icons && !cli.no_icons && config::env("NO_ICONS").is_none();
    app.set_icons(icons);
    app.quality = api::quality_index(&cfg.quality);
    if let Some(id) = cli.station {
        app.start_station_id(&id);
    } else if cli.wave {
        app.start_station_id(api::MY_WAVE);
    }

    // SIGTERM ends the loop like `q`: the sound stops, settings are saved. SIGHUP keeps its default
    // action: with the terminal gone crossterm spins on EOF and the loop would never see a flag;
    // the sound stops with the process and settings are already saved by then.
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;

    let mut terminal = ratatui::init();
    // Pasted text arrives as one event instead of a stream of hotkeys.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    // Must run after entering the alternate screen and before reading events.
    app.picker = Some(query_picker());
    // Keys are read here rather than by crossterm: it drops the physical key the terminal tells.
    let mut keys = input::Reader::new();
    let mut size = ratatui::crossterm::terminal::size()?;
    // Media keys and desktop player widgets; their commands come back as messages.
    let mut mpris = mpris::Mpris::start(move |r| {
        let _ = remote.send(app::Msg::Remote(r));
    });
    let res = (|| -> Result<()> {
        // Redraw only when something changed: immediately on input/messages,
        // otherwise at 5 fps while something animates and 1 fps when idle.
        let mut dirty = true;
        let mut last_draw = Instant::now();
        let mut last_input = Instant::now();
        while !app.quit && !stop.load(Ordering::Relaxed) {
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
            if dirty || last_draw.elapsed() >= frame {
                terminal.draw(|f| ui::draw(f, &mut app))?;
                last_draw = Instant::now();
                dirty = false;
            }
            let inputs = keys.read(Duration::from_millis(50))?;
            if !inputs.is_empty() {
                dirty = true;
                last_input = Instant::now();
            }
            for i in inputs {
                match i {
                    input::Input::Key(k) => app.on_key(k),
                    input::Input::Paste(s) => app.on_paste(&s),
                }
            }
            let now = ratatui::crossterm::terminal::size()?;
            if now != size {
                size = now;
                app.on_resize();
                dirty = true;
            }
            // Changed settings are saved once the keys are left alone for a moment, so they
            // survive a crash or kill; not on every volume step.
            if last_input.elapsed() > Duration::from_secs(2) && sync_settings(&mut cfg, &app, icons) {
                let _ = cfg.save();
            }
            while let Ok(m) = rx.try_recv() {
                // Playback position is shown by the regular animation frames.
                dirty |= !matches!(m, app::Msg::Player(player::PlayerEvent::TimePos(_)));
                app.on_msg(m);
            }
            app.tick();
            let (state, position) = app.mpris_state();
            mpris.sync(state, position);
        }
        Ok(())
    })();
    keys.finish();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();

    // The listening history learns where the track stopped; a slow network does not hold the exit long.
    if let Some(report) = app.quit_play() {
        let (done_tx, done_rx) = mpsc::channel();
        let api = app.api.clone();
        std::thread::spawn(move || {
            let _ = api.send_play(&report);
            let _ = done_tx.send(());
        });
        let _ = done_rx.recv_timeout(Duration::from_secs(2));
    }

    sync_settings(&mut cfg, &app, icons);
    if app.logout {
        cfg.token = None;
    }
    let _ = cfg.save();
    if app.logout && res.is_ok() {
        println!("Вы вышли из аккаунта. Войти снова: ymusic login");
        warn_env_token();
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

/// Copies the settings changed in the program into the config; true if any of them changed.
/// `icons`: the value this run started with (`--no-icons` is not saved unless switched by hand).
fn sync_settings(cfg: &mut Config, app: &app::App, icons: bool) -> bool {
    let volume = app.save_volume();
    let eq = eq::PRESETS[app.eq].id;
    let quality = api::QUALITIES[app.quality].id;
    let icons = if app.icons != icons { app.icons } else { cfg.icons };
    let changed = cfg.volume != volume
        || cfg.eq != eq
        || cfg.quality != quality
        || cfg.crossfade != app.crossfade
        || cfg.art != app.art.id()
        || cfg.finish_track != app.finish_track
        || cfg.web_api != app.api.web
        || cfg.icons != icons;
    cfg.volume = volume;
    cfg.eq = eq.to_string();
    cfg.quality = quality.to_string();
    cfg.crossfade = app.crossfade;
    cfg.art = app.art.id().to_string();
    cfg.finish_track = app.finish_track;
    cfg.web_api = app.api.web;
    cfg.icons = icons;
    changed
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
