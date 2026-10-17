mod api;
mod app;
mod auth;
mod clipboard;
mod config;
mod eq;
mod player;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load();

    match cli.command {
        Some(Cmd::Login { token, browser }) => return login(&mut cfg, token, browser),
        Some(Cmd::Logout) => {
            cfg.token = None;
            cfg.save()?;
            println!("Токен удалён.");
            return Ok(());
        }
        Some(Cmd::Whoami) => {
            let mut api = api::Api::new(&token(&mut cfg)?);
            let a = api.account_status()?;
            println!("{} ({}) uid={} Плюс: {}", a.name, a.login, a.uid, if a.has_plus { "да" } else { "нет" });
            return Ok(());
        }
        None => {}
    }

    let token = token(&mut cfg)?;
    let mut api = api::Api::new(&token);
    let account = api.account_status().context("не удалось войти (попробуйте `ymusic login`)")?;

    let (tx, rx) = mpsc::channel();
    let player = player::Player::spawn(tx.clone(), cfg.volume)?;
    let mut app = app::App::new(api, account, tx, player, cfg.volume);
    app.set_eq(eq::index(&cfg.eq));
    app.set_crossfade(cfg.crossfade);
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

    let mut terminal = ratatui::init();
    // Must run after entering the alternate screen and before reading events.
    app.picker = Some(
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::from_fontsize((8, 16))),
    );
    let res = (|| -> Result<()> {
        // Redraw only when something changed: immediately on input/messages,
        // otherwise at 5 fps while something animates and 1 fps when idle.
        let mut dirty = true;
        let mut last_draw = Instant::now();
        while !app.quit {
            let frame = Duration::from_millis(if app.is_animating() { 200 } else { 1000 });
            if dirty || last_draw.elapsed() >= frame {
                terminal.draw(|f| ui::draw(f, &mut app))?;
                last_draw = Instant::now();
                dirty = false;
            }
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                    Event::Resize(..) => app.on_resize(),
                    _ => continue,
                }
                dirty = true;
            }
            while let Ok(m) = rx.try_recv() {
                // Playback position is shown by the regular animation frames.
                dirty |= !matches!(m, app::Msg::Player(player::PlayerEvent::TimePos(_)));
                app.on_msg(m);
            }
        }
        Ok(())
    })();
    ratatui::restore();

    cfg.volume = app.save_volume();
    cfg.eq = eq::PRESETS[app.eq].id.to_string();
    cfg.quality = api::QUALITIES[app.quality].id.to_string();
    cfg.crossfade = app.crossfade;
    if app.icons != icons {
        cfg.icons = app.icons;
    }
    if app.logout {
        cfg.token = None;
    }
    let _ = cfg.save();
    if app.logout && res.is_ok() {
        println!("Вы вышли из аккаунта. Войти снова: ymusic login");
    }
    res
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
