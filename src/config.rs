use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User settings, stored at %APPDATA%\widgets\config.toml.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub clock_24h: bool,
    /// City for the weather widget, e.g. "Berlin" or "Austin".
    pub city: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { clock_24h: true, city: String::new() }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
        base.join("widgets").join("config.toml")
    }

    /// Always reads fresh from disk, so edits made in Notepad are picked up.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, text);
        }
    }

    /// Writes the file with all fields present, so it's easy to edit by hand.
    pub fn ensure_file() {
        Self::load().save();
    }
}
