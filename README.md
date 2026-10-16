# ymusic-cli

**Яндекс Музыка в терминале.** Моя волна, лайки, станции по жанрам и настроениям, поиск — с понятными горячими клавишами.

<p align="center">
  <img src="docs/screenshots/wave.png" alt="Моя волна" width="900">
</p>

<table>
  <tr>
    <td align="center"><img src="docs/screenshots/stations.png" alt="Станции" width="290"><br><sub>Станции: жанры, настроения, занятия</sub></td>
    <td align="center"><img src="docs/screenshots/liked.png" alt="Мне нравится" width="290"><br><sub>Мне нравится</sub></td>
    <td align="center"><img src="docs/screenshots/search.png" alt="Поиск" width="290"><br><sub>Поиск</sub></td>
  </tr>
</table>

> Иконки — из [Nerd Fonts](https://www.nerdfonts.com) (например, JetBrainsMono Nerd Font: `pacman -S ttf-jetbrains-mono-nerd`).
> Без Nerd Font запускайте `ymusic --no-icons` или поставьте `"icons": false` в конфиге.

## Возможности

- **Моя волна** — персональный поток рекомендаций, как в приложении.
- **Лайки и дизлайки учитываются волной**: лайк/дизлайк сохраняется в аккаунт и отправляется
  волне как событие обратной связи (как и старт, пропуск и дослушивание трека), поэтому рекомендации подстраиваются
  прямо во время прослушивания. Дизлайкнутый трек пропускается и больше не попадётся.
- **Выбор волны**: Моя волна с настройками (Любимое / Незнакомое / Популярное, Бодрое / Весёлое / Спокойное / Грустное),
  жанры (рок, метал, русский рок, инди, электроника, рэп, джаз…), настроения, занятия (спорт, работа, сон…), эпохи.
  Список станций подгружается из вашего аккаунта.
- **Волна по треку** — `r` на любом треке запускает похожую музыку.
- **Мне нравится** — все лайкнутые треки, воспроизведение по порядку или вперемешку.
- **Плейлисты** — ваши плейлисты (включая загруженные треки), воспроизведение по порядку или вперемешку.
- **Поиск** треков.
- **Обложки альбомов** прямо в терминале: sixel / kitty / iTerm2-протоколы (foot, kitty, WezTerm, Ghostty, Konsole, iTerm2),
  в остальных терминалах — цветными блоками.
- **Вход через браузер** (токен подхватывается автоматически) или по токену.
- Горячие клавиши работают и на **русской раскладке** (`д` = `l`, `т` = `n` и т. д.).
- Воспроизведение через **mpv** (если стоит [mpv-mpris](https://github.com/hoyon/mpv-mpris) — работают медиаклавиши).

> Для полных треков нужна подписка **Яндекс Плюс**, без неё API отдаёт только 30-секундные фрагменты.

## Установка

### Arch Linux (AUR)

```bash
yay -S ymusic-cli
```

### Готовый бинарник

Статически собранный `ymusic` для Linux лежит в [релизах](https://github.com/iodnoorov/ymusic/releases/latest) —
работает на любом дистрибутиве, собирать из исходников не нужно. Для звука нужен установленный `mpv`.
Есть сборки для x86_64 и aarch64 (ARM64); сборка для aarch64 не проверялась на реальном ARM-устройстве.

```bash
V=0.6.0         # номер последнего релиза
A=$(uname -m)   # x86_64 или aarch64
curl -LO https://github.com/iodnoorov/ymusic/releases/download/v$V/ymusic-$V-$A-linux.tar.gz
curl -LO https://github.com/iodnoorov/ymusic/releases/download/v$V/ymusic-$V-$A-linux.tar.gz.sha256
sha256sum -c ymusic-$V-$A-linux.tar.gz.sha256
tar xzf ymusic-$V-$A-linux.tar.gz
install -Dm755 ymusic-$V-$A-linux/ymusic ~/.local/bin/ymusic
```

### Из исходников

Вручную из склонированного репозитория (собирает текущие исходники):

```bash
git clone https://github.com/iodnoorov/ymusic
cd ymusic/packaging/local
makepkg -si
```

После установки приложение запускается командой `ymusic` из любого терминала. Удалить: `sudo pacman -R ymusic-cli`.

### NixOS / Nix

Запуск без установки:

```bash
nix run github:iodnoorov/ymusic
```

В профиль:

```bash
nix profile install github:iodnoorov/ymusic
```

В конфигурации NixOS через flake:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    ymusic-cli = {
      url = "github:iodnoorov/ymusic";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, ymusic-cli, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./configuration.nix
        ({ pkgs, ... }: {
          environment.systemPackages = [ ymusic-cli.packages.${pkgs.stdenv.hostPlatform.system}.default ];
        })
      ];
    };
  };
}
```

Или через overlay: `nixpkgs.overlays = [ ymusic-cli.overlays.default ];` и затем `pkgs.ymusic-cli`
(подходит и для home-manager: `home.packages = [ pkgs.ymusic-cli ];`).

Без flakes: `nix-build` в корне репозитория или `pkgs.callPackage (fetchTarball "https://github.com/iodnoorov/ymusic/archive/main.tar.gz") { }`.

mpv подтягивается автоматически.

### Из исходников (Cargo)

Нужны Rust ≥ 1.80 и `mpv` в `PATH`.

```bash
cargo install --git https://github.com/iodnoorov/ymusic
# или
git clone https://github.com/iodnoorov/ymusic && cd ymusic
cargo build --release && ./target/release/ymusic
```

## Вход

```bash
ymusic login
```

Появится меню:

1. **Через браузер** — откроется отдельное окно браузера со страницей входа Яндекса. Войдите как обычно
   (пароль, QR, Яндекс ID) — токен подхватится автоматически, окно закроется само.
   Поддерживаются Firefox, Zen, LibreWolf, Floorp, Chromium, Chrome, Brave, Vivaldi, Edge, Яндекс Браузер.
   Используется браузер по умолчанию; другой можно указать: `YMUSIC_BROWSER=chromium ymusic login`.
   Окно запускается с временным чистым профилем, ваш основной профиль не затрагивается.
2. **По токену** — вставить OAuth-токен (или целиком URL с `access_token=`).

Без меню:

```bash
ymusic login --browser              # сразу через браузер
ymusic login --token y0_AgAAAA...   # готовый токен
YMUSIC_TOKEN=y0_... ymusic          # токен из переменной окружения (не сохраняется)
ymusic whoami                       # проверить аккаунт
ymusic logout                       # удалить токен
```

<details>
<summary>Как получить токен вручную</summary>

Откройте <https://oauth.yandex.ru/authorize?response_type=token&client_id=23cabbbdc6cd418abb4b39c32c41195d>,
войдите — браузер перейдёт на `https://music.yandex.ru/#access_token=...&token_type=bearer...`.
Токен — всё между `access_token=` и `&`.
</details>

Токен хранится в `~/.config/ymusic/config.json` (права `0600`). Если запустить `ymusic` без входа — меню входа появится автоматически.

## Использование

```bash
ymusic                       # открыть интерфейс
ymusic --wave                # сразу включить Мою волну
ymusic --station genre:metal # сразу включить станцию (genre:rock, mood:calm, activity:workout, epoch:nineties…)
ymusic --no-icons            # без иконок Nerd Font
```

### Горячие клавиши

| Клавиша            | Действие                                              |
|--------------------|-------------------------------------------------------|
| `Space` / `p`      | пауза / продолжить                                    |
| `n` / `b`          | следующий / предыдущий трек                           |
| `←` / `→`          | перемотка на 10 секунд                                |
| `+` / `-`          | громкость                                             |
| `m`                | без звука                                             |
| `l`                | **лайк** текущего трека (повторно — убрать лайк)      |
| `L`                | лайк трека, выбранного в списке                       |
| `d`                | **дизлайк**: пропустить и больше не показывать        |
| `w`                | запустить **Мою волну**                               |
| `r`                | волна по выбранному (или текущему) треку              |
| `A`                | все треки исполнителя выбранного (или текущего) трека |
| `s`                | перемешать и играть список (лайки / поиск / плейлист) |
| `y`                | скопировать в буфер обмена ссылку на выбранный трек   |
| `1` … `5`          | вкладки: волна, станции, лайки, поиск, плейлисты      |
| `Tab` / `Shift+Tab`| следующая / предыдущая вкладка                        |
| `j`/`k`, `↑`/`↓`   | перемещение по списку; `g`/`G` — начало/конец         |
| `Enter`            | играть трек / запустить станцию / играть плейлист     |
| `/`                | поиск (`Enter` — искать, `Esc` — выйти из ввода)      |
| `R`                | обновить лайки, плейлисты и список станций            |
| `?`                | справка                                               |
| `q`                | выход                                                 |

## Как это устроено

- Rust + [ratatui](https://ratatui.rs) для интерфейса, [mpv](https://mpv.io) через JSON IPC для звука.
- Неофициальный API `api.music.yandex.net` (тот же, что у мобильного приложения).
  Моя волна работает через rotor-сессии (`/rotor/session/*`) с откатом на старый API станций.
  Клиент отправляет волне события `radioStarted`, `trackStarted`, `trackFinished`, `skip`, `like`, `dislike` —
  именно по ним Яндекс подстраивает рекомендации.

## Отказ от ответственности

Проект не связан с ООО «Яндекс». Используется неофициальный API; он может измениться в любой момент.
Используйте со своим аккаунтом и подпиской в соответствии с условиями сервиса.

## Отличия от первоначальной версии

Форк [Rexilone/yamusic-cli](https://github.com/Rexilone/yamusic-cli). Добавлено:

- `y` — копирование в буфер обмена ссылки на выбранный (или текущий) трек.
- `A` — плейлист из всех треков исполнителя выбранного (или текущего) трека.
- Вкладка «Плейлисты» (`5`) — плейлисты пользователя, `Enter` — играть, `s` — вперемешку.
- Проект перенесён в репозиторий `iodnoorov/ymusic`, ссылки и пакеты AUR указывают на него.
- Программа переименована в `ymusic`, пакет — в `ymusic-cli`. Настройки — в `~/.config/ymusic/` (старый `~/.config/yamusic-cli/` переносится автоматически), переменные окружения — `YMUSIC_*` (старые `YAMUSIC_*` тоже работают).
- Пакет `ymusic-cli` в AUR и готовые статические бинарники (x86_64, aarch64) в релизах GitHub.

## Дополнительная информация

- Проект доработан с помощью [Claude Code](https://claude.com/claude-code).
