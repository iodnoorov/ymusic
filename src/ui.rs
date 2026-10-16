//! Rendering.

use crate::api::Track;
use crate::app::{App, Source, StationRow, Tab};
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
use std::sync::OnceLock;

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
    error: &'static str,
    keys: &'static str,
    genre: &'static str,
    mood: &'static str,
    activity: &'static str,
    epoch: &'static str,
    other: &'static str,
    clock: &'static str,
    queue: &'static str,
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
    error: "󰗖",
    keys: "󰌌",
    genre: "󰋄",
    mood: "󰇵",
    activity: "󰜎",
    epoch: "󰃰",
    other: "󰐹",
    clock: "󰅐",
    queue: "󰲸",
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
    error: "!",
    keys: "⌨",
    genre: "♪",
    mood: "☺",
    activity: "⚑",
    epoch: "◷",
    other: "◉",
    clock: "◷",
    queue: "≡",
};

static ICONS: OnceLock<&'static Icons> = OnceLock::new();

pub fn set_nerd_icons(nerd: bool) {
    let _ = ICONS.set(if nerd { &NERD } else { &PLAIN });
}

fn ic() -> &'static Icons {
    ICONS.get().copied().unwrap_or(&NERD)
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
    }
    draw_player(f, app, player);
    draw_hints(f, app, hints);
    if app.show_help {
        draw_help(f);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let i = ic();
    let tabs = [(i.wave, "Волна"), (i.stations, "Станции"), (i.liked, "Мне нравится"), (i.search, "Поиск")];
    let mut spans = vec![
        Span::styled(format!(" {} ", i.logo), Style::new().fg(ON_ACCENT).bg(ACCENT).bold()),
        Span::styled(" yamusic ", Style::new().fg(ACCENT).bold()),
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
    let Some(t) = app.current() else {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("Ничего не играет", Style::new().fg(SUB))),
            Line::from(""),
            Line::from(key_hint("w", "Моя волна").to_vec()),
            Line::from(key_hint("2", "станции").to_vec()),
        ];
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    };

    let t = t.clone();
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
        match app.current_cover() {
            Some(proto) => f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle))),
                cover_area,
                proto,
            ),
            // Placeholder while the cover is loading (or when there is none).
            None => draw_cover(f.buffer_mut(), cover_area, t.album_id.as_deref().unwrap_or(&t.id)),
        }
    }
    let t = &t;

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(t.title.clone(), Style::new().fg(Color::Reset).bold())),
        Line::from(Span::styled(t.artists.clone(), Style::new().fg(SUB))),
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
                Cell::from(Line::from(vec![heart, Span::styled(t.title.clone(), title_style)])),
                Cell::from(Span::styled(t.artists.clone(), artist_style)),
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
        let rows: [(&str, &str); 5] = [
            ("w", "запустить Мою волну"),
            ("2", "станции: жанры, настроения, занятия"),
            ("3", "мои лайки"),
            ("/", "поиск"),
            ("?", "все горячие клавиши"),
        ];
        let mut lines = vec![
            Line::from(Span::styled(format!("{}  yamusic", i.logo), Style::new().fg(ACCENT).bold())),
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
    let title = if app.search_loading {
        format!("Результаты · {}", spinner(app.tick))
    } else {
        format!("Результаты · {}", app.search_results.len())
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
                        Span::styled(s.name.clone(), Style::new().fg(ACCENT).bold()),
                    ])
                } else {
                    Line::from(vec![Span::styled("   ·   ", Style::new().fg(MUTED)), Span::raw(s.name.clone())])
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
    let right = Line::from(vec![
        Span::styled(format!("{vol_icon} "), Style::new().fg(vol_color)),
        Span::styled("━".repeat(filled), Style::new().fg(vol_color)),
        Span::styled("━".repeat(10 - filled), Style::new().fg(BORDER)),
        Span::styled(format!(" {:>3.0}%", app.volume), Style::new().fg(SUB)),
    ]);

    let play_icon = if app.paused || app.current().is_none() { i.play } else { i.pause };
    let mut left = vec![
        Span::styled(format!("{} ", i.prev), Style::new().fg(SUB)),
        Span::styled(format!(" {play_icon} "), Style::new().fg(ON_ACCENT).bg(ACCENT).bold()),
        Span::styled(format!(" {}", i.next), Style::new().fg(SUB)),
        Span::raw("   "),
    ];
    match app.current() {
        Some(t) => {
            left.push(Span::styled(t.title.clone(), Style::new().fg(Color::Reset).bold()));
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
        let err = s.contains("Ошибка") || s.contains("Не удалось") || s.contains("не удалось");
        let (icon, color) = if err { (i.error, ERROR) } else { (i.info, ACCENT) };
        f.render_widget(Paragraph::new(Span::styled(format!(" {icon} {s}"), Style::new().fg(color))), area);
        return;
    }
    let keys: &[(&str, &str)] = if app.search_editing {
        &[("Enter", "искать"), ("Esc", "отмена"), ("Ctrl+U", "очистить")]
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
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in keys {
        spans.extend(key_hint(k, d));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

type HelpSection = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

fn draw_help(f: &mut Frame) {
    let i = ic();
    let sections: [HelpSection; 4] = [
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
            ],
        ),
        (
            i.keys,
            "Навигация",
            &[
                ("1-4  Tab", "вкладки"),
                ("j k  ↑ ↓", "перемещение"),
                ("g  G", "в начало / в конец"),
                ("Enter", "играть"),
                ("s", "перемешать список"),
                ("y", "копировать ссылку на трек"),
                ("/", "поиск"),
                ("R", "обновить"),
                ("q", "выход"),
            ],
        ),
    ];
    let mut lines = vec![];
    for (icon, title, rows) in sections {
        lines.push(Line::from(vec![
            Span::styled(format!("{icon}  "), Style::new().fg(ACCENT)),
            Span::styled(title.to_uppercase(), Style::new().fg(SUB).bold()),
        ]));
        for (k, d) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("   {k:<12}"), Style::new().fg(ACCENT).bold()),
                Span::styled(*d, Style::new().fg(Color::Reset)),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.pop();
    let area = f.area();
    let h = (lines.len() as u16 + 4).min(area.height);
    let [v] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(52.min(area.width))]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let block = panel(i.keys, "Горячие клавиши")
        .border_style(Style::new().fg(ACCENT))
        .title_bottom(Line::from(Span::styled(" любая клавиша — закрыть ", Style::new().fg(MUTED))).centered())
        .padding(Padding::uniform(1));
    f.render_widget(Paragraph::new(lines).block(block), r);
}

#[cfg(test)]
mod tests {
    use crate::api::{Account, Api, Track};
    use crate::app::{App, Tab};
    use ratatui::{backend::TestBackend, Terminal};

    /// Renders every tab with a real mpv instance. Run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn render_smoke() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let player = crate::player::Player::spawn(tx.clone(), 50).expect("mpv");
        let acc = Account { uid: "1".into(), login: "tester".into(), name: "Tester".into(), has_plus: true };
        let mut app = App::new(Api::new("x"), acc, tx, player, 50);
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
        app.show_help = true;
        let mut term = Terminal::new(TestBackend::new(100, 34)).unwrap();
        term.draw(|f| super::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
    }

    /// Downloads a real image and renders it as sixel and half blocks. Needs network.
    #[test]
    #[ignore]
    fn cover_render() {
        use ratatui_image::picker::{Picker, ProtocolType};
        let (tx, _rx) = std::sync::mpsc::channel();
        let player = crate::player::Player::spawn(tx.clone(), 50).expect("mpv");
        let acc = Account { uid: "1".into(), login: "t".into(), name: "T".into(), has_plus: true };
        let mut app = App::new(Api::new("x"), acc, tx, player, 50);
        let url = crate::api::cover_url("avatars.yandex.net/get-music-misc/34161/rotor-genre-pop-icon/%%");
        let img = app.api.fetch_image(&url).unwrap();
        for proto in [ProtocolType::Sixel, ProtocolType::Halfblocks, ProtocolType::Kitty] {
            let mut picker = Picker::from_fontsize((10, 20));
            picker.set_protocol_type(proto);
            app.covers = vec![(url.clone(), picker.new_resize_protocol(img.clone()))];
            app.picker = Some(picker);
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
