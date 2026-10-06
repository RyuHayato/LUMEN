//! User-editable configuration, stored as TOML at
//! `%APPDATA%/LUMEN/config.toml`.
//!
//! App-internal state lives in the SQLite `settings` table instead — this
//! file is only for things a user may reasonably edit by hand.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::output::OutputMode;
use crate::error::ConfigError;
use crate::playback::RepeatMode;

pub const APP_DIR_NAME: &str = "LUMEN";
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const DB_FILE_NAME: &str = "lumen.db";
pub const ARTWORK_DIR_NAME: &str = "artwork";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Linear output gain, 0.0..=1.0.
    pub volume: f32,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    /// Opaque backend device id (`None` = system default device).
    pub output_device_id: Option<String>,
    /// Shared vs exclusive output (ADR-015).
    #[serde(default)]
    pub output_mode: OutputMode,
    /// Root folders the library scanner watches (Phase 1).
    pub library_folders: Vec<PathBuf>,
    /// Cross track boundaries without a gap (see `audio::pipeline`). Defaults to
    /// on; the switch exists so the behaviour can be A/B'd, not because off is
    /// the norm.
    #[serde(default = "default_true")]
    pub gapless: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            volume: 1.0,
            repeat: RepeatMode::Off,
            shuffle: false,
            output_device_id: None,
            output_mode: OutputMode::Shared,
            library_folders: Vec::new(),
            gapless: true,
        }
    }
}

impl Config {
    /// Clamp/repair values that are syntactically valid but out of range.
    pub fn sanitized(mut self) -> Self {
        self.volume = self.volume.clamp(0.0, 1.0);
        self
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        Ok(config.sanitized())
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;
        fs::write(path, text).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Root directory for all LUMEN application data.
///
/// Windows-first: uses `%APPDATA%`. A platform-path seam belongs here when
/// macOS/Linux work actually starts — not before.
pub fn app_data_dir() -> Result<PathBuf, ConfigError> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|p| p.join(APP_DIR_NAME))
        .ok_or(ConfigError::NoAppDataDir)
}

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    app_data_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

pub fn default_db_path() -> Result<PathBuf, ConfigError> {
    app_data_dir().map(|dir| dir.join(DB_FILE_NAME))
}

pub fn default_artwork_dir() -> Result<PathBuf, ConfigError> {
    app_data_dir().map(|dir| dir.join(ARTWORK_DIR_NAME))
}

/// Load the user config from the default location, or defaults if absent.
pub fn load_default() -> Result<Config, ConfigError> {
    Config::load(&default_config_path()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = format!(
            "lumen-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = temp_dir("missing");
        let path = dir.join("config.toml");
        let config = Config::load(&path).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("config.toml");

        let config = Config {
            volume: 0.42,
            repeat: RepeatMode::All,
            shuffle: true,
            library_folders: vec![PathBuf::from("D:\\Music")],
            ..Config::default()
        };

        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_file_is_a_parse_error() {
        let dir = temp_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "this is = = not toml").unwrap();

        match Config::load(&path) {
            Err(ConfigError::Parse { .. }) => {}
            other => panic!("expected parse error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_volume_is_sanitized() {
        let config = Config {
            volume: 7.5,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(config.volume, 1.0);
    }

    #[test]
    fn partial_file_falls_back_to_defaults_per_field() {
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "volume = 0.25\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.volume, 0.25);
        assert_eq!(config.repeat, RepeatMode::Off);

        let _ = fs::remove_dir_all(&dir);
    }
}
