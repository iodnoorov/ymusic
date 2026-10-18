use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default = "default_volume")]
    pub volume: u8,
    /// Nerd Font icons (false — plain Unicode symbols).
    #[serde(default = "default_true")]
    pub icons: bool,
    /// Equalizer preset id (see `eq::PRESETS`).
    #[serde(default)]
    pub eq: String,
    /// Sound quality id (see `api::QUALITIES`).
    #[serde(default)]
    pub quality: String,
    /// Fade-in at the start and fade-out at the end of each track, seconds (0 — off).
    #[serde(default)]
    pub crossfade: u8,
}

/// Longest allowed fade-in/fade-out, seconds.
pub const MAX_CROSSFADE: u8 = 15;

fn default_true() -> bool {
    true
}

fn default_volume() -> u8 {
    70
}

impl Default for Config {
    fn default() -> Self {
        Config {
            token: None,
            volume: default_volume(),
            icons: true,
            eq: String::new(),
            quality: String::new(),
            crossfade: 0,
        }
    }
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
    use super::Config;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ymusic-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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

        let cfg = Config { token: Some("t".into()), volume: 120, ..Config::default() };
        cfg.save_to(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
        let back = Config::load_from(&link);
        assert_eq!((back.token.as_deref(), back.volume), (Some("t"), 120));
        assert!(!d.join("real.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
