use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

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
    pub fn load() -> Config {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(dir()).context("не удалось создать каталог конфигурации")?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path())?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}
