//! Rendering.

use crate::api::Track;
use crate::app::{App, Modal, Setting, Source, StationRow, Tab};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, HighlightSpacing, List, ListItem, Padding, Paragraph, Row, Table, TableState,
    Wrap,
};
use ratatui::Frame;
use ratatui_image::{FilterType, Resize, StatefulImage};
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------- theme

const ACCENT: Color = Color::Rgb(255, 219, 77);
const HEART: Color = Color::Rgb(255, 77, 106);
const ERROR: Color = Color::Rgb(255, 99, 99);
const SUB: Color = Color::Rgb(160, 160, 176);
const MUTED: Color = Color::Rgb(98, 98, 116);
const BORDER: Color = Color::Rgb(62, 62, 78);
const SEL_BG: Color = Color::Rgb(42, 42, 56);
const ON_ACCENT: Color = Color::Rgb(20, 20, 24);

// ---------------------------------------------------------------- icons

pub struct Icons {
    logo: &'static str,
    wave: &'static str,
    stations: &'static str,
    liked: &'static str,
    search: &'static str,
    play: &'static str,
    pause: &'static str,
    next: &'static str,
    prev: &'static str,
    heart: &'static str,
    vol_hi: &'static str,
    vol_mid: &'static str,
    vol_lo: &'static str,
    vol_off: &'static str,
    album: &'static str,
    user: &'static str,
    plus: &'static str,
    note: &'static str,
    info: &'static str,
    settings: &'static str,
    error: &'static str,
    keys: &'static str,
    genre: &'static str,
    mood: &'static str,
    activity: &'static str,
    epoch: &'static str,
    other: &'static str,
    clock: &'static str,
    queue: &'static str,
    link: &'static str,
}

/// JetBrainsMono Nerd Font (Material Design icons).
const NERD: Icons = Icons {
    logo: "󰝚",
    wave: "󱑽",
    stations: "󰐹",
    liked: "󰋑",
    search: "󰍉",
    play: "󰐊",
    pause: "󰏤",
    next: "󰒭",
    prev: "󰒮",
    heart: "󰋑",
    vol_hi: "󰕾",
    vol_mid: "󰖀",
    vol_lo: "󰕿",
    vol_off: "󰖁",
    album: "󰀥",
    user: "󰀉",
    plus: "󰆥",
    note: "󰎇",
    info: "󰋽",
    settings: "󰒓",
    error: "󰗖",
    keys: "󰌌",
    genre: "󰋄",
    mood: "󰇵",
    activity: "󰜎",
    epoch: "󰃰",
    other: "󰐹",
    clock: "󰅐",
    queue: "󰲸",
    link: "󰌹",
};

/// Fallback for terminals without a Nerd Font.
const PLAIN: Icons = Icons {
    logo: "♫",
    wave: "≋",
    stations: "◉",
    liked: "♥",
    search: "⌕",
    play: "▶",
    pause: "‖",
    next: "»",
    prev: "«",
    heart: "♥",
    vol_hi: "◢",
    vol_mid: "◢",
    vol_lo: "◢",
    vol_off: "×",
    album: "◎",
    user: "@",
    plus: "✦",
    note: "♪",
    info: "•",
    settings: "⚙",
    error: "!",
    keys: "⌨",
    genre: "♪",
    mood: "☺",
    activity: "⚑",
    epoch: "◷",
    other: "◉",
    clock: "◷",
    queue: "≡",
    link: "↗",
};

/// Nerd Font icons or plain Unicode symbols; switched from the settings window.
static NERD_ICONS: AtomicBool = AtomicBool::new(true);

pub fn set_nerd_icons(nerd: bool) {
    NERD_ICONS.store(nerd, Ordering::Relaxed);
}

fn ic() -> &'static Icons {
    if NERD_ICONS.load(Ordering::Relaxed) {
        &NERD
    } else {
        &PLAIN
    }
}

// ---------------------------------------------------------------- helpers

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn equalizer(tick: u64) -> &'static str {
    const FRAMES: [&str; 6] = ["▁▃▅", "▂▅▃", "▃▂▆", "▅▁▄", "▆▃▂", "▄▆▁"];
    FRAMES[tick as usize % FRAMES.len()]
}

fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick as usize % FRAMES.len()]
}

fn panel<'a>(icon: &'a str, title: impl Into<String>) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(icon, Style::new().fg(ACCENT)),
            Span::styled(format!(" {} ", title.into()), Style::new().fg(Color::Reset).bold()),
        ]))
        .padding(Padding::horizontal(1))
}

fn key_hint(k: &str, d: &str) -> [Span<'static>; 2] {
    [
        Span::styled(k.to_string(), Style::new().fg(ACCENT).bold()),
        Span::styled(format!(" {d}   "), Style::new().fg(MUTED)),
    ]
}

// ---------------------------------------------------------------- root

pub fn draw(f: &mut Frame, app: &mut App) {
    app.tick = app.tick.wrapping_add(1);
    let [top, _, body, player, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, app, top);

    let content = if body.width >= 100 {
        let [side, main] = Layout::horizontal([Constraint::Length(34), Constraint::Min(40)]).spacing(1).areas(body);
        draw_now_playing(f, app, side);
        main
    } else {
        body
    };
    match app.tab {
        Tab::Wave => draw_wave(f, app, content),
        Tab::Stations => draw_stations(f, app, content),
        Tab::Liked => draw_liked(f, app, content),
        Tab::Search => draw_search(f, app, content),
        Tab::Playlists => draw_playlists(f, app, content),
    }
    draw_player(f, app, player);
    draw_hints(f, app, hints);
    if app.show_help {
        draw_help(f);
    }
    if app.show_about {
        draw_about(f);
    }
    draw_modal(f, app);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let i = ic();
    let tabs =
        [(i.wave, "Волна"), (i.stations, "Станции"), (i.liked, "Мне нравится"), (i.search, "Поиск"), (i.queue, "Плейлисты")];
    let mut spans = vec![
        Span::styled(format!(" {} ", i.logo), Style::new().fg(ON_ACCENT).bg(ACCENT).bold()),
        Span::styled(" ymusic ", Style::new().fg(ACCENT).bold()),
        Span::raw("  "),
    ];
    let compact = area.width < 110;
    for (n, (icon, name)) in tabs.iter().enumerate() {
        if n == app.tab.index() {
            spans.push(Span::styled(format!(" {icon} {name} "), Style::new().fg(ON_ACCENT).bg(ACCENT).bold()));
        } else {
            spans.push(Span::styled(format!(" {}", n + 1), Style::new().fg(MUTED)));
            let label = if compact { format!(" {icon} ") } else { format!(" {icon} {name} ") };
            spans.push(Span::styled(label, Style::new().fg(SUB)));
        }
        spans.push(Span::raw(" "));
    }
    let who = if app.account.login.is_empty() { app.account.name.clone() } else { app.account.login.clone() };
    let mut right = vec![Span::styled(format!("{} {who}", i.user), Style::new().fg(SUB))];
    if app.account.has_plus {
        let plus = if compact { format!("  {} ", i.plus) } else { format!("  {} Плюс ", i.plus) };
        right.push(Span::styled(plus, Style::new().fg(ACCENT).bold()));
    }
    let right = Line::from(right);
    let [l, r] = Layout::horizontal([Constraint::Min(10), Constraint::Length(right.width() as u16 + 1)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), l);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), r);
}

// ---------------------------------------------------------------- now playing card

fn hsl(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

/// Generative "cover art": a diagonal gradient seeded by the album id, drawn with half blocks.
fn draw_cover(buf: &mut Buffer, area: Rect, seed: &str) {
    let h = seed.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |a, b| (a ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    let hue1 = (h % 360) as f64;
    let hue2 = hue1 + 40.0 + ((h >> 12) % 100) as f64;
    let c1 = hsl(hue1, 0.65, 0.55);
    let c2 = hsl(hue2, 0.70, 0.22);
    let (w, hh) = (f64::from(area.width.max(1)), f64::from(area.height.max(1) * 2));
    let color = |x: u16, py: u16| {
        let t = ((f64::from(x) / w) * 0.55 + (f64::from(py) / hh) * 0.45).clamp(0.0, 1.0);
        let mix = |a: f64, b: f64| ((a + (b - a) * t) * 255.0) as u8;
        Color::Rgb(mix(c1.0, c2.0), mix(c1.1, c2.1), mix(c1.2, c2.2))
    };
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                cell.set_symbol("▀").set_fg(color(x, y * 2)).set_bg(color(x, y * 2 + 1));
            }
        }
    }
    let (cx, cy) = (area.width / 2, area.height / 2);
    if let Some(cell) = buf.cell_mut((area.x + cx, area.y + cy)) {
        cell.set_symbol(ic().note).set_fg(Color::Rgb(255, 255, 255)).set_bg(color(cx, cy * 2));
    }
}

fn draw_now_playing(f: &mut Frame, app: &mut App, area: Rect) {
    let i = ic();
    let block = panel(i.note, "Сейчас играет");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.current().is_none() {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("Ничего не играет", Style::new().fg(SUB))),
            Line::from(""),
            Line::from(key_hint("w", "Моя волна").to_vec()),
            Line::from(key_hint("2", "станции").to_vec()),
        ];
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    }

    let info = if app.show_covers { draw_now_cover(f, app, inner) } else { inner };
    let Some(t) = app.current() else { return };

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(t.title.as_str(), Style::new().fg(Color::Reset).bold())),
        Line::from(Span::styled(t.artists.as_str(), Style::new().fg(SUB))),
    ];
    if !t.album.is_empty() {
        lines.push(Line::from(Span::styled(format!("{} {}", i.album, t.album), Style::new().fg(MUTED))));
    }
    lines.push(Line::from(""));
    let src = app.source_name();
    if !src.is_empty() {
        let icon = if app.source == Source::Wave { i.wave } else { i.queue };
        lines.push(Line::from(Span::styled(format!("{icon} {src}"), Style::new().fg(ACCENT))));
    }
    if app.is_liked(t) {
        lines.push(Line::from(Span::styled(format!("{} В «Мне нравится»", i.heart), Style::new().fg(HEART))));
    } else {
        lines.push(Line::from(vec![
            Span::styled("l", Style::new().fg(ACCENT).bold()),
            Span::styled(" — добавить в любимые", Style::new().fg(MUTED)),
        ]));
    }
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true }), info);
}

/// Draws the cover at the top of «Сейчас играет»; returns the area left for the track info.
fn draw_now_cover(f: &mut Frame, app: &mut App, inner: Rect) -> Rect {
    // Square cover: cell aspect ratio comes from the terminal font size.
    let (fw, fh) = app.picker.as_ref().map_or((1, 2), |p| p.font_size());
    let ratio = f64::from(fh.max(1)) / f64::from(fw.max(1));
    let mut cover_h = (inner.height * 45 / 100).min(inner.height.saturating_sub(9));
    let mut cover_w = (f64::from(cover_h) * ratio).round() as u16;
    if cover_w > inner.width {
        cover_w = inner.width;
        cover_h = (f64::from(cover_w) / ratio).round() as u16;
    }
    let [_, cover_area, info] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(cover_h), Constraint::Min(0)]).areas(inner);
    let [cover_area] = Layout::horizontal([Constraint::Length(cover_w)]).flex(Flex::Center).areas(cover_area);
    if cover_h >= 3 {
        if let Some(proto) = app.current_cover() {
            f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle))),
                cover_area,
                proto,
            );
        } else if let Some(t) = app.current() {
            // Placeholder while the cover is loading (or when there is none).
            draw_cover(f.buffer_mut(), cover_area, t.album_id.as_deref().unwrap_or(&t.id));
        }
    }
    info
}

// ---------------------------------------------------------------- track tables

/// Renders a track table building only the visible rows (lists can hold thousands of tracks).
#[allow(clippy::too_many_arguments)]
fn render_tracks(
    f: &mut Frame,
    area: Rect,
    app: &App,
    tracks: &[Track],
    state: &mut TableState,
    playing: Option<usize>,
    history: bool,
    block: Block,
) {
    // Inner height minus header and its margin.
    let visible = (block.inner(area).height.saturating_sub(2) as usize).max(1);
    let sel = state.selected().filter(|&s| s < tracks.len());
    let mut off = state.offset();
    if let Some(s) = sel {
        if s < off {
            off = s;
        } else if s >= off + visible {
            off = s + 1 - visible;
        }
    }
    off = off.min(tracks.len().saturating_sub(visible));
    *state.offset_mut() = off;
    let end = (off + visible).min(tracks.len());
    let table = track_table(app, &tracks[off..end], off, playing, history).block(block);
    let mut window = TableState::default().with_selected(sel.map(|s| s - off));
    f.render_stateful_widget(table, area, &mut window);
}

fn track_table<'a>(app: &App, tracks: &'a [Track], base: usize, playing: Option<usize>, history: bool) -> Table<'a> {
    let i = ic();
    let rows: Vec<Row> = tracks
        .iter()
        .enumerate()
        .map(|(n, t)| {
            let n = n + base;
            let is_playing = playing == Some(n);
            let num = if is_playing {
                let s = if app.loading_track {
                    spinner(app.tick)
                } else if app.paused {
                    i.pause
                } else {
                    equalizer(app.tick)
                };
                Span::styled(s, Style::new().fg(ACCENT).bold())
            } else {
                Span::styled(format!("{}", n + 1), Style::new().fg(MUTED))
            };
            let heart = if app.is_liked(t) {
                Span::styled(format!("{} ", i.heart), Style::new().fg(HEART))
            } else {
                Span::raw("  ")
            };
            let past = history && playing.is_some_and(|p| n < p);
            let (title_style, artist_style) = if !t.available {
                (Style::new().fg(MUTED).crossed_out(), Style::new().fg(MUTED))
            } else if is_playing {
                (Style::new().fg(ACCENT).bold(), Style::new().fg(ACCENT))
            } else if past {
                (Style::new().fg(SUB), Style::new().fg(MUTED))
            } else {
                (Style::new().fg(Color::Reset), Style::new().fg(SUB))
            };
            Row::new(vec![
                Cell::from(Line::from(num).alignment(Alignment::Right)),
                Cell::from(Line::from(vec![heart, Span::styled(t.title.as_str(), title_style)])),
                Cell::from(Span::styled(t.artists.as_str(), artist_style)),
                Cell::from(
                    Line::from(Span::styled(fmt_time(t.duration_ms as f64 / 1000.0), Style::new().fg(MUTED)))
                        .alignment(Alignment::Right),
                ),
            ])
        })
        .collect();
    let header = Row::new(vec![
        Cell::from(Line::from("#").alignment(Alignment::Right)),
        Cell::from("  Название"),
        Cell::from("Исполнитель"),
        Cell::from(Line::from(i.clock).alignment(Alignment::Right)),
    ])
    .style(Style::new().fg(MUTED))
    .bottom_margin(1);
    Table::new(rows, [Constraint::Length(4), Constraint::Fill(3), Constraint::Fill(2), Constraint::Length(6)])
        .header(header)
        .column_spacing(2)
        .row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol(Line::from(Span::styled("▌", Style::new().fg(ACCENT))))
        .highlight_spacing(HighlightSpacing::Always)
}

fn draw_wave(f: &mut Frame, app: &mut App, area: Rect) {
    let i = ic();
    if app.queue.is_empty() {
        let block = panel(i.wave, "Моя волна");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let rows: [(&str, &str); 6] = [
            ("w", "запустить Мою волну"),
            ("2", "станции: жанры, настроения, занятия"),
            ("3", "мои лайки"),
            ("5", "мои плейлисты"),
            ("/", "поиск"),
            ("?", "все горячие клавиши"),
        ];
        let mut lines = vec![
            Line::from(Span::styled(format!("{}  ymusic", i.logo), Style::new().fg(ACCENT).bold())),
            Line::from(Span::styled("Яндекс Музыка в терминале", Style::new().fg(SUB))),
            Line::from(""),
        ];
        for (k, d) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!(" {k} "), Style::new().fg(ON_ACCENT).bg(ACCENT).bold()),
                Span::styled(format!("  {d:<36}"), Style::new().fg(SUB)),
            ]));
            lines.push(Line::from(""));
        }
        let h = lines.len() as u16;
        let [v] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(inner);
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), v);
        return;
    }
    let title = match &app.source {
        Source::Wave => app.source_name(),
        Source::List(n) => n.clone(),
        Source::None => "Очередь".into(),
    };
    let icon = if app.source == Source::Wave { i.wave } else { i.queue };
    let fetching = app.wave.as_ref().is_some_and(|w| w.fetching);
    let title = if fetching { format!("{title} · {}", spinner(app.tick)) } else { title };
    let mut st = std::mem::take(&mut app.queue_state);
    render_tracks(f, area, app, &app.queue, &mut st, app.pos, true, panel(icon, title));
    app.queue_state = st;
}

fn draw_liked(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.liked_loading && app.liked.is_empty() {
        format!("Мне нравится · {}", spinner(app.tick))
    } else {
        format!("Мне нравится · {}", app.liked.len())
    };
    let playing = matches!(&app.source, Source::List(n) if n.starts_with("Мне нравится"))
        .then(|| app.current().map(|c| c.id.clone()))
        .flatten()
        .and_then(|id| app.liked.iter().position(|t| t.id == id));
    let mut st = std::mem::take(&mut app.liked_state);
    render_tracks(f, area, app, &app.liked, &mut st, playing, false, panel(ic().liked, title));
    app.liked_state = st;
}

fn draw_playlists(f: &mut Frame, app: &mut App, area: Rect) {
    let i = ic();
    if let Some(open) = &app.open_playlist {
        let title = if open.loading && open.tracks.is_empty() {
            format!("{} · {}", open.pl.title, spinner(app.tick))
        } else {
            format!("{} · {}   Esc — к списку", open.pl.title, open.tracks.len())
        };
        let playing = (matches!(app.source, Source::List(_)) && app.playing_playlist == Some(open.pl.key()))
            .then(|| app.current().map(|c| c.id.clone()))
            .flatten()
            .and_then(|id| open.tracks.iter().position(|t| t.id == id));
        let mut st = std::mem::take(&mut app.open_state);
        render_tracks(f, area, app, &open.tracks, &mut st, playing, false, panel(i.queue, title));
        app.open_state = st;
        return;
    }
    let title = if app.playlists_loading && app.playlists.is_empty() {
        format!("Плейлисты · {}", spinner(app.tick))
    } else {
        format!("Плейлисты · {}", app.playlists.len())
    };
    let block = panel(i.queue, title);
    if app.playlists.is_empty() && !app.playlists_loading {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [v] = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(inner);
        f.render_widget(Paragraph::new(Span::styled("Плейлистов нет", Style::new().fg(SUB))).alignment(Alignment::Center), v);
        return;
    }
    let playing = matches!(app.source, Source::List(_)).then_some(app.playing_playlist.as_deref()).flatten();
    let rows: Vec<Row> = app
        .playlists
        .iter()
        .enumerate()
        .map(|(n, p)| {
            let on = playing == Some(p.key().as_str());
            let num = if on {
                let s = if app.paused { i.pause } else { equalizer(app.tick) };
                Span::styled(s, Style::new().fg(ACCENT).bold())
            } else {
                Span::styled(format!("{}", n + 1), Style::new().fg(MUTED))
            };
            let title_style = if on { Style::new().fg(ACCENT).bold() } else { Style::new().fg(Color::Reset) };
            Row::new(vec![
                Cell::from(Line::from(num).alignment(Alignment::Right)),
                Cell::from(Span::styled(p.title.as_str(), title_style)),
                Cell::from(
                    Line::from(Span::styled(format!("{} {}", p.track_count, i.note), Style::new().fg(MUTED)))
                        .alignment(Alignment::Right),
                ),
            ])
        })
        .collect();
    let header = Row::new(vec![
        Cell::from(Line::from("#").alignment(Alignment::Right)),
        Cell::from("Название"),
        Cell::from(Line::from("Треков").alignment(Alignment::Right)),
    ])
    .style(Style::new().fg(MUTED))
    .bottom_margin(1);
    let table = Table::new(rows, [Constraint::Length(4), Constraint::Fill(1), Constraint::Length(8)])
        .header(header)
        .column_spacing(2)
        .block(block)
        .row_highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol(Line::from(Span::styled("▌", Style::new().fg(ACCENT))))
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, area, &mut app.playlists_state);
}

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let i = ic();
    let [input, results] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    let mut spans = vec![Span::styled(format!("{}  ", i.search), Style::new().fg(ACCENT))];
    if app.search_input.is_empty() && !app.search_editing {
        spans.push(Span::styled("Трек или исполнитель…  (/ или i — ввод)", Style::new().fg(MUTED)));
    } else {
        spans.push(Span::styled(app.search_input.clone(), Style::new().fg(Color::Reset).bold()));
    }
    if app.search_editing && (app.tick / 2).is_multiple_of(2) {
        spans.push(Span::styled("▎", Style::new().fg(ACCENT)));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(if app.search_editing { ACCENT } else { BORDER }))
                .padding(Padding::horizontal(1)),
        ),
        input,
    );
    let shown = app.search_results.len();
    let title = match (app.search_loading, shown) {
        (true, 0) => format!("Результаты · {}", spinner(app.tick)),
        (true, _) => format!("Результаты · {shown} из {} {}", app.search_total, spinner(app.tick)),
        (false, _) if app.search_total > shown => format!("Результаты · {shown} из {}", app.search_total),
        (false, _) => format!("Результаты · {shown}"),
    };
    let current = app.current().map(|c| c.id.clone());
    let playing = current.and_then(|id| app.search_results.iter().position(|t| t.id == id));
    let mut st = std::mem::take(&mut app.search_state);
    render_tracks(f, results, app, &app.search_results, &mut st, playing, false, panel(i.search, title));
    app.search_state = st;
}

fn draw_stations(f: &mut Frame, app: &mut App, area: Rect) {
    let i = ic();
    let active = (app.source == Source::Wave)
        .then(|| app.wave.as_ref().map(|w| (w.station.clone(), w.name.clone())))
        .flatten();
    let items: Vec<ListItem> = app
        .stations
        .iter()
        .map(|row| match row {
            StationRow::Header(h) => {
                let icon = match h.as_str() {
                    "Моя волна" => i.wave,
                    "Жанры" => i.genre,
                    "Настроение" => i.mood,
                    "Занятия" => i.activity,
                    "Эпохи" => i.epoch,
                    _ => i.other,
                };
                ListItem::new(vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(format!("{icon}  "), Style::new().fg(ACCENT)),
                        Span::styled(h.to_uppercase(), Style::new().fg(SUB).bold()),
                    ]),
                ])
            }
            StationRow::Station(s) => {
                let on = active.as_ref().is_some_and(|(id, name)| *id == s.id && *name == s.name);
                let line = if on {
                    let mark = if app.paused { i.pause } else { equalizer(app.tick) };
                    Line::from(vec![
                        Span::styled(format!("   {mark:<3} "), Style::new().fg(ACCENT)),
                        Span::styled(s.name.as_str(), Style::new().fg(ACCENT).bold()),
                    ])
                } else {
                    Line::from(vec![Span::styled("   ·   ", Style::new().fg(MUTED)), Span::raw(s.name.as_str())])
                };
                ListItem::new(line)
            }
        })
        .collect();
    let list = List::new(items)
        .block(panel(i.stations, "Станции"))
        .highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, area, &mut app.stations_state);
}

// ---------------------------------------------------------------- player bar

fn draw_player(f: &mut Frame, app: &App, area: Rect) {
    let i = ic();
    let block = Block::new().borders(Borders::TOP).border_style(Style::new().fg(BORDER)).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [l1, l2] = Layout::vertical([Constraint::Length(1); 2]).areas(inner);

    // volume
    let vol_icon = if app.muted || app.volume <= 0.0 {
        i.vol_off
    } else if app.volume < 35.0 {
        i.vol_lo
    } else if app.volume < 70.0 {
        i.vol_mid
    } else {
        i.vol_hi
    };
    let filled = ((app.volume.min(100.0) / 10.0).round() as usize).min(10);
    let vol_color = if app.muted { MUTED } else { ACCENT };
    let mut right = vec![];
    let preset = &crate::eq::PRESETS[app.eq];
    if !preset.is_flat() {
        right.push(Span::styled("EQ ", Style::new().fg(ACCENT).bold()));
        right.push(Span::styled(format!("{}   ", preset.name), Style::new().fg(SUB)));
    }
    right.extend([
        Span::styled(format!("{vol_icon} "), Style::new().fg(vol_color)),
        Span::styled("━".repeat(filled), Style::new().fg(vol_color)),
        Span::styled("━".repeat(10 - filled), Style::new().fg(BORDER)),
        Span::styled(format!(" {:>3.0}%", app.volume), Style::new().fg(SUB)),
    ]);
    let right = Line::from(right);

    let play_icon = if app.paused || app.current().is_none() { i.play } else { i.pause };
    let mut left = vec![
        Span::styled(format!("{} ", i.prev), Style::new().fg(SUB)),
        Span::styled(format!(" {play_icon} "), Style::new().fg(ON_ACCENT).bg(ACCENT).bold()),
        Span::styled(format!(" {}", i.next), Style::new().fg(SUB)),
        Span::raw("   "),
    ];
    match app.current() {
        Some(t) => {
            left.push(Span::styled(t.title.as_str(), Style::new().fg(Color::Reset).bold()));
            left.push(Span::styled(format!("  {}", t.artists), Style::new().fg(SUB)));
            if app.is_liked(t) {
                left.push(Span::styled(format!("  {}", i.heart), Style::new().fg(HEART)));
            }
        }
        None => left.push(Span::styled("Ничего не играет — нажмите w", Style::new().fg(MUTED))),
    }
    let [a, b] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(right.width() as u16)]).spacing(2).areas(l1);
    f.render_widget(Paragraph::new(Line::from(left)), a);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), b);

    // progress
    let pos = fmt_time(app.time_pos);
    let dur = fmt_time(app.duration);
    let bar_w = l2.width.saturating_sub((pos.len() + dur.len() + 2) as u16) as usize;
    let ratio = if app.duration > 0.0 { (app.time_pos / app.duration).clamp(0.0, 1.0) } else { 0.0 };
    let done = ((bar_w as f64) * ratio) as usize;
    let mut spans = vec![Span::styled(format!("{pos} "), Style::new().fg(SUB))];
    if bar_w > 0 {
        let color = if app.paused { SUB } else { ACCENT };
        spans.push(Span::styled("━".repeat(done), Style::new().fg(color)));
        if done < bar_w {
            spans.push(Span::styled("●", Style::new().fg(color)));
            spans.push(Span::styled("━".repeat(bar_w - done - 1), Style::new().fg(BORDER)));
        }
    }
    spans.push(Span::styled(format!(" {dur}"), Style::new().fg(SUB)));
    f.render_widget(Paragraph::new(Line::from(spans)), l2);
}

fn draw_hints(f: &mut Frame, app: &App, area: Rect) {
    let i = ic();
    if let Some(s) = app.visible_status() {
        let (icon, color) = if app.status_error { (i.error, ERROR) } else { (i.info, ACCENT) };
        f.render_widget(Paragraph::new(Span::styled(format!(" {icon} {s}"), Style::new().fg(color))), area);
        return;
    }
    let keys: &[(&str, &str)] = if app.search_editing {
        &[("Enter", "искать"), ("Esc", "отмена"), ("Ctrl+U", "очистить")]
    } else if let Some(modal) = &app.modal {
        match modal {
            Modal::AddTo { .. } => &[("↑↓", "выбор"), ("Enter", "добавить"), ("Esc", "отмена")],
            Modal::ConfirmRemove { .. } | Modal::ConfirmDelete { .. } => &[("Enter", "удалить"), ("Esc", "отмена")],
            Modal::Rename { .. } => &[("Enter", "сохранить"), ("Esc", "отмена"), ("Ctrl+U", "очистить")],
            Modal::OpenLink { .. } => &[("Enter", "играть"), ("Esc", "отмена"), ("Ctrl+U", "очистить")],
            Modal::Settings { .. } => &[("↑↓", "выбор"), ("←→", "изменить"), ("Enter", "открыть"), ("Esc", "закрыть")],
            Modal::ConfirmLogout => &[("Enter", "выйти"), ("Esc", "отмена")],
        }
    } else if app.tab == Tab::Playlists && app.open_playlist.is_some() {
        &[
            ("Enter", "играть"),
            ("s", "перемешать"),
            ("x", "удалить"),
            ("a", "в плейлист"),
            ("E", "переименовать"),
            ("Esc", "назад"),
        ]
    } else {
        match app.tab {
            Tab::Stations => &[
                ("Enter", "запустить"),
                ("␣", "пауза"),
                ("n", "далее"),
                ("l", "лайк"),
                ("d", "дизлайк"),
                ("?", "помощь"),
                ("q", "выход"),
            ],
            Tab::Playlists => &[
                ("Enter", "открыть"),
                ("s", "перемешать"),
                ("E", "переименовать"),
                ("x", "удалить"),
                ("R", "обновить"),
                ("?", "помощь"),
            ],
            Tab::Liked | Tab::Search => &[
                ("Enter", "играть"),
                ("s", "перемешать"),
                ("L", "лайк"),
                ("r", "волна по треку"),
                ("␣", "пауза"),
                ("n", "далее"),
                ("?", "помощь"),
            ],
            Tab::Wave => &[
                ("w", "волна"),
                ("␣", "пауза"),
                ("n/b", "трек"),
                ("l", "лайк"),
                ("d", "дизлайк"),
                ("←→", "перемотка"),
                ("+-", "громкость"),
                ("?", "помощь"),
                ("q", "выход"),
            ],
        }
    };
    // «Настройки» sit at the right edge, except while text is typed (`o` is input there).
    let about = (!app.search_editing && app.modal.is_none()).then(|| {
        Line::from(vec![
            Span::styled("o", Style::new().fg(ACCENT).bold()),
            Span::styled(" настройки ", Style::new().fg(MUTED)),
        ])
    });
    let right_w = about.as_ref().map_or(0, Line::width);
    // Drop hints from the end (keeping «? помощь») until they fit next to it.
    let hint_w = |(k, d): &(&str, &str)| Line::from(key_hint(k, d).to_vec()).width();
    let mut shown: Vec<(&str, &str)> = keys.to_vec();
    while 1 + shown.iter().map(hint_w).sum::<usize>() + right_w > area.width as usize {
        let Some(last) = shown.iter().rposition(|(k, _)| *k != "?") else { break };
        shown.remove(last);
    }
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in shown {
        spans.extend(key_hint(k, d));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    if let Some(about) = about {
        f.render_widget(Paragraph::new(about).alignment(Alignment::Right), area);
    }
}

type HelpSection = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

fn draw_help(f: &mut Frame) {
    let i = ic();
    let sections: [HelpSection; 5] = [
        (
            i.play,
            "Воспроизведение",
            &[
                ("Space  p", "пауза / продолжить"),
                ("n  b", "следующий / предыдущий"),
                ("←  →", "перемотка ±10 с"),
                ("+  -", "громкость"),
                ("m", "без звука"),
            ],
        ),
        (i.heart, "Оценки", &[("l", "лайк текущего трека"), ("L", "лайк выбранного в списке"), ("d", "дизлайк и пропуск")]),
        (
            i.wave,
            "Волны",
            &[
                ("w", "Моя волна"),
                ("2  Enter", "станция: жанр, настроение…"),
                ("r", "волна по треку"),
                ("A", "все треки исполнителя"),
                ("e", "альбом трека"),
            ],
        ),
        (
            i.keys,
            "Навигация",
            &[
                ("1-5  Tab", "вкладки"),
                ("j k  ↑ ↓", "перемещение"),
                ("g  G", "в начало / в конец"),
                ("Enter", "играть"),
                ("s", "перемешать список"),
                ("y", "копировать ссылку на трек"),
                ("Y", "играть по ссылке"),
                ("/", "поиск"),
                ("R", "обновить"),
                ("o", "настройки, о программе"),
                ("q", "выход"),
            ],
        ),
        (
            i.queue,
            "Плейлисты",
            &[
                ("a", "добавить трек в плейлист"),
                ("Enter  Esc", "открыть / закрыть плейлист"),
                ("x", "удалить трек / плейлист"),
                ("E", "переименовать плейлист"),
            ],
        ),
    ];
    let render = |sections: &[HelpSection]| {
        let mut lines = vec![];
        for (icon, title, rows) in sections {
            lines.push(Line::from(vec![
                Span::styled(format!("{icon}  "), Style::new().fg(ACCENT)),
                Span::styled(title.to_uppercase(), Style::new().fg(SUB).bold()),
            ]));
            for (k, d) in *rows {
                lines.push(Line::from(vec![
                    Span::styled(format!("   {k:<12}"), Style::new().fg(ACCENT).bold()),
                    Span::styled(*d, Style::new().fg(Color::Reset)),
                ]));
            }
            lines.push(Line::from(""));
        }
        lines.pop();
        lines
    };
    const COL: u16 = 46;
    let area = f.area();
    // Two columns when they fit, otherwise one (cut at the bottom on very small terminals).
    let columns = if area.width >= 2 * COL + 6 {
        vec![render(&sections[..3]), render(&sections[3..])]
    } else {
        vec![render(&sections)]
    };
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let width = (columns.len() as u16 * COL + 4).min(area.width);
    let [v] = Layout::vertical([Constraint::Length((rows + 4).min(area.height))]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let block = panel(i.keys, "Горячие клавиши")
        .border_style(Style::new().fg(ACCENT))
        .title_bottom(Line::from(Span::styled(" любая клавиша — закрыть ", Style::new().fg(MUTED))).centered())
        .padding(Padding::uniform(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let cols = Layout::horizontal(vec![Constraint::Fill(1); columns.len()]).split(inner);
    for (lines, col) in columns.into_iter().zip(cols.iter()) {
        f.render_widget(Paragraph::new(lines), *col);
    }
}

pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");
pub const UPSTREAM_URL: &str = "https://github.com/Rexilone/yamusic-cli";
pub const AUR_URL: &str = "https://aur.archlinux.org/packages/ymusic-cli";
pub const AUR_BIN_URL: &str = "https://aur.archlinux.org/packages/ymusic-cli-bin";

/// Renders `text` as a clickable link (OSC 8) in one line starting at `x`, `y`.
fn render_link(buf: &mut Buffer, x: u16, y: u16, text: &str, url: &str, style: Style) {
    buf.set_string(x, y, text, style);
    // The escape sequence is counted as two cells wide by ratatui, so the link is written in
    // two-character chunks (the same workaround as in ratatui's hyperlink example).
    let chars: Vec<char> = text.chars().collect();
    for (n, pair) in chars.chunks(2).enumerate() {
        let s: String = pair.iter().collect();
        if let Some(cell) = buf.cell_mut((x + n as u16 * 2, y)) {
            cell.set_symbol(&format!("\x1b]8;;{url}\x07{s}\x1b]8;;\x07"));
        }
    }
}

fn draw_about(f: &mut Frame) {
    let i = ic();
    let link = Style::new().fg(ACCENT).underlined();
    // Terminals can't use a smaller font for one line, so the fork note is dimmed instead.
    let faint = Style::new().fg(MUTED).italic();
    // One entry per row: text segments (text, style, link target); an empty entry is a blank row.
    let rows: Vec<Vec<(String, Style, Option<&str>)>> = vec![
        vec![(format!("{}  ymusic", i.logo), Style::new().fg(ACCENT).bold(), None)],
        vec![],
        vec![(format!("Версия {}", env!("CARGO_PKG_VERSION")), Style::new().fg(SUB), None)],
        vec![],
        vec![],
        vec![(REPO_URL.into(), link, Some(REPO_URL))],
        vec![(AUR_URL.into(), link, Some(AUR_URL))],
        vec![(AUR_BIN_URL.into(), link, Some(AUR_BIN_URL))],
        vec![],
        vec![],
        vec![("Forked from: ".into(), faint, None), (UPSTREAM_URL.into(), faint.underlined(), Some(UPSTREAM_URL))],
    ];
    let row_width = |row: &[(String, Style, Option<&str>)]| row.iter().map(|(t, ..)| Span::raw(t.as_str()).width()).sum::<usize>();
    let widest = rows.iter().map(|r| row_width(r)).max().unwrap_or(0) as u16;
    let area = f.area();
    let [v] = Layout::vertical([Constraint::Length((rows.len() as u16 + 4).min(area.height))]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length((widest + 10).min(area.width))]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let block = panel(i.info, "О программе")
        .border_style(Style::new().fg(ACCENT))
        .title_bottom(Line::from(Span::styled(" любая клавиша — закрыть ", Style::new().fg(MUTED))).centered())
        .padding(Padding::uniform(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let buf = f.buffer_mut();
    for (n, row) in rows.iter().enumerate().take(inner.height as usize) {
        let y = inner.y + n as u16;
        let w = row_width(row) as u16;
        if w > inner.width {
            // Too narrow for links: plain, truncated text.
            let text: String = row.iter().map(|(t, ..)| t.as_str()).collect();
            buf.set_stringn(inner.x, y, &text, inner.width as usize, Style::new().fg(SUB));
            continue;
        }
        let mut x = inner.x + (inner.width - w) / 2;
        for (text, style, url) in row {
            match url {
                Some(url) => render_link(buf, x, y, text, url, *style),
                None => {
                    buf.set_string(x, y, text, *style);
                }
            }
            x += Span::raw(text.as_str()).width() as u16;
        }
    }
}

fn setting_name(s: Setting) -> &'static str {
    match s {
        Setting::Quality => "Качество звука",
        Setting::Equalizer => "Эквалайзер",
        Setting::Crossfade => "Плавные переходы",
        Setting::LessAi => "Меньше ИИ-музыки",
        Setting::Covers => "Обложка трека",
        Setting::Icons => "Иконки Nerd Font",
        Setting::About => "О программе",
        Setting::Logout => "Выйти из аккаунта",
    }
}

/// Explanation shown under the settings list for the selected row.
fn setting_note(app: &App, s: Setting) -> String {
    match s {
        Setting::Quality => format!("{} · со следующего трека", crate::api::QUALITIES[app.quality].note),
        Setting::Equalizer => "Слышно сразу".into(),
        Setting::Crossfade => "Затухание в конце трека и нарастание в начале".into(),
        Setting::LessAi => "Реже ИИ-треки в рекомендациях · настройка аккаунта".into(),
        Setting::Covers => "Обложка альбома в панели «Сейчас играет»".into(),
        Setting::Icons => "Без шрифта Nerd Font — простые символы".into(),
        Setting::About => "Версия и ссылки на проект".into(),
        Setting::Logout => "Токен будет удалён, программа закроется".into(),
    }
}

/// Centered pop-up box of the given inner size; returns the inner area.
fn popup(f: &mut Frame, icon: &str, title: &str, w: u16, h: u16) -> Rect {
    popup_block(f, panel(icon, title.to_string()), w, h)
}

/// Like [`popup`], for a box with extra decorations (e.g. a bottom title).
fn popup_block(f: &mut Frame, block: Block, w: u16, h: u16) -> Rect {
    let area = f.area();
    let [v] = Layout::vertical([Constraint::Length((h + 4).min(area.height))]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length((w + 4).min(area.width))]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let block = block.border_style(Style::new().fg(ACCENT)).padding(Padding::uniform(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

fn draw_modal(f: &mut Frame, app: &App) {
    let i = ic();
    let Some(modal) = &app.modal else { return };
    match modal {
        Modal::AddTo { track, choices, sel } => {
            let visible = choices.len().min(12);
            let inner = popup(f, i.queue, "Добавить в плейлист", 48, visible as u16 + 2);
            let [head, _, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
            f.render_widget(
                Paragraph::new(Span::styled(format!("{} — {}", track.title, track.artists), Style::new().fg(SUB))),
                head,
            );
            let items: Vec<ListItem> = choices
                .iter()
                .map(|p| {
                    ListItem::new(Line::from(vec![
                        Span::raw(p.title.clone()),
                        Span::styled(format!("  {} {}", p.track_count, i.note), Style::new().fg(MUTED)),
                    ]))
                })
                .collect();
            let mut st = ratatui::widgets::ListState::default().with_selected(Some(*sel));
            let widget = List::new(items)
                .highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
                .highlight_symbol("▌ ")
                .highlight_spacing(HighlightSpacing::Always);
            f.render_stateful_widget(widget, list, &mut st);
        }
        Modal::ConfirmRemove { pl, track, .. } => {
            let inner = popup(f, i.error, "Удалить трек", 48, 4);
            let lines = vec![
                Line::from(Span::styled(track.title.clone(), Style::new().fg(Color::Reset).bold())),
                Line::from(Span::styled(format!("из плейлиста «{}»?", pl.title), Style::new().fg(SUB))),
                Line::from(""),
                Line::from([key_hint("Enter", "удалить"), key_hint("Esc", "отмена")].concat()),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
        Modal::ConfirmDelete { pl } => {
            let inner = popup(f, i.error, "Удалить плейлист", 48, 4);
            let lines = vec![
                Line::from(Span::styled(
                    format!("«{}» · {} {}", pl.title, pl.track_count, i.note),
                    Style::new().fg(Color::Reset).bold(),
                )),
                Line::from(Span::styled("Плейлист будет удалён из аккаунта.", Style::new().fg(SUB))),
                Line::from(""),
                Line::from([key_hint("Enter", "удалить"), key_hint("Esc", "отмена")].concat()),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
        Modal::Settings { sel } => {
            let rows = Setting::ALL;
            let n = rows.len() as u16;
            let block = panel(i.settings, "Настройки")
                .title_bottom(Line::from(Span::styled(" Esc — закрыть окно ", Style::new().fg(MUTED))).centered());
            let inner = popup_block(f, block, 54, n + 2);
            let [list, _, note] =
                Layout::vertical([Constraint::Length(n), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
            let items: Vec<ListItem> = rows
                .iter()
                .map(|&s| {
                    let name = setting_name(s);
                    let label = if s == Setting::Logout { Style::new().fg(ERROR) } else { Style::new() };
                    let mut spans = vec![Span::styled(format!("{name:<20}"), label)];
                    if let Some(v) = app.setting_value(s) {
                        spans.push(Span::styled("‹ ", Style::new().fg(MUTED)));
                        spans.push(Span::styled(v, Style::new().fg(ACCENT).bold()));
                        spans.push(Span::styled(" ›", Style::new().fg(MUTED)));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let mut st = ratatui::widgets::ListState::default().with_selected(Some(*sel));
            let widget = List::new(items)
                .highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
                .highlight_symbol("▌ ")
                .highlight_spacing(HighlightSpacing::Always);
            f.render_stateful_widget(widget, list, &mut st);
            let text = setting_note(app, rows[*sel]);
            f.render_widget(Paragraph::new(Span::styled(text, Style::new().fg(MUTED))), note);
        }
        Modal::ConfirmLogout => {
            let inner = popup(f, i.error, "Выход из аккаунта", 48, 4);
            let lines = vec![
                Line::from(Span::styled(
                    format!("Выйти из аккаунта {}?", app.account.login),
                    Style::new().fg(Color::Reset).bold(),
                )),
                Line::from(Span::styled("Токен будет удалён, программа закроется.", Style::new().fg(SUB))),
                Line::from(""),
                Line::from([key_hint("Enter", "выйти"), key_hint("Esc", "отмена")].concat()),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
        Modal::Rename { input, .. } => {
            let inner = popup(f, i.queue, "Название плейлиста", 48, 1);
            let mut spans = vec![Span::styled(input.clone(), Style::new().fg(Color::Reset).bold())];
            if (app.tick / 2).is_multiple_of(2) {
                spans.push(Span::styled("▎", Style::new().fg(ACCENT)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), inner);
        }
        Modal::OpenLink { input } => {
            let inner = popup(f, i.link, "Играть по ссылке", 64, 1);
            // Long links scroll: the end with the cursor stays visible.
            let room = (inner.width as usize).saturating_sub(1);
            let skip = input.chars().count().saturating_sub(room);
            let text: String = input.chars().skip(skip).collect();
            let mut spans = vec![Span::styled(text, Style::new().fg(Color::Reset).bold())];
            if input.is_empty() {
                spans.push(Span::styled("Ссылка на трек, альбом или плейлист", Style::new().fg(MUTED)));
            } else if (app.tick / 2).is_multiple_of(2) {
                spans.push(Span::styled("▎", Style::new().fg(ACCENT)));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::Track;
    use crate::app::{App, Tab};
    use ratatui::{backend::TestBackend, Terminal};

    /// Renders every tab.
    #[test]
    fn render_smoke() {
        let (mut app, _audio) = App::for_test();
        let t = Track {
            id: "1".into(),
            album_id: Some("2".into()),
            title: "Выхода нет".into(),
            artists: "Сплин".into(),
            main_artist: None,
            album: "Гранатовый альбом".into(),
            duration_ms: 220_000,
            available: true,
            cover: None,
            batch: None,
        };
        app.queue = vec![
            Track { id: "0".into(), title: "Группа крови".into(), artists: "Кино".into(), ..t.clone() },
            t.clone(),
            Track { id: "3".into(), title: "Искала".into(), artists: "Земфира".into(), ..t.clone() },
        ];
        app.pos = Some(1);
        app.liked = vec![t.clone()];
        app.liked_ids.insert("1".into());
        app.liked_loading = false;
        app.playlists = ["Bloodborne soundtracks", "В дорогу"]
            .iter()
            .enumerate()
            .map(|(n, t)| crate::api::Playlist { owner: "1".into(), kind: n.to_string(), title: (*t).into(), track_count: 61 })
            .collect();
        app.playlists_loading = false;
        app.playlists_state.select(Some(0));
        app.time_pos = 61.0;
        app.duration = 220.0;
        app.status.clear();
        for (w, h) in [(120, 30), (80, 24)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            for tab in Tab::ALL {
                app.tab = tab;
                term.draw(|f| super::draw(f, &mut app)).unwrap();
                let buf = term.backend().buffer().clone();
                let text: String = (0..buf.area.height)
                    .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
                    .collect();
                println!("{text}");
                assert!(text.contains("Выхода нет"));
            }
        }
        // Opened playlist and the pop-up dialogs.
        use crate::app::{Modal, OpenPlaylist};
        let pl = app.playlists[0].clone();
        app.tab = Tab::Playlists;
        app.open_playlist = Some(OpenPlaylist { pl: pl.clone(), tracks: app.queue.clone(), loading: false, req: 0 });
        app.open_state.select(Some(2));
        let modals = [
            Some(Modal::ConfirmDelete { pl: pl.clone() }),
            None,
            Some(Modal::AddTo { track: t.clone(), choices: app.playlists.clone(), sel: 1 }),
            Some(Modal::ConfirmRemove { pl: pl.clone(), index: 2, track: app.queue[2].clone() }),
            Some(Modal::Rename { pl: pl.clone(), input: "Новое название".into() }),
            Some(Modal::Settings { sel: 2 }),
            Some(Modal::ConfirmLogout),
        ];
        let expect = [
            "Плейлист будет удалён из аккаунта.",
            "Искала",
            "Добавить в плейлист",
            "из плейлиста «Bloodborne soundtracks»?",
            "Новое название",
            "Затухание в конце трека и нарастание в начале",
            "Выйти из аккаунта tester?",
        ];
        for (modal, want) in modals.into_iter().zip(expect) {
            app.modal = modal;
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            term.draw(|f| super::draw(f, &mut app)).unwrap();
            let buf = term.backend().buffer().clone();
            let text: String = (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
                .collect();
            println!("{text}");
            assert!(text.contains(want), "{want}");
        }
        app.modal = None;
        app.open_playlist = None;

        // The player bar names a non-flat equalizer preset.
        app.set_eq(crate::eq::index("rock"));
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| super::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let bar: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .filter(|l| l.contains("%"))
            .collect();
        println!("{bar}");
        assert!(bar.contains("EQ Рок"));
        app.set_eq(0);

        app.show_about = true;
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| super::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .collect();
        // What a terminal shows: replay the cells ratatui actually sends (diff against an empty
        // screen), with OSC 8 sequences stripped.
        let strip = |sym: &str| {
            let mut out = String::new();
            let mut rest = sym;
            while let Some(i) = rest.find("\x1b]8;;") {
                out.push_str(&rest[..i]);
                rest = &rest[i..];
                rest = rest.find('\x07').map_or("", |j| &rest[j + 1..]);
            }
            out + rest
        };
        let mut screen = vec![vec![' '; buf.area.width as usize]; buf.area.height as usize];
        for (x, y, cell) in ratatui::buffer::Buffer::empty(buf.area).diff(&buf) {
            for (k, ch) in strip(cell.symbol()).chars().enumerate() {
                if let Some(c) = screen[y as usize].get_mut(x as usize + k) {
                    *c = ch;
                }
            }
        }
        let plain: String = screen.iter().map(|l| l.iter().collect::<String>() + "\n").collect();
        println!("{plain}");
        for want in [
            "О программе",
            env!("CARGO_PKG_VERSION"),
            "https://github.com/iodnoorov/ymusic",
            "https://aur.archlinux.org/packages/ymusic-cli ",
            "https://aur.archlinux.org/packages/ymusic-cli-bin",
            "Forked from: https://github.com/Rexilone/yamusic-cli",
        ] {
            assert!(plain.contains(want), "{want}");
        }
        assert!(text.contains(&format!("\x1b]8;;{}\x07", super::UPSTREAM_URL)));
        assert!(text.contains(&format!("\x1b]8;;{}\x07", super::AUR_BIN_URL)));
        app.show_about = false;

        app.show_help = true;
        let mut term = Terminal::new(TestBackend::new(100, 34)).unwrap();
        term.draw(|f| super::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
    }

    /// Kitty protocol: the cover is sent to the terminal again after switching back to
    /// an earlier track and after a resize, not only placeholders for an old image id.
    #[test]
    fn cover_retransmit() {
        use ratatui_image::picker::{Picker, ProtocolType};
        let (mut app, _audio) = App::for_test();
        let mut picker = Picker::from_fontsize((10, 20));
        picker.set_protocol_type(ProtocolType::Kitty);
        app.picker = Some(picker);
        let track = |id: &str| Track {
            id: id.into(),
            album_id: None,
            title: id.into(),
            artists: "A".into(),
            main_artist: None,
            album: String::new(),
            duration_ms: 1000,
            available: true,
            cover: Some(format!("cover-{id}")),
            batch: None,
        };
        app.queue = vec![track("a"), track("b")];
        app.covers = ["a", "b"]
            .iter()
            .map(|id| (format!("cover-{id}"), image::DynamicImage::new_rgb8(40, 40)))
            .collect();
        let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
        let mut transmits = |app: &mut App| {
            term.draw(|f| super::draw(f, app)).unwrap();
            term.backend().buffer().content().iter().any(|c| c.symbol().contains("_G"))
        };
        app.pos = Some(0);
        assert!(transmits(&mut app), "first show");
        assert!(!transmits(&mut app), "same cover is not sent twice");
        app.pos = Some(1);
        assert!(transmits(&mut app), "next track");
        app.pos = Some(0);
        assert!(transmits(&mut app), "back to the previous track");
        app.on_resize();
        assert!(transmits(&mut app), "after resize");
    }

    /// Downloads a real image and renders it as sixel and half blocks. Needs network.
    #[test]
    #[ignore]
    fn cover_render() {
        use ratatui_image::picker::{Picker, ProtocolType};
        let (mut app, _audio) = App::for_test();
        let url = crate::api::cover_url("avatars.yandex.net/get-music-misc/34161/rotor-genre-pop-icon/%%");
        let img = app.api.fetch_image(&url).unwrap();
        for proto in [ProtocolType::Sixel, ProtocolType::Halfblocks, ProtocolType::Kitty] {
            let mut picker = Picker::from_fontsize((10, 20));
            picker.set_protocol_type(proto);
            app.covers = vec![(url.clone(), img.clone())];
            app.picker = Some(picker);
            app.on_resize();
            app.queue = vec![Track {
                id: "1".into(),
                album_id: None,
                title: "Cover".into(),
                artists: "A".into(),
                main_artist: None,
                album: String::new(),
                duration_ms: 1000,
                available: true,
                cover: Some(url.clone()),
                batch: None,
            }];
            app.pos = Some(0);
            let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
            term.draw(|f| super::draw(f, &mut app)).unwrap();
            let res = app.current_cover().unwrap().last_encoding_result();
            assert!(matches!(res, Some(Ok(()))), "{proto:?}: {res:?}");
            let buf = term.backend().buffer().clone();
            let pos = buf.content().iter().position(|c| c.symbol().len() > 8 || c.symbol() == "▀" || c.symbol() == "▄");
            let at = pos.map(|p| (p as u16 % buf.area.width, p as u16 / buf.area.width));
            println!("{proto:?}: image starts at {at:?}");
            assert!(at.is_some_and(|(x, _)| x < 34), "image not in the card");
        }
    }
}
