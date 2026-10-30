//! The hotkeys of the main screen in one table: what each key does (`App::act`), the help (`?`)
//! and the hints at the bottom all come from it, so they cannot tell different stories. Pop-up
//! windows and the search field have keys of their own.

use ratatui::crossterm::event::KeyCode;

/// What a hotkey does; some depend on the tab (see `App::act`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    Help,
    Settings,
    PlayPause,
    Next,
    Prev,
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    Mute,
    Repeat,
    LikeCurrent,
    LikeSelected,
    Dislike,
    MyWave,
    TrackWave,
    Artist,
    Album,
    Tab(usize),
    NextTab,
    PrevTab,
    Down,
    Up,
    PageDown,
    PageUp,
    Top,
    Bottom,
    Activate,
    Shuffle,
    CopyLink,
    OpenLink,
    Search,
    /// Back into the music or podcast search field.
    EditSearch,
    Reload,
    AddToPlaylist,
    /// The track from the open playlist, or the selected playlist.
    Remove,
    Rename,
    NewPlaylist,
    /// Out of the open playlist, podcast, or podcast description.
    Back,
    PodcastPreviousSection,
    PodcastNextSection,
    PodcastFromStart,
    PodcastDescription,
    PodcastFavorite,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Playback,
    Ratings,
    Waves,
    Navigation,
    Playlists,
    Podcasts,
}

impl Section {
    pub const ALL: [Section; 6] = [
        Section::Playback, Section::Ratings, Section::Waves, Section::Navigation, Section::Playlists, Section::Podcasts,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Playback => "Воспроизведение",
            Section::Ratings => "Оценки",
            Section::Waves => "Волны",
            Section::Navigation => "Навигация",
            Section::Playlists => "Плейлисты",
            Section::Podcasts => "Подкасты",
        }
    }
}

/// A row of the help and the keys behind it.
pub struct Row {
    pub section: Section,
    /// The keys as the help shows them, when shorter than all of them listed (`1-6  Tab`).
    pub label: Option<&'static str>,
    /// What the row says in the help; `None`: not shown there.
    pub help: Option<&'static str>,
    pub keys: &'static [(KeyCode, Action)],
}

use Action as A;
use KeyCode::Char as C;

const fn row(section: Section, help: &'static str, keys: &'static [(KeyCode, Action)]) -> Row {
    Row { section, label: None, help: Some(help), keys }
}

const fn labeled(section: Section, label: &'static str, help: &'static str, keys: &'static [(KeyCode, Action)]) -> Row {
    Row { section, label: Some(label), help: Some(help), keys }
}

pub const ROWS: &[Row] = &[
    row(Section::Playback, "пауза / продолжить", &[(C(' '), A::PlayPause), (C('p'), A::PlayPause)]),
    row(Section::Playback, "следующий / предыдущий", &[(C('n'), A::Next), (C('b'), A::Prev)]),
    row(Section::Playback, "перемотка ±10 с", &[(KeyCode::Left, A::SeekBack), (KeyCode::Right, A::SeekForward)]),
    labeled(
        Section::Playback,
        "+  -",
        "громкость",
        &[(C('+'), A::VolumeUp), (C('-'), A::VolumeDown), (C('='), A::VolumeUp), (C('_'), A::VolumeDown)],
    ),
    row(Section::Playback, "без звука", &[(C('m'), A::Mute)]),
    row(Section::Playback, "повтор: очередь / трек / выкл", &[(C('T'), A::Repeat)]),
    row(Section::Ratings, "лайк текущего трека / выпуска", &[(C('l'), A::LikeCurrent)]),
    row(Section::Ratings, "лайк выбранного трека / выпуска", &[(C('L'), A::LikeSelected)]),
    row(Section::Ratings, "дизлайк и пропуск", &[(C('d'), A::Dislike)]),
    row(Section::Waves, "Моя волна", &[(C('w'), A::MyWave)]),
    // The stations tab and Enter, both bound below.
    labeled(Section::Waves, "2  Enter", "станция: жанр, настроение…", &[]),
    row(Section::Waves, "волна по треку", &[(C('r'), A::TrackWave)]),
    row(Section::Waves, "все треки исполнителя", &[(C('A'), A::Artist)]),
    row(Section::Waves, "альбом трека", &[(C('e'), A::Album)]),
    labeled(
        Section::Navigation,
        "1-6  Tab",
        "вкладки",
        &[
            (C('1'), A::Tab(0)),
            (C('2'), A::Tab(1)),
            (C('3'), A::Tab(2)),
            (C('4'), A::Tab(3)),
            (C('5'), A::Tab(4)),
            (C('6'), A::Tab(5)),
            (KeyCode::Tab, A::NextTab),
            (KeyCode::BackTab, A::PrevTab),
        ],
    ),
    labeled(
        Section::Navigation,
        "j k  ↑ ↓",
        "перемещение",
        &[
            (C('j'), A::Down),
            (C('k'), A::Up),
            (KeyCode::Down, A::Down),
            (KeyCode::Up, A::Up),
            (KeyCode::PageDown, A::PageDown),
            (KeyCode::PageUp, A::PageUp),
        ],
    ),
    labeled(
        Section::Navigation,
        "g  G",
        "в начало / в конец",
        &[(C('g'), A::Top), (C('G'), A::Bottom), (KeyCode::Home, A::Top), (KeyCode::End, A::Bottom)],
    ),
    row(Section::Navigation, "играть", &[(KeyCode::Enter, A::Activate)]),
    row(Section::Navigation, "список в случайном порядке", &[(C('s'), A::Shuffle)]),
    row(Section::Navigation, "копировать ссылку на трек", &[(C('y'), A::CopyLink)]),
    row(Section::Navigation, "играть по ссылке", &[(C('Y'), A::OpenLink)]),
    labeled(Section::Navigation, "/", "поиск", &[(C('/'), A::Search), (C('i'), A::EditSearch)]),
    row(Section::Navigation, "обновить", &[(C('R'), A::Reload)]),
    row(Section::Navigation, "настройки, о программе", &[(C('o'), A::Settings)]),
    row(Section::Navigation, "выход", &[(C('q'), A::Quit)]),
    // The help itself says how it is opened.
    Row { section: Section::Navigation, label: None, help: None, keys: &[(C('?'), A::Help), (KeyCode::F(1), A::Help)] },
    row(Section::Playlists, "добавить трек в плейлист", &[(C('a'), A::AddToPlaylist)]),
    labeled(
        Section::Playlists,
        "Enter  Esc",
        "открыть / закрыть плейлист",
        &[(KeyCode::Esc, A::Back), (KeyCode::Backspace, A::Back)],
    ),
    row(Section::Playlists, "удалить трек / плейлист", &[(C('x'), A::Remove)]),
    row(Section::Playlists, "переименовать плейлист", &[(C('E'), A::Rename)]),
    row(Section::Playlists, "новый плейлист", &[(C('N'), A::NewPlaylist)]),
    row(
        Section::Podcasts,
        "предыдущий / следующий раздел",
        &[(C('['), A::PodcastPreviousSection), (C(']'), A::PodcastNextSection)],
    ),
    labeled(Section::Podcasts, "Enter  Esc", "открыть / играть / назад", &[]),
    row(Section::Podcasts, "эпизод с начала", &[(C('z'), A::PodcastFromStart)]),
    row(Section::Podcasts, "описание / закрыть описание", &[(C('I'), A::PodcastDescription)]),
    row(Section::Podcasts, "добавить / убрать подкаст из Моих", &[(C('f'), A::PodcastFavorite)]),
    labeled(Section::Podcasts, "↑ ↓  PgUp PgDn", "прокрутка описания", &[]),
    labeled(Section::Podcasts, "Home  End", "начало / конец описания", &[]),
];

/// What `code` does on the main screen.
pub fn action(code: KeyCode) -> Option<Action> {
    ROWS.iter().flat_map(|r| r.keys).find(|(k, _)| *k == code).map(|&(_, a)| a)
}

/// A key as the help writes it (`short`: as the hints at the bottom do).
fn key_name(code: KeyCode, short: bool) -> String {
    match code {
        KeyCode::Char(' ') if short => "␣".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other}"),
    }
}

/// The keys of a help row.
pub fn row_keys(row: &Row) -> String {
    match row.label {
        Some(label) => label.to_string(),
        None => row.keys.iter().map(|&(k, _)| key_name(k, false)).collect::<Vec<_>>().join("  "),
    }
}

/// The key of a hint: the first one bound to each of `actions`, run together for symbols (`←→`,
/// `+-`), with a slash between letters (`n/b`).
pub fn hint_keys(actions: &[Action]) -> String {
    let names: Vec<String> = actions
        .iter()
        .filter_map(|a| ROWS.iter().flat_map(|r| r.keys).find(|(_, b)| b == a))
        .map(|&(k, _)| key_name(k, true))
        .collect();
    let symbols = names.iter().all(|n| !n.chars().any(char::is_alphanumeric));
    names.join(if symbols { "" } else { "/" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        // No key does two things.
        let all: Vec<KeyCode> = ROWS.iter().flat_map(|r| r.keys).map(|&(k, _)| k).collect();
        for (i, k) in all.iter().enumerate() {
            assert!(!all[..i].contains(k), "{k} is bound twice");
        }
        assert_eq!(action(C(' ')), Some(A::PlayPause));
        assert_eq!(action(KeyCode::F(1)), Some(A::Help));
        assert_eq!(action(C('Z')), None);
        assert_eq!(row_keys(&ROWS[0]), "Space  p");
        assert_eq!(hint_keys(&[A::PlayPause]), "␣");
        assert_eq!(hint_keys(&[A::Next, A::Prev]), "n/b");
        assert_eq!(hint_keys(&[A::SeekBack, A::SeekForward]), "←→");
        assert_eq!(hint_keys(&[A::VolumeUp, A::VolumeDown]), "+-");
        assert_eq!(hint_keys(&[A::Back]), "Esc");
        assert_eq!(action(C('6')), Some(A::Tab(5)));
        assert_eq!(action(C('[')), Some(A::PodcastPreviousSection));
        assert_eq!(action(C(']')), Some(A::PodcastNextSection));
        assert_eq!(action(C('z')), Some(A::PodcastFromStart));
        assert_eq!(action(C('I')), Some(A::PodcastDescription));
        assert_eq!(action(C('f')), Some(A::PodcastFavorite));
        assert_eq!(hint_keys(&[A::PodcastPreviousSection, A::PodcastNextSection]), "[]");
        // Every row the help shows has keys behind it (or names keys bound elsewhere).
        for r in ROWS {
            assert!(!r.keys.is_empty() || r.label.is_some(), "{:?}", r.help);
        }
    }
}
