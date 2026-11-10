//! Saved connection profiles, stored as TOML. Never holds credentials.
//!
//! Owner: connections. Contract only; bodies are stubs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Profile;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("no profile named {0}")]
    NotFound(String),
    #[error("a profile named {0} already exists")]
    Exists(String),
    #[error("could not find a config directory for this user")]
    NoConfigDir,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

impl Config {
    /// `<config dir>/redis-lens/config.toml`, e.g. `~/Library/Application Support/redis-lens/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        todo!("Config::default_path")
    }

    /// A missing file is an empty config, not an error.
    pub fn load(_path: &Path) -> Result<Self, ConfigError> {
        todo!("Config::load")
    }

    /// Writes atomically (temp file and rename), creating the directory if needed.
    pub fn save(&self, _path: &Path) -> Result<(), ConfigError> {
        todo!("Config::save")
    }

    pub fn get(&self, _name: &str) -> Option<&Profile> {
        todo!("Config::get")
    }

    /// Fails with [`ConfigError::Exists`] unless `replace` is true.
    pub fn add(&mut self, _profile: Profile, _replace: bool) -> Result<(), ConfigError> {
        todo!("Config::add")
    }

    pub fn remove(&mut self, _name: &str) -> Result<Profile, ConfigError> {
        todo!("Config::remove")
    }
}
