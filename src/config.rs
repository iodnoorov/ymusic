use crate::api::{quality_index, Quality, QUALITIES};
use crate::app::{Art, Output};
use crate::eq::{index as eq_index, Preset, PRESETS};
use crate::queue::Repeat;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// No `Debug`: printing the config would print the token.
#[derive(Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub settings: Settings,
    /// Older configs: `false` — no cover; read once into `art` and not written back.
    #[serde(default = "default_true", skip_serializing)]
    pub covers: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config { token: None, settings: Settings::default(), covers: true }
    }
}

/// What the program remembers between runs, as the settings window and the keys change it.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub volume: u8,
    /// Nerd Font icons (false — plain Unicode symbols).
    pub icons: bool,
    /// What «Сейчас играет» shows above the track; not set in older configs (see `Config::covers`).
    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub art: Option<Art>,
    /// Equalizer preset, stored by its id.
    #[serde(serialize_with = "by_id", deserialize_with = "preset")]
    pub eq: &'static Preset,
    /// Sound quality, stored by its id.
    #[serde(serialize_with = "by_id", deserialize_with = "quality")]
    pub quality: &'static Quality,
    /// Crossfade: the end of a track overlaps the start of the next one, seconds (0 — off).
    pub crossfade: u8,
    /// A new list starts after the current track instead of cutting it off.
    pub finish_track: bool,
    #[serde(deserialize_with = "lenient")]
    pub repeat: Repeat,
    /// Requests go to the API of the music.yandex.ru website instead of the mobile one.
    pub web_api: bool,
    #[serde(deserialize_with = "lenient")]
    pub output: Output,
    /// The header shows no login and no Plus badge.
    pub hide_account: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            volume: 70,
            icons: true,
            art: None,
            eq: &PRESETS[0],
            quality: &QUALITIES[quality_index("")],
            crossfade: 0,
            finish_track: false,
            repeat: Repeat::Off,
            web_api: false,
            output: Output::Auto,
            hide_account: false,
        }
    }
}

/// Something stored by its id in the config.
trait Id {
    fn id(&self) -> &'static str;
}

impl Id for &'static Preset {
    fn id(&self) -> &'static str {
        self.id
    }
}

impl Id for &'static Quality {
    fn id(&self) -> &'static str {
        self.id
    }
}

fn by_id<S: Serializer>(v: &impl Id, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v.id())
}

/// The id as a string; anything else is no id (and gets the default).
fn id<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(serde_json::Value::deserialize(d)?.as_str().unwrap_or_default().to_string())
}

/// An unknown preset is the flat one.
fn preset<'de, D: Deserializer<'de>>(d: D) -> Result<&'static Preset, D::Error> {
    Ok(&PRESETS[eq_index(&id(d)?)])
}

/// An unknown quality is the normal one.
fn quality<'de, D: Deserializer<'de>>(d: D) -> Result<&'static Quality, D::Error> {
    Ok(&QUALITIES[quality_index(&id(d)?)])
}

/// An unknown or broken value is the default instead of an error: one stale setting must not throw
/// the whole config (and the token in it) away.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    Ok(T::deserialize(serde_json::Value::deserialize(d)?).unwrap_or_default())
}

/// Longest allowed crossfade, seconds.
pub const MAX_CROSSFADE: u8 = 15;

fn default_true() -> bool {
    true
}

/// Environment variable `YMUSIC_<name>`.
pub fn env(name: &str) -> Option<OsString> {
    std::env::var_os(format!("YMUSIC_{name}"))
}

/// Like [`env`], as a string (`None` if unset or not valid UTF-8).
pub fn env_str(name: &str) -> Option<String> {
    env(name).and_then(|v| v.into_string().ok())
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn dir() -> PathBuf {
    config_home().join("ymusic")
}

pub fn path() -> PathBuf {
    dir().join("config.json")
}

impl Config {
    /// A config that fails to parse is moved to `config.json.bak`, so saving the defaults
    /// on exit does not destroy the token and settings in it.
    pub fn load() -> Config {
        Self::load_from(&path())
    }

    fn load_from(path: &Path) -> Config {
        let Ok(s) = std::fs::read_to_string(path) else { return Config::default() };
        match serde_json::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => {
                let bak = path.with_extension("json.bak");
                eprintln!("ymusic: не удалось прочитать {}: {e}", path.display());
                match std::fs::rename(path, &bak) {
                    Ok(()) => eprintln!("ymusic: файл сохранён как {}, используются настройки по умолчанию", bak.display()),
                    Err(e) => eprintln!("ymusic: не удалось переименовать его в {}: {e}", bak.display()),
                }
                Config::default()
            }
        }
    }

    /// Writes a temporary file and renames it over the config: a crash mid-write
    /// never leaves a truncated config, and the result always has mode 0600.
    /// A symlinked config (dotfiles) is written through: its target is replaced, not the link.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(dir()).context("не удалось создать каталог конфигурации")?;
        self.save_to(&path())
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let tmp = target.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Settings};
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ymusic-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The file keeps its format: an older config is read as it is, unknown values are defaults
    /// (not a broken config), and the settings are written under the same names.
    #[test]
    fn settings_format() {
        use crate::app::Art;
        use crate::queue::Repeat;
        let cfg: Config = serde_json::from_str(
            r#"{"token": "t", "volume": 40, "art": "spectrum", "eq": "laptop", "quality": "lq", "repeat": "one",
                "crossfade": 5, "finish_track": true, "web_api": true, "icons": false, "output": "pulse",
                "hide_account": true}"#,
        )
        .unwrap();
        let s = &cfg.settings;
        assert_eq!((s.volume, s.art, s.eq.id, s.quality.id, s.repeat), (40, Some(Art::Spectrum), "laptop", "lq", Repeat::One));
        assert!(s.finish_track && s.web_api && !s.icons && s.crossfade == 5 && s.hide_account);
        assert_eq!(s.output, crate::app::Output::Pulse);
        let back: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back["art"], "spectrum");
        assert_eq!(back["eq"], "laptop");
        assert_eq!(back["quality"], "lq");
        assert_eq!(back["repeat"], "one");
        assert_eq!(back["output"], "pulse");

        // Stale or empty values, as older versions wrote them: defaults, the token stays.
        let cfg: Config =
            serde_json::from_str(r#"{"token": "t", "art": "", "eq": "gone", "quality": 5, "repeat": "", "output": "jack", "covers": false}"#).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("t"));
        assert!(cfg.settings == Settings::default());
        assert!(!cfg.covers, "the old switch is still read");
        assert!(!serde_json::to_string(&cfg).unwrap().contains("covers"), "and not written back");
        // A missing file: covers are on.
        assert!(Config::default().covers);
    }

    #[test]
    fn broken_config_is_kept() {
        let d = temp_dir("broken");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"token": "secret", "volume": 50,}"#).unwrap();
        let cfg = Config::load_from(&p);
        assert_eq!(cfg.token, None);
        cfg.save_to(&p).unwrap();
        let bak = std::fs::read_to_string(d.join("config.json.bak")).unwrap();
        assert!(bak.contains("secret"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_is_private_and_follows_symlinks() {
        let d = temp_dir("save");
        let real = d.join("real.json");
        std::fs::write(&real, "{}").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = d.join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let cfg = Config { token: Some("t".into()), settings: Settings { volume: 120, ..Settings::default() }, ..Config::default() };
        cfg.save_to(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
        let back = Config::load_from(&link);
        assert_eq!((back.token.as_deref(), back.settings.volume), (Some("t"), 120));
        assert!(!d.join("real.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
